//! The built-in diagnostics worker (`aoe __plugin-diagnostics`).
//!
//! A first-party worker plugin that samples system memory and the AoE-managed
//! agent/process counts on a timer and pushes them to the global `home-pane`
//! slot as a memory-over-time sparkline plus a two-line count readout. It runs
//! as `aoe` re-invoked through the [`crate::plugin::launch::RuntimeSpec::SelfExec`]
//! runtime, so it needs no capability beyond `runtime.worker`: the memory read
//! and the count are ordinary in-process calls, not host RPCs.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{json, Value};

use crate::process::metrics::{
    count_running_agents, pressure_band, sample_memory, AgentCounts, MemorySample, PressureBand,
    HEADROOM_CRITICAL, HEADROOM_WARN,
};
use crate::session::Storage;

/// Samples retained for the plot. At one sample per second this is the last
/// two minutes, enough to see a climb build without an unbounded buffer.
const HISTORY_LEN: usize = 120;
/// Seconds between samples. Matches the built-in strip's cadence.
const REFRESH_SECS: u64 = 1;

/// Run the worker loop until the host closes the pipe. `profile` is the
/// daemon's session-storage profile (empty resolves to the default), used to
/// scope the agent count to the sessions the daemon manages.
pub fn run(profile: String) -> anyhow::Result<()> {
    // Drain host->worker traffic so a closed or unread stdin never stalls us;
    // this worker only pushes and has no requests to answer.
    std::thread::spawn(drain_stdin);

    let mut history: VecDeque<f64> = VecDeque::with_capacity(HISTORY_LEN);
    let mut out = std::io::stdout();
    // The storage handle is stable across ticks; open it once (retrying while
    // it can't be resolved, e.g. a profile that appears after startup) so only
    // the per-tick `load()` re-reads sessions.json.
    let mut storage: Option<Storage> = None;
    loop {
        let mem = sample_memory();
        let counts = sample_counts(&mut storage, &profile);
        if history.len() == HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(mem.used_fraction());

        let payload = home_pane_payload(history.make_contiguous(), &mem, &counts);
        let line = ui_state_set_line("home-pane", "memory", payload);
        // A write/flush error means the host closed the pipe; exit and let the
        // supervisor decide whether to respawn.
        if writeln!(out, "{line}").is_err() || out.flush().is_err() {
            break;
        }
        std::thread::sleep(Duration::from_secs(REFRESH_SECS));
    }
    Ok(())
}

fn drain_stdin() {
    let mut stdin = std::io::stdin().lock();
    let mut line = String::new();
    while matches!(stdin.read_line(&mut line), Ok(n) if n > 0) {
        line.clear();
    }
}

/// Count agents/procs for `profile`, degrading to zero counts (not an error)
/// when storage cannot be read: a diagnostics tick should never crash the
/// worker over a transient read. Opens `storage` lazily and reuses the handle;
/// only `load()` (the `sessions.json` read) repeats each tick.
fn sample_counts(storage: &mut Option<Storage>, profile: &str) -> AgentCounts {
    if storage.is_none() {
        *storage = Storage::open_unwatched(profile).ok();
    }
    storage
        .as_ref()
        .and_then(|s| s.load().ok())
        .map(|instances| count_running_agents(&instances))
        .unwrap_or_default()
}

/// Build the `home-pane` payload: a memory-headroom sparkline banded at the
/// same warn/critical thresholds the pressure classification uses, plus one
/// row each for the live agent and process counts.
fn home_pane_payload(history: &[f64], mem: &MemorySample, counts: &AgentCounts) -> Value {
    let caption = if mem.total_bytes == 0 {
        "memory unavailable".to_string()
    } else {
        format!(
            "{}% RAM in use",
            (mem.used_fraction() * 100.0).round() as u32
        )
    };
    json!({
        "title": "System",
        "blocks": [
            {
                "kind": "sparkline",
                "values": history,
                "max": 1.0,
                "tone": tone_for(pressure_band(mem)),
                "bands": [
                    {"at": HEADROOM_WARN, "tone": "warn"},
                    {"at": HEADROOM_CRITICAL, "tone": "danger"},
                ],
                "caption": caption,
            },
            {"kind": "row", "label": "agents", "value": counts.agents.to_string()},
            {"kind": "row", "label": "processes", "value": counts.procs.to_string()},
        ],
    })
}

fn tone_for(band: PressureBand) -> &'static str {
    match band {
        PressureBand::Ok => "success",
        PressureBand::Warn => "warn",
        PressureBand::Critical => "danger",
    }
}

/// A `ui.state.set` JSON-RPC notification line (no `id`: the host runs it and
/// sends nothing back).
fn ui_state_set_line(slot: &str, id: &str, payload: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "ui.state.set",
        "params": {"slot": slot, "id": id, "payload": payload},
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_at(used_fraction: f64) -> MemorySample {
        MemorySample {
            total_bytes: 1000,
            available_bytes: (1000.0 * (1.0 - used_fraction)).round() as u64,
            ..MemorySample::default()
        }
    }

    #[test]
    fn payload_carries_sparkline_history_and_count_rows() {
        let counts = AgentCounts {
            agents: 3,
            procs: 24,
        };
        let payload = home_pane_payload(&[0.1, 0.2, 0.3], &mem_at(0.3), &counts);
        let blocks = payload["blocks"].as_array().unwrap();

        assert_eq!(blocks[0]["kind"], "sparkline");
        assert_eq!(blocks[0]["values"], json!([0.1, 0.2, 0.3]));
        assert_eq!(blocks[0]["max"], 1.0);
        // Banded at the shared warn/critical headroom thresholds.
        assert_eq!(blocks[0]["bands"][0]["at"], HEADROOM_WARN);
        assert_eq!(blocks[0]["bands"][1]["at"], HEADROOM_CRITICAL);
        assert_eq!(blocks[0]["caption"], "30% RAM in use");

        assert_eq!(
            blocks[1],
            json!({"kind": "row", "label": "agents", "value": "3"})
        );
        assert_eq!(
            blocks[2],
            json!({"kind": "row", "label": "processes", "value": "24"})
        );
    }

    #[test]
    fn sparkline_tone_tracks_pressure() {
        let cases = [(0.30, "success"), (0.75, "warn"), (0.95, "danger")];
        for (frac, expected) in cases {
            let payload = home_pane_payload(&[frac], &mem_at(frac), &AgentCounts::default());
            assert_eq!(payload["blocks"][0]["tone"], expected, "fraction {frac}");
        }
    }

    #[test]
    fn caption_reports_unavailable_when_ram_unknown() {
        let payload = home_pane_payload(&[0.0], &MemorySample::default(), &AgentCounts::default());
        assert_eq!(payload["blocks"][0]["caption"], "memory unavailable");
    }

    #[test]
    fn emits_a_ui_state_set_notification_without_an_id() {
        let line = ui_state_set_line("home-pane", "memory", json!({"title": "System"}));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "ui.state.set");
        assert!(parsed.get("id").is_none(), "notification carries no id");
        assert_eq!(parsed["params"]["slot"], "home-pane");
        assert_eq!(parsed["params"]["id"], "memory");
    }
}
