//! `(emit { data: … })` — a payload object where the event name belongs —
//! must be rejected by `orb validate` as a coded, located, teachable error
//! (`ORB_EFF_EMIT_INVALID_NAME`), never fall through to the resolve phase's
//! uncoded "Failed to resolve S-expression: Expected string"
//! (T-RESOLVE-OPAQUE, run 19 TopicManager).

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

const EMIT_NO_NAME_LOLO: &str = r#"orbital TopicOrbital {
  entity Topic [runtime] {
    id : string!
  }
  trait TopicSave -> Topic {
    initial: idle
    emits { SAVED }
    state idle {
      INIT -> idle
        (emit { data: @entity.id })
        (render-ui main { type: stack children: [] })
    }
  }
  page "/" as P -> TopicSave
}"#;

#[test]
fn emit_without_event_name_is_a_coded_validate_error_not_a_resolve_crash() {
    let tmp = std::env::temp_dir().join(format!(
        "orb-emit-no-name-test-{}-{}.lolo",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, EMIT_NO_NAME_LOLO).expect("write temp lolo");

    let output = Command::new(orb_bin())
        .args(["validate", "--json", tmp.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");
    let _ = std::fs::remove_file(&tmp);

    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("validate --json output is not valid JSON");
    assert_eq!(result["valid"], false);

    let errors = result["errors"].as_array().expect("errors array");
    let emit_err = errors
        .iter()
        .find(|e| e["code"] == "ORB_EFF_EMIT_INVALID_NAME")
        .unwrap_or_else(|| panic!("ORB_EFF_EMIT_INVALID_NAME not found in {errors:?}"));
    assert!(emit_err["message"]
        .as_str()
        .unwrap()
        .contains("event name missing"));
    assert!(emit_err["suggestion"]
        .as_str()
        .unwrap()
        .contains("(emit EVENT_NAME { … })"));
    // Located via the L2 source map — the effect's own line, not null.
    assert!(
        emit_err["line"].as_u64().is_some(),
        "line missing: {emit_err:?}"
    );

    // The resolve phase must no longer be the reporter for this shape: any
    // residual COMPILATION_ERROR would be the old opaque path.
    assert!(
        !errors.iter().any(|e| e["code"] == "COMPILATION_ERROR"
            && e["message"]
                .as_str()
                .unwrap_or("")
                .contains("Expected string")),
        "opaque resolve error still present: {errors:?}"
    );
}
