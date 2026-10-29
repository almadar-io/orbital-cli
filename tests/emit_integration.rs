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
        "export interface ModalRecord",
        "export type ModalRecordModalEventPayloadMap =",
        "// ───── packages/client/public/program.client.orb ─────",
    ] {
        assert!(
            stdout.contains(needle),
            "expected TS output to contain {:?}. first 500 chars:\n{}",
            needle,
            &stdout[..stdout.len().min(500)]
        );
    }
}

// The Python shell runs the served program on the native kernel: no entity model, state enum or trait class is generated, so `emit py` has no file to print.
#[test]
fn emit_py_matches_no_file_under_the_kernel_host() {
    let orb = std_modal_orb();
    let (stdout, stderr, ok) = run_orb(&["emit", "py", orb.to_str().unwrap()]);
    assert!(ok, "emit py failed. stderr:\n{}", stderr);
    assert!(
        stderr.contains("matched zero generated files"),
        "expected the zero-match warning. stderr:\n{stderr}"
    );
    for absent in ["class ModalRecord(BaseModel)", "class ModalRecordModalState(Enum)", "class ModalRecordModal"] {
        assert!(!stdout.contains(absent), "unexpected {absent:?} in:\n{stdout}");
    }
}

// --- Phase 2: section filter ----------------------------------------------

// The kernel host interprets trait logic from the program (`program.orb`); no per-trait state, hook or component file exists to select.
#[test]
fn emit_ts_section_traits_matches_no_file_under_the_kernel_host() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, stderr, ok) = run_orb(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "traits.ModalRecordModal",
    ]);
    assert!(ok);
    assert!(
        stderr.contains("matched zero generated files"),
        "expected the zero-match warning. stderr:\n{stderr}"
    );
    assert!(
        !stdout.lines().any(|l| l.starts_with("// ───── ")),
        "a trait section must not select any file:\n{stdout}"
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
fn emit_py_section_entities_name_filter_matches_no_file_under_the_kernel_host() {
    let orb = std_modal_orb();
    let (stdout, stderr, ok) = run_orb(&["emit", "py", orb.to_str().unwrap(), "--section", "entities.ModalRecord"]);
    assert!(ok, "emit py failed. stderr:\n{}", stderr);
    assert!(
        stderr.contains("matched zero generated files"),
        "expected the zero-match warning. stderr:\n{stderr}"
    );
    assert!(!stdout.contains("class ModalRecord(BaseModel)"), "no entity class is generated:\n{stdout}");
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
                                // spelling-dependent provenance: the printed source hashes differently
                                && k != "sourceDeclaration"
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
    use base64::Engine;
    use ring::signature::{KeyPair, UnparsedPublicKey, ED25519};
    let mut signed: serde_json::Value = serde_json::from_str(&stdout).expect("signed manifest is JSON");
    let signature = signed.as_object_mut().unwrap().remove("signature").expect("a signed manifest carries its signature");
    assert_eq!((signature["alg"].as_str(), signature["key_id"].as_str()), (Some("ed25519"), Some("deploy-1")));
    assert_eq!(signed["program"], format!("sha256:{digest}"));
    assert_eq!(signed["credentials"], serde_json::json!({ "Orders": ["stripe"] }));
    let public = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("key").public_key().as_ref().to_vec();
    let sig = base64::engine::general_purpose::STANDARD.decode(signature["sig"].as_str().expect("sig")).expect("base64");
    let canonical = canonical_json(&signed);
    assert!(UnparsedPublicKey::new(&ED25519, &public).verify(canonical.as_bytes(), &sig).is_ok(), "the signature covers the canonical manifest");
    signed["program"] = serde_json::json!("sha256:tampered");
    assert!(UnparsedPublicKey::new(&ED25519, &public).verify(canonical_json(&signed).as_bytes(), &sig).is_err(), "control: a changed manifest no longer verifies");
}

// --- `orb compile` keeps what the program authored --------------------------

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries.into_iter().map(|(k, v)| format!("{}:{}", serde_json::Value::String(k.clone()), canonical_json(v))).collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => format!("[{}]", items.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        scalar => scalar.to_string(),
    }
}

fn null_keys(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if child.is_null() {
                    found.push(key.clone());
                }
                null_keys(child, found);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| null_keys(item, found)),
        _ => {}
    }
}

fn find_emit<'a>(value: &'a serde_json::Value, event: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => {
            if items.first().and_then(|v| v.as_str()) == Some("emit") && items.get(1).and_then(|v| v.as_str()) == Some(event) {
                return items.get(2);
            }
            items.iter().find_map(|item| find_emit(item, event))
        }
        serde_json::Value::Object(map) => map.values().find_map(|child| find_emit(child, event)),
        _ => None,
    }
}

/// An authored `null` inside a payload literal is a value the program runs with (`orb lower`, `orb resolve` and the
/// embedded kernel all keep it), so `orb compile` must not strip it from `schema.orb`, `resolved.orb` or the shipped programs;
/// a schema field left unset stays out of the JSON, which the control checks: the only null anywhere is the authored one.
#[test]
fn compile_keeps_an_authored_null_payload_literal_and_leaves_unset_schema_fields_out() {
    let lolo = fixtures_dir().join("emit-null-literal.lolo");
    let out = tempfile::tempdir().expect("tempdir");
    let (_stdout, stderr, ok) = run_orb(&["compile", lolo.to_str().unwrap(), "--shell", "typescript", "--host", "kernel", "-o", out.path().to_str().unwrap()]);
    assert!(ok, "compile exited non-zero. stderr:\n{stderr}");

    for file in ["schema.orb", "resolved.orb", "packages/server/generated/program.orb", "packages/client/public/program.client.orb"] {
        let text = std::fs::read_to_string(out.path().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file} is not JSON: {e}"));
        let payload = find_emit(&json, "NULL_FAILED").unwrap_or_else(|| panic!("{file}: the emit effect is missing"));
        assert_eq!(payload.get("code"), Some(&serde_json::Value::Null), "{file}: the authored `code: null` was dropped: {payload}");
        assert_eq!(payload["message"], "nothing to do", "{file}");
        // The composed std server and the client's protocol table carry nulls of their own; the fixture's orbital is the unit under test.
        let mut nulls = Vec::new();
        for orbital in json["orbitals"].as_array().expect("orbitals[]").iter().filter(|o| o["name"] == "NullOrbital") {
            null_keys(orbital, &mut nulls);
        }
        assert_eq!(nulls, vec!["code".to_string()], "{file}: a null other than the authored one is in NullOrbital");
    }
}

// --- `emit orb`, the manifest program hash and `stamp` keep authored nulls ----

/// A deployable program whose one transition emits a payload carrying an authored `null`.
fn null_payload_program() -> serde_json::Value {
    serde_json::json!({
        "name": "nulls",
        "deployment": { "api": ["Orders"] },
        "orbitals": [{
            "name": "Orders",
            "entity": { "name": "OrdersItem", "fields": [{ "name": "id", "type": "string" }] },
            "traits": [{
                "name": "OrdersFlow",
                "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{
                        "from": "idle", "to": "idle", "event": "GO",
                        "effects": [["emit", "NULL_FAILED", { "code": null, "message": "nope" }]]
                    }]
                }
            }]
        }]
    })
}

fn assert_only_the_authored_null(json: &serde_json::Value, what: &str) {
    let payload = find_emit(json, "NULL_FAILED").unwrap_or_else(|| panic!("{what}: the emit effect is missing"));
    assert_eq!(payload.get("code"), Some(&serde_json::Value::Null), "{what}: the authored `code: null` was dropped: {payload}");
    let mut nulls = Vec::new();
    null_keys(json, &mut nulls);
    assert_eq!(nulls, vec!["code".to_string()], "{what}: a null other than the authored one is in the output");
}

#[test]
fn emit_orb_the_manifest_hash_and_stamp_keep_an_authored_null_and_the_hash_is_stable() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join("nulls.orb");
    std::fs::write(&program, null_payload_program().to_string()).expect("write program");
    let program = program.to_str().unwrap();

    let (canonical, stderr, ok) = run_orb(&["emit", "orb", program]);
    assert!(ok, "emit orb failed. stderr:\n{stderr}");
    assert_only_the_authored_null(&serde_json::from_str(&canonical).expect("emit orb is JSON"), "emit orb");

    let digest: String = Sha256::digest(canonical.trim_end_matches('\n').as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut hashes = Vec::new();
    for run in ["first", "second"] {
        let out = dir.path().join(run);
        let (_, stderr, ok) = run_orb(&["emit", "manifest", program, "--out", out.to_str().unwrap()]);
        assert!(ok, "emit manifest ({run}) failed. stderr:\n{stderr}");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("api.manifest.json")).expect("api manifest")).expect("manifest json");
        hashes.push(manifest["program"].as_str().expect("program hash").to_string());
    }
    assert_eq!(hashes[0], hashes[1], "the manifest program hash differs between two runs");
    assert_eq!(hashes[0], format!("sha256:{digest}"), "the hash is not the hash of the program `emit orb` writes, authored null included");

    let (stamped, stderr, ok) = run_orb(&["stamp", program]);
    assert!(ok, "stamp failed. stderr:\n{stderr}");
    let stamped: serde_json::Value = serde_json::from_str(&stamped).expect("stamp is JSON");
    let payload = find_emit(&stamped, "NULL_FAILED").expect("stamp: the emit effect is missing");
    assert_eq!(payload.get("code"), Some(&serde_json::Value::Null), "stamp: the authored `code: null` was dropped: {payload}");
    let mut nulls = Vec::new();
    null_keys(&stamped, &mut nulls);
    assert_eq!(nulls, vec!["code".to_string()], "stamp: a null other than the authored one is in the output");
}

// --- the compile gate: a program that fails `orb validate` is never compiled ----

/// `orb compile` (std-orb-compile over `program/eval { generate }`) prints the op's report lines and writes the project.
#[test]
fn compile_prints_the_generate_report_and_writes_the_project() {
    let lolo = fixtures_dir().join("emit-null-literal.lolo");
    let out = tempfile::tempdir().expect("tempdir");
    let target = out.path().join("app");
    let (stdout, stderr, ok) = run_orb(&["compile", lolo.to_str().unwrap(), "--shell", "typescript", "-o", target.to_str().unwrap()]);
    assert!(ok, "compile exited non-zero. stderr:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], format!("Compiling schema: {}", lolo.display()), "{stdout}");
    assert!(lines.contains(&"Target shell: Typescript") && lines.contains(&"Mode: Full"), "{stdout}");
    assert!(lines.iter().any(|l| l.starts_with("✅ Generated ") && l.ends_with(" files")), "{stdout}");
    assert!(target.join("schema.orb").is_file(), "the project is written");
}

#[test]
fn compile_refuses_a_program_that_fails_validate_and_writes_nothing() {
    let lolo = fixtures_dir().join("compile-invalid.lolo");
    let out = tempfile::tempdir().expect("tempdir");
    let target = out.path().join("app");
    let (_stdout, stderr, ok) = run_orb(&["compile", lolo.to_str().unwrap(), "--shell", "typescript", "-o", target.to_str().unwrap()]);
    assert!(!ok, "compile accepted an invalid program");
    assert!(stderr.contains("ORB_X_OPERATOR_NULL_OPERAND"), "the refusal names the finding:\n{stderr}");
    assert!(!target.exists(), "nothing is written for a refused program");
}
