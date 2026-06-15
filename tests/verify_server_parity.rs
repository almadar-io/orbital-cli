//! `orb verify --server` — server↔client render parity over real HTTP.
//!
//! Boots `orbital-server` in-process, drives the client via `SyncServerBridge`, and runs the
//! same L2 render contract over the wire. Asserts the parity pass is actually executed (the
//! `[server]` line) and the run succeeds — so the emitted server's routes + the client render
//! agree with the in-process kernel path.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std-modal.orb")
}

#[test]
fn verify_server_runs_http_parity_pass() {
    let output = Command::new(orb_bin())
        .args(["verify", fixture().to_str().unwrap(), "--server"])
        .output()
        .expect("spawn orb verify --server");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "orb verify --server failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The parity pass must actually run (and pass) — not silently skip.
    assert!(
        stdout.contains("[server]"),
        "no server-parity pass in output (the [server] L2 line is missing):\n{stdout}"
    );
    assert!(
        !stdout.contains("L2/server FAIL"),
        "server-parity pass reported failures:\n{stdout}"
    );
    assert!(stdout.contains("passed"), "verify did not report a pass:\n{stdout}");
}
