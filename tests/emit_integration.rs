//! Integration tests for `orb emit`.
//!
//! Covers Phase 1 (coarse TS/Python emit) and Phase 2 (`--section` filter)
//! from `docs/Almadar_Compiler_Emit_Code.md`. Runs the compiled `orb` binary
//! via `CARGO_BIN_EXE_orb` — no external crate dependency required.

use std::path::PathBuf;
use std::process::Command;

/// Self-contained fixtures vendored into the test crate. The std submodule that
/// once held these (`packages/almadar-std/…`) was removed when orbital-rust moved
/// to npm-pinned bake inputs, so the emit tests carry their own copies.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn std_modal_lolo() -> PathBuf {
    fixtures_dir().join("std-modal.lolo")
}

fn std_modal_orb() -> PathBuf {
    fixtures_dir().join("std-modal.orb")
}

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn run_orb(args: &[&str]) -> (String, String, bool) {
    let output = Command::new(orb_bin())
        .args(args)
        .output()
        .expect("failed to spawn orb binary");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

// --- Phase 1: target = orb -------------------------------------------------

#[test]
fn emit_orb_from_lolo_produces_valid_json() {
    let lolo = std_modal_lolo();
    assert!(lolo.exists(), "test fixture missing: {}", lolo.display());
    let (stdout, stderr, ok) = run_orb(&["emit", "orb", lolo.to_str().unwrap()]);
    assert!(ok, "emit orb exited non-zero. stderr:\n{}", stderr);
    // Valid JSON parse — catches regressions where the output is not well-formed.
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("output is not valid JSON");
    // Shape sanity: must have orbitals[] with the ModalRecordOrbital.
    let orbitals = parsed
        .get("orbitals")
        .and_then(|v| v.as_array())
        .expect("missing orbitals[]");
    assert!(
        orbitals
            .iter()
            .any(|o| o.get("name").and_then(|n| n.as_str()) == Some("ModalRecordOrbital")),
        "expected orbital ModalRecordOrbital in output"
    );
}

// --- Phase 1: coarse emit for TS / Python ----------------------------------

#[test]
fn emit_ts_contains_expected_exports() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, stderr, ok) = run_orb(&["emit", "ts", orb.to_str().unwrap()]);
    assert!(ok, "emit ts failed. stderr:\n{}", stderr);
    for needle in [
        "export type ModalRecordModalState =",
        "export type ModalRecordModalEvent =",
        "export interface ModalRecord",
    ] {
        assert!(
            stdout.contains(needle),
            "expected TS output to contain {:?}. first 500 chars:\n{}",
            needle,
            &stdout[..stdout.len().min(500)]
        );
    }
}

#[test]
fn emit_py_contains_expected_classes() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, stderr, ok) = run_orb(&["emit", "py", orb.to_str().unwrap()]);
    assert!(ok, "emit py failed. stderr:\n{}", stderr);
    for needle in [
        "class ModalRecord(BaseModel)",
        "class ModalRecordModalState(Enum)",
        "class ModalRecordModal",
    ] {
        assert!(
            stdout.contains(needle),
            "expected Python output to contain {:?}",
            needle
        );
    }
}

// --- Phase 2: section filter ----------------------------------------------

#[test]
fn emit_ts_section_traits_restricts_output() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run_orb(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "traits.ModalRecordModal",
    ]);
    assert!(ok);
    // Every banner line must fall under a trait-related path.
    let banners: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("// ───── ") && l.ends_with("─────"))
        .collect();
    assert!(!banners.is_empty(), "expected at least one file banner");
    for banner in &banners {
        assert!(
            banner.contains("ModalRecordModal")
                || banner.contains("modal-record-modal")
                || banner.contains("modal_record_modal"),
            "section filter leaked a non-trait file: {}",
            banner
        );
    }
    // No entities/shared code should be present.
    assert!(
        !stdout.contains("packages/shared/src/types/entities.ts"),
        "traits section should not pull in the shared entities file"
    );
}

#[test]
fn emit_ts_section_entities_excludes_reducers() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run_orb(&["emit", "ts", orb.to_str().unwrap(), "--section", "entities"]);
    assert!(ok);
    // Pure entity files have no React/reducer code.
    assert!(
        !stdout.contains("useReducer"),
        "entities section should not contain any reducer code"
    );
    assert!(stdout.contains("export interface ModalRecord"));
}

#[test]
fn emit_py_section_entities_name_filter() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run_orb(&[
        "emit",
        "py",
        orb.to_str().unwrap(),
        "--section",
        "entities.ModalRecord",
    ]);
    assert!(ok);
    assert!(
        stdout.contains("class ModalRecord(BaseModel)"),
        "entity-by-name section should contain the entity class"
    );
    // Python backend's state machine class must NOT appear.
    assert!(!stdout.contains("class ModalRecordModalState(Enum)"));
}

// --- Phase 2: composition of --section with --only ------------------------

#[test]
fn only_hooks_under_traits_section() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run_orb(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "traits",
        "--only",
        "hooks",
    ]);
    assert!(ok);
    // Every banner must have "/hooks/" in its path.
    for banner in stdout.lines().filter(|l| l.starts_with("// ───── ")) {
        assert!(
            banner.contains("/hooks/"),
            "--only hooks leaked a non-hook path: {}",
            banner
        );
    }
}

// --- Phase 1/2: format flags -----------------------------------------------

#[test]
fn json_format_parses() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run_orb(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "entities",
        "--format",
        "json",
    ]);
    assert!(ok);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--format json should produce valid JSON");
    assert!(parsed.is_object());
    let obj = parsed.as_object().unwrap();
    assert!(
        obj.keys()
            .any(|k| k.contains("shared/src/types/entities.ts")),
        "json output should be keyed by file path"
    );
}

// --- Phase 2: error handling ----------------------------------------------

#[test]
fn bad_selector_reports_clear_error() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (_, stderr, ok) = run_orb(&["emit", "ts", orb.to_str().unwrap(), "--section", "nonsense"]);
    assert!(!ok, "bad selector must exit non-zero");
    assert!(
        stderr.contains("unknown section selector"),
        "error message should identify the offending selector; got: {}",
        stderr
    );
}

// --- V4 identity: in-place re-emit id stability ------------------------------

/// `orb emit orb <lolo> --output <existing.orb>` must reuse the prior output's
/// ids (zero churn); a fresh path mints new ids, so run-2-vs-run-1 equality is
/// only guaranteed by the prior-ledger thread in `emit_orb`.
#[test]
fn emit_orb_in_place_reemit_is_id_stable() {
    let lolo = std_modal_lolo();
    assert!(lolo.exists(), "test fixture missing: {}", lolo.display());
    let mut out = std::env::temp_dir();
    out.push(format!("orb-emit-id-stable-{}.orb", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let (_, stderr, ok) = run_orb(&[
        "emit",
        "orb",
        lolo.to_str().unwrap(),
        "--output",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "first emit failed. stderr:\n{}", stderr);
    let first = std::fs::read_to_string(&out).expect("first output missing");
    assert!(
        first.contains("\"ledger\""),
        "emitted .orb should carry an identity ledger"
    );

    let (_, stderr, ok) = run_orb(&[
        "emit",
        "orb",
        lolo.to_str().unwrap(),
        "--output",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "second emit failed. stderr:\n{}", stderr);
    let second = std::fs::read_to_string(&out).expect("second output missing");

    assert_eq!(
        first, second,
        "in-place re-emit churned ids — prior ledger was not threaded"
    );
    let _ = std::fs::remove_file(&out);
}
