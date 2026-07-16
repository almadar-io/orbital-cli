//! Regression test: `orb validate --json` must surface compile/resolve
//! failures. A malformed `persist` effect (missing its operation verb —
//! `["persist", <Entity>, {…}]` instead of `["persist", "create", <Entity>, {…}]`)
//! parses and passes comprehensive validation but fails at `compile_schema`.
//! The `--json` path used to drop that `Err` (`if let Ok`) and report
//! `valid:true`, disagreeing with plain `orb validate`.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn validate_json(orb: &str, name: &str) -> serde_json::Value {
    let tmp = std::env::temp_dir().join(format!(
        "orb-validate-compile-err-{}-{}-{}.orb",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, orb).expect("write temp orb");

    let output = Command::new(orb_bin())
        .args(["validate", "--json", tmp.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");

    assert!(
        output.status.success(),
        "orb validate exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("validate --json output is not valid JSON");

    let _ = std::fs::remove_file(&tmp);
    result
}

/// Fully valid schema whose SAVE transition carries a well-formed
/// `["persist", "create", "Note", {…}]` effect.
const WELL_FORMED_PERSIST: &str = r#"{
  "name": "MinPersist",
  "version": "1.0.0",
  "orbitals": [
    {
      "name": "Note",
      "entity": {
        "name": "Note",
        "collection": "notes",
        "persistence": "persistent",
        "fields": [
          { "name": "id", "required": true, "type": "string" },
          { "name": "body", "type": "string" },
          { "items": { "type": "object" }, "name": "data", "type": "array" }
        ]
      },
      "traits": [
        {
          "name": "NoteFeed",
          "category": "interaction",
          "linkedEntity": "Note",
          "scope": "collection",
          "emits": [
            { "event": "SAVE", "scope": "external", "tier": "domain", "payloadSchema": [{ "name": "body", "type": "string" }] },
            { "event": "SAVED", "scope": "external", "tier": "domain", "payloadSchema": [{ "name": "id", "type": "string" }] },
            { "event": "SAVE_FAILED", "scope": "external", "tier": "internal", "payloadSchema": [{ "name": "error", "type": "string" }] },
            { "event": "LOADED", "scope": "external", "tier": "domain", "payloadSchema": [{ "name": "data", "type": "[json]" }] },
            { "event": "LOAD_FAILED", "scope": "external", "tier": "internal", "payloadSchema": [{ "name": "error", "type": "string" }] }
          ],
          "stateMachine": {
            "events": [
              { "key": "INIT", "name": "Init" },
              { "key": "SAVE", "name": "Save", "payloadSchema": [{ "name": "body", "type": "string" }] },
              { "key": "SAVED", "name": "Saved", "payloadSchema": [{ "name": "id", "type": "string" }] },
              { "key": "SAVE_FAILED", "name": "Save Failed", "payloadSchema": [{ "name": "error", "type": "string" }] },
              { "key": "LOADED", "name": "Loaded", "payloadSchema": [{ "name": "data", "type": "[json]" }] },
              { "key": "LOAD_FAILED", "name": "Load Failed", "payloadSchema": [{ "name": "error", "type": "string" }] }
            ],
            "states": [
              { "name": "loading", "isInitial": true },
              { "name": "browsing" },
              { "name": "saving" }
            ],
            "transitions": [
              {
                "event": "INIT",
                "from": "loading",
                "to": "loading",
                "effects": [
                  ["fetch", "Note", { "emit": { "failure": "LOAD_FAILED", "success": "LOADED" } }],
                  ["render-ui", "main", { "type": "typography", "content": "Loading notes" }]
                ]
              },
              {
                "event": "LOADED",
                "from": "loading",
                "to": "browsing",
                "effects": [
                  ["set", "@entity.data", "@payload.data"],
                  ["render-ui", "main", { "type": "typography", "content": "Notes" }]
                ]
              },
              { "event": "LOAD_FAILED", "from": "loading", "to": "browsing" },
              {
                "event": "SAVE",
                "from": "browsing",
                "to": "saving",
                "effects": [
                  ["persist", "create", "Note", { "emit": { "failure": "SAVE_FAILED", "success": "SAVED" }, "fields": { "body": "@payload.body" } }]
                ]
              },
              { "event": "SAVED", "from": "saving", "to": "browsing" },
              { "event": "SAVE_FAILED", "from": "saving", "to": "browsing" }
            ]
          }
        }
      ],
      "pages": [
        { "name": "NotePage", "path": "/notes", "traits": [{ "ref": "NoteFeed" }] }
      ]
    }
  ]
}"#;

/// Same schema with the persist operation verb removed:
/// `["persist", "Note", {…}]` — resolves to a compile error, not a
/// comprehensive-validation error.
fn malformed_persist_schema() -> String {
    let mut schema: serde_json::Value =
        serde_json::from_str(WELL_FORMED_PERSIST).expect("fixture parses");
    let effect = &mut schema["orbitals"][0]["traits"][0]["stateMachine"]["transitions"][3]
        ["effects"][0];
    let arr = effect.as_array_mut().expect("persist effect is an array");
    assert_eq!(arr[1], "create");
    arr.remove(1);
    schema.to_string()
}

#[test]
fn malformed_persist_effect_reports_invalid_via_json() {
    let result = validate_json(&malformed_persist_schema(), "bad");
    assert_eq!(
        result["valid"], false,
        "malformed persist must report valid:false, got: {result}"
    );
    let errors = result["errors"]
        .as_array()
        .expect("errors array present on invalid schema");
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "COMPILATION_ERROR"
                && e["message"]
                    .as_str()
                    .map(|m| m.starts_with("Compilation error:"))
                    .unwrap_or(false)),
        "expected a COMPILATION_ERROR row, got: {errors:#?}"
    );
}

#[test]
fn well_formed_persist_effect_stays_valid_via_json() {
    let result = validate_json(WELL_FORMED_PERSIST, "good");
    assert_eq!(
        result["valid"], true,
        "well-formed persist must stay valid:true, got: {result}"
    );
}
