//! `orb verify --trait NAME` — scope a verify run to one trait's `(from, event)`
//! pairs instead of the target's full walk.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std-modal.orb")
}

#[test]
fn verify_trait_scopes_the_report_to_the_named_trait() {
    let dir = std::env::temp_dir().join("orbital-verify-trait-scope-cli-test");
    let _ = std::fs::remove_dir_all(&dir);

    let output = Command::new(orb_bin())
        .args([
            "verify",
            fixture().to_str().unwrap(),
            "--trait",
            "ModalRecordModal",
            "--report-dir",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("spawn orb verify --trait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "orb verify --trait ModalRecordModal failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[trait ModalRecordModal]"),
        "report line should be tagged with the trait scope:\n{stdout}"
    );

    let report_path = dir.join("std-modal.verify-report.json");
    let report_json = std::fs::read_to_string(&report_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", report_path.display()));
    let report: serde_json::Value =
        serde_json::from_str(&report_json).expect("verify-report.json must be valid JSON");

    assert_eq!(
        report["traits"],
        serde_json::json!(["ModalRecordModal"]),
        "report should carry the resolved trait scope: {report}"
    );
    let frames = report["layer1"]["frames"]
        .as_array()
        .expect("layer1.frames must be an array");
    assert!(!frames.is_empty(), "scoped walk drove zero frames: {report}");
    assert!(
        frames
            .iter()
            .all(|f| f["cause"]["trait_name"] == "ModalRecordModal"),
        "every frame should belong to the named trait: {report}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_trait_unknown_name_errors_listing_available_traits() {
    let output = Command::new(orb_bin())
        .args(["verify", fixture().to_str().unwrap(), "--trait", "NoSuchTrait"])
        .output()
        .expect("spawn orb verify --trait NoSuchTrait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "orb verify --trait NoSuchTrait should fail, stderr:\n{stderr}"
    );
    assert!(stderr.contains("NoSuchTrait"), "error should name the bad value:\n{stderr}");
    assert!(
        stderr.contains("ModalRecordModal"),
        "error should list the target's real trait name(s):\n{stderr}"
    );
}
