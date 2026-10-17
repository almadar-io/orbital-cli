//! Integration tests for `orb emit`.
//!
//! Covers Phase 1 (coarse TS/Python emit) and Phase 2 (`--section` filter)
//! from `docs/Almadar_Compiler_Emit_Code.md`. Runs the compiled `orb` binary
//! via `CARGO_BIN_EXE_orb` — no external crate dependency required.

mod common;

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
    common::orb()
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

// --- target = lolo ---------------------------------------------------------

#[test]
fn emit_lolo_from_orb_reemits_to_the_same_orb() {
    let orb = std_modal_orb();
    let (printed, stderr, ok) = run_orb(&["emit", "lolo", orb.to_str().unwrap()]);
    assert!(ok, "emit lolo exited non-zero. stderr:\n{}", stderr);
    assert!(printed.starts_with("app "), "printed source:\n{printed}");

    let dir = tempfile::tempdir().unwrap();
    let lolo = dir.path().join("printed.lolo");
    std::fs::write(&lolo, &printed).unwrap();
    let strip = |v: serde_json::Value| -> serde_json::Value {
        fn walk(v: serde_json::Value) -> serde_json::Value {
            match v {
                serde_json::Value::Object(m) => serde_json::Value::Object(
                    m.into_iter()
                        .filter(|(k, v)| {
                            k != "id"
                                && k != "ledger"
                                && k != "schemaVersion"
                                && !k.ends_with("Id")
                                && !k.ends_with("Ids")
                                && !v.is_null()
                        })
                        .map(|(k, v)| (k, walk(v)))
                        .collect(),
                ),
                serde_json::Value::Array(a) => {
                    serde_json::Value::Array(a.into_iter().map(walk).collect())
                }
                v => v,
            }
        }
        walk(v)
    };
    let (before, _, ok) = run_orb(&["emit", "orb", std_modal_lolo().to_str().unwrap()]);
    assert!(ok);
    let (after, stderr, ok) = run_orb(&["emit", "orb", lolo.to_str().unwrap()]);
    assert!(ok, "printed source must compile. stderr:\n{stderr}");
    let a: serde_json::Value = serde_json::from_str(&before).unwrap();
    let b: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(strip(a), strip(b));
}

#[test]
fn emit_lolo_rejects_unreadable_input() {
    let (_, _, ok) = run_orb(&["emit", "lolo", "/nonexistent/file.orb"]);
    assert!(!ok);
}

// --- target = manifest ------------------------------------------------------

fn two_unit_program() -> serde_json::Value {
    let orbital = |name: &str, os_grants: serde_json::Value, effects: serde_json::Value| {
        serde_json::json!({
            "name": name,
            "osGrants": os_grants,
            "entity": { "name": format!("{name}Item"), "fields": [{ "name": "id", "type": "string" }] },
            "traits": [{
                "name": format!("{name}Flow"),
                "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "GO", "effects": effects }]
                }
            }]
        })
    };
    serde_json::json!({
        "name": "placed",
        "egress": ["https://api.example.org"],
        "deployment": { "api": ["Orders"], "web": ["Storefront"] },
        "orbitals": [
            orbital(
                "Orders",
                serde_json::json!({ "exec": ["git"] }),
                serde_json::json!([["http", "GET", "https://api.example.org", "/v1",
                    { "auth": { "scheme": "bearer", "credential": "stripe" } }, { "emit": { "success": "OK" } }]]),
            ),
            orbital("Storefront", serde_json::json!({}), serde_json::json!([])),
        ]
    })
}

#[test]
fn emit_manifest_writes_a_manifest_per_unit_signs_on_request_and_refuses_unknown_units() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join("placed.orb");
    std::fs::write(&program, two_unit_program().to_string()).expect("write program");
    let program = program.to_str().unwrap();
    let out = dir.path().join("manifests");
    let out_arg = out.to_str().unwrap();

    let (canonical, _, ok) = run_orb(&["emit", "orb", program]);
    assert!(ok);
    let digest: String = Sha256::digest(canonical.trim_end_matches('\n').as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let (_, stderr, ok) = run_orb(&["emit", "manifest", program, "--out", out_arg]);
    assert!(ok, "emit manifest failed. stderr:\n{stderr}");
    assert!(stderr.contains("UNSIGNED"), "{stderr}");
    let read = |unit: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(out.join(format!("{unit}.manifest.json"))).expect(unit);
        serde_json::from_str(&text).expect("manifest json")
    };
    let (api, web) = (read("api"), read("web"));
    assert_eq!(api["unit"], "api");
    assert_eq!(api["program"], format!("sha256:{digest}"));
    assert_eq!(api["grants"]["exec"], serde_json::json!(["git"]));
    assert_eq!(api["credentials"], serde_json::json!({ "Orders": ["stripe"] }));
    assert_eq!(api["egress"], serde_json::json!(["https://api.example.org"]));
    assert!(api["orbitals"].get("Storefront").is_none());
    assert_eq!(web["unit"], "web");
    assert_eq!(web["credentials"], serde_json::json!({}));
    assert_eq!(web["grants"]["exec"], serde_json::json!([]));
    assert!(api.get("signature").is_none());

    let (_, stderr, ok) = run_orb(&["emit", "manifest", program]);
    assert!(!ok && stderr.contains("2 units"), "{stderr}");
    let (_, stderr, ok) = run_orb(&["emit", "manifest", program, "--unit", "nope"]);
    assert!(!ok && stderr.contains("MANIFEST_BUILD_UNKNOWN_UNIT"), "{stderr}");

    let seed = dir.path().join("seed");
    std::fs::write(&seed, "07".repeat(32)).expect("seed");
    let (stdout, stderr, ok) = run_orb(&[
        "emit", "manifest", program, "--unit", "api",
        "--sign-key", seed.to_str().unwrap(), "--key-id", "deploy-1",
    ]);
    assert!(ok, "signed emit failed. stderr:\n{stderr}");
    assert!(!stderr.contains("UNSIGNED"), "{stderr}");
    let manifest = io_gate::Manifest::parse(&stdout).expect("signed manifest parses");
    let keys = io_gate::manifest::KeySet::from_keys([(
        "deploy-1".to_string(),
        io_gate::manifest::public_key_of(&[7; 32]).expect("public key"),
    )]);
    assert_eq!(manifest.verify(&keys, &digest), Ok(()));
    assert!(manifest.credential_allowed("Orders", "stripe"));
}
