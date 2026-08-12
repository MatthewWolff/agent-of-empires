//! System memory + agent-count sampling for the `aoe.diagnostics` worker plugin.
//!
//! It answers one question: how close is the machine to a memory thrash, and
//! how many agents/processes are driving it. The headline signal is memory
//! headroom (`1 - available/total`), because on a box with a RAM-backed tmpfs
//! the unreclaimable `Shmem` collapses `MemAvailable` while kernel
//! pressure-stall counters can still read zero; `MemAvailable` already excludes
//! tmpfs/shmem, so it is the number that actually moves. PSI is carried as a
//! secondary escalation signal, not the headline.
//!
//! PSI is Linux only, so it is `None` on macOS, where the native pressure
//! level stands in for the color band instead.

use crate::session::Instance;

/// One memory reading. Byte fields are absolute bytes. `total_bytes == 0` means
/// the RAM figures could not be read this sample (both are read together, so a
/// half-read never fabricates a false 100%); callers treat that as unavailable.
/// The `Option` pressure fields are `None` where the platform does not expose
/// them, so a Linux-only signal never reads as a false all-clear on macOS.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemorySample {
    /// Total physical RAM in bytes (`MemTotal`). 0 when unknown this sample.
    pub total_bytes: u64,
    /// Kernel estimate of RAM available for new work without swapping
    /// (`MemAvailable`). Already excludes unreclaimable tmpfs/shmem, so this
    /// is the honest "distance to the wall" figure and the plotted line, and
    /// it already moves when tmpfs or zram grows.
    pub available_bytes: u64,
    /// Memory pressure stall, `some` share over the last 10s (percent 0..=100)
    /// from `/proc/pressure/memory`. Linux only.
    pub psi_mem_some_avg10: Option<f32>,
    /// I/O pressure stall, `some` share over the last 10s (percent 0..=100)
    /// from `/proc/pressure/io`. Carried because a fast thrash can raise io
    /// pressure ahead of memory pressure. Linux only.
    pub psi_io_some_avg10: Option<f32>,
    /// macOS native pressure level (`kern.memorystatus_vm_pressure_level`):
    /// 1 = normal, 2 = warn, 4 = critical. macOS only; `None` on Linux, which
    /// derives its band from headroom + PSI instead.
    pub macos_pressure_level: Option<u8>,
}

impl MemorySample {
    /// Bytes in use, derived as `total - available` so the plotted line, the
    /// percentage, and the used/total readout can never disagree.
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    /// Fraction of RAM used, `0.0..=1.0`. This is the plotted headroom line.
    /// Returns 0.0 when the RAM figures are unknown this sample (`total == 0`).
    pub fn used_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.used_bytes() as f64 / self.total_bytes as f64
    }
}

/// Count of agents and their processes, both scoped to AoE-managed sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AgentCounts {
    /// Live agent sessions (one per instance whose agent process is alive).
    pub agents: usize,
    /// Total processes across those agents' process trees.
    pub procs: usize,
}

/// Sample system memory for the current platform. Cheap: on Linux a handful of
/// `/proc` + `/sys` reads, so the worker's sampling loop can call it directly.
pub(crate) fn sample_memory() -> MemorySample {
    #[cfg(target_os = "linux")]
    {
        super::linux::sample_memory()
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::sample_memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        MemorySample::default()
    }
}

/// Count live AoE-managed agents and the total processes in their trees.
///
/// `agents` reuses the same batched liveness check the recovery paths use
/// (one process-table walk, scoped to AoE via the injected instance-id env
/// marker). `procs` counts every process across the live agents' trees in a
/// single further walk (see [`crate::process::count_pids_in_trees`]). ACP
/// agents that run detached with no tmux pane are folded in under the `serve`
/// feature, where the worker registry exists.
pub(crate) fn count_running_agents(insts: &[Instance]) -> AgentCounts {
    let alive = crate::session::recovery::orphaned_agents_alive(insts);
    let mut roots: Vec<u32> = insts
        .iter()
        .zip(alive.iter())
        .filter(|(_, &is_alive)| is_alive)
        .filter_map(|(inst, _)| agent_root_pid(inst))
        .collect();

    let acp_roots = live_acp_agent_pids();
    let agents = alive.iter().filter(|&&a| a).count() + acp_roots.len();
    roots.extend(acp_roots);

    AgentCounts {
        agents,
        procs: crate::process::count_pids_in_trees(&roots),
    }
}

/// Fraction used at or above which memory reads Critical.
pub(crate) const HEADROOM_CRITICAL: f64 = 0.90;
/// Fraction used at or above which memory reads Warn.
pub(crate) const HEADROOM_WARN: f64 = 0.70;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Critical.
const PSI_CRITICAL: f32 = 20.0;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Warn.
const PSI_WARN: f32 = 5.0;

/// Memory-pressure severity, worst-of across the available signals. Ordered
/// ascending so the derived `Ord` lets callers fold inputs with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PressureBand {
    Ok,
    Warn,
    Critical,
}

/// Classify a memory sample into a pressure band. Headroom is always an input;
/// PSI (Linux) and the macOS pressure level contribute only when present, so a
/// platform that omits a signal never has it read as a false all-clear. The
/// worst band across the present inputs wins.
pub(crate) fn pressure_band(mem: &MemorySample) -> PressureBand {
    let mut band = band_from_headroom(mem.used_fraction());
    if let Some(v) = mem.psi_mem_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(v) = mem.psi_io_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(level) = mem.macos_pressure_level {
        band = band.max(band_from_macos(level));
    }
    band
}

fn band_from_headroom(used_fraction: f64) -> PressureBand {
    if used_fraction >= HEADROOM_CRITICAL {
        PressureBand::Critical
    } else if used_fraction >= HEADROOM_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_psi(some_avg10: f32) -> PressureBand {
    if some_avg10 >= PSI_CRITICAL {
        PressureBand::Critical
    } else if some_avg10 >= PSI_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_macos(level: u8) -> PressureBand {
    match level {
        4 => PressureBand::Critical,
        2 => PressureBand::Warn,
        _ => PressureBand::Ok,
    }
}

/// The root PID of an instance's agent process, via the tmux pane PID that
/// `aoe` already uses elsewhere. Resolves the session's live name (a
/// smart-renamed session keeps its original tmux name), so the lookup does not
/// go stale when the title moves. `None` when the session has no live pane.
fn agent_root_pid(inst: &Instance) -> Option<u32> {
    let session_name = crate::tmux::Session::resolve_name(&inst.id, &inst.title);
    crate::process::get_pane_pid(&session_name)
}

/// Root PIDs of detached ACP agents (structured-view workers with no tmux
/// pane). Empty without the `serve` feature, where the worker registry that
/// tracks them does not exist.
fn live_acp_agent_pids() -> Vec<u32> {
    #[cfg(feature = "serve")]
    {
        crate::process::worker_registry::list()
            .unwrap_or_default()
            .into_iter()
            .filter(crate::process::worker_registry::is_record_live)
            .map(|rec| rec.pid)
            .collect()
    }
    #[cfg(not(feature = "serve"))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_with_fraction(used_fraction: f64) -> MemorySample {
        // total 1000 so available maps cleanly to the target fraction.
        MemorySample {
            total_bytes: 1000,
            available_bytes: (1000.0 * (1.0 - used_fraction)).round() as u64,
            ..MemorySample::default()
        }
    }

    #[test]
    fn pressure_band_from_headroom_only() {
        let cases = [
            (0.0, PressureBand::Ok),
            (0.69, PressureBand::Ok),
            (0.70, PressureBand::Warn),
            (0.89, PressureBand::Warn),
            (0.90, PressureBand::Critical),
            (0.99, PressureBand::Critical),
        ];
        for (frac, expected) in cases {
            assert_eq!(
                pressure_band(&sample_with_fraction(frac)),
                expected,
                "headroom fraction {frac}"
            );
        }
    }

    #[test]
    fn pressure_band_escalates_on_psi_when_headroom_calm() {
        // Headroom alone is Ok (50% used); PSI drives the band up.
        let mut mem = sample_with_fraction(0.50);
        assert_eq!(pressure_band(&mem), PressureBand::Ok);

        mem.psi_mem_some_avg10 = Some(6.0);
        assert_eq!(pressure_band(&mem), PressureBand::Warn);

        mem.psi_mem_some_avg10 = Some(25.0);
        assert_eq!(pressure_band(&mem), PressureBand::Critical);

        // io pressure alone (mem PSI absent) still escalates.
        let mem_io = MemorySample {
            psi_io_some_avg10: Some(25.0),
            ..sample_with_fraction(0.50)
        };
        assert_eq!(pressure_band(&mem_io), PressureBand::Critical);
    }

    #[test]
    fn pressure_band_from_macos_level() {
        let cases = [
            (1u8, PressureBand::Ok),
            (2, PressureBand::Warn),
            (4, PressureBand::Critical),
        ];
        for (level, expected) in cases {
            let mem = MemorySample {
                macos_pressure_level: Some(level),
                ..sample_with_fraction(0.50)
            };
            assert_eq!(pressure_band(&mem), expected, "macos level {level}");
        }
    }

    #[test]
    fn pressure_band_takes_worst_of_inputs() {
        // Critical headroom must not be softened by a calm PSI reading.
        let mem = MemorySample {
            psi_mem_some_avg10: Some(1.0),
            ..sample_with_fraction(0.95)
        };
        assert_eq!(pressure_band(&mem), PressureBand::Critical);
    }

    #[test]
    fn pressure_band_default_sample_is_ok() {
        assert_eq!(pressure_band(&MemorySample::default()), PressureBand::Ok);
    }
}
