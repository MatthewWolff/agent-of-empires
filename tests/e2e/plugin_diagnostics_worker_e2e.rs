//! Full-binary e2e for the built-in diagnostics worker.
//!
//! Runs the real `aoe __plugin-diagnostics` subcommand (the self-exec worker
//! the plugin host would spawn) as a subprocess and asserts it speaks the
//! plugin protocol: within a few seconds it writes a `ui.state.set`
//! notification pushing a `home-pane` sparkline plus the agent/process count
//! rows. This covers the CLI dispatch and the sampling loop end to end; the
//! payload shape itself is unit-tested in `src/plugin/diagnostics.rs`.
//!
//! Serve-gated because the subcommand only exists under `feature = "serve"`.
//! Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- plugin_diagnostics_worker
//! ```
#![cfg(feature = "serve")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use serial_test::parallel;

#[test]
#[parallel]
fn plugin_diagnostics_worker_emits_home_pane_sparkline() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .arg("__plugin-diagnostics")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env_remove("AGENT_OF_EMPIRES_PROFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aoe __plugin-diagnostics");

    // Read stdout on a thread and forward the first line that parses as the
    // worker's ui.state.set push, so a stray startup line never fails the test.
    let stdout = child.stdout.take().expect("worker stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                if msg["method"] == "ui.state.set" {
                    let _ = tx.send(msg);
                    return;
                }
            }
        }
    });

    // Generous margin: the worker emits its first sample within a second, but a
    // CI box under post-build load can be slow to schedule the fresh process.
    let msg = rx.recv_timeout(Duration::from_secs(30));
    let _ = child.kill();
    let _ = child.wait();
    let msg = msg.expect("worker emitted a ui.state.set push within 30s");

    assert_eq!(msg["params"]["slot"], "home-pane");
    assert_eq!(msg["params"]["id"], "memory");
    let blocks = msg["params"]["payload"]["blocks"]
        .as_array()
        .expect("payload carries blocks");
    assert_eq!(blocks[0]["kind"], "sparkline");
    assert!(
        blocks.iter().any(|b| b["label"] == "agents"),
        "an agents count row is present: {blocks:?}"
    );
    assert!(
        blocks.iter().any(|b| b["label"] == "processes"),
        "a processes count row is present: {blocks:?}"
    );
}
