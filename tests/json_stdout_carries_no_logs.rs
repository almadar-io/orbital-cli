//! `--json` stdout is the data channel: log lines go to stderr. Logged to stdout, any
//! ERROR/debug line made a passing `orbital verify --json` unparseable to the Studio's
//! publish gate (`Unexpected token '\x1B'`).

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn run_verify(debug: Option<&str>) -> std::process::Output {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std-modal.orb");
    let mut cmd = Command::new(orb_bin());
    cmd.args(["verify", fixture.to_str().unwrap(), "--trait", "ModalRecordModal", "--json"]);
    if let Some(namespaces) = debug {
        cmd.env("ALMADAR_DEBUG", namespaces);
    }
    cmd.output().expect("spawn orb verify")
}

#[test]
fn with_debug_logging_on_stdout_is_still_pure_json() {
    let output = run_verify(Some("almadar:*"));
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", String::from_utf8_lossy(&output.stdout)));
    assert!(!output.stderr.is_empty(), "the debug log lines must land on stderr");
}

#[test]
fn control_without_debug_logging_stdout_is_json() {
    let output = run_verify(None);
    serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("stdout is JSON");
}
