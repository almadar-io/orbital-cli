//! `orb validate --json` populates `line`/`column` on an L2 error (an
//! `SEXPR_ARITY_MISMATCH`) when the input was `.lolo` — resolved through the
//! L2 source map built during lowering — and leaves them `null` when the
//! exact same schema is provided as `.orb` JSON (no `.lolo` source to map
//! back to). `(not @entity.flag @entity.flag)` reliably trips
//! `SEXPR_ARITY_MISMATCH`: `not` takes at most 1 argument (see
//! orbital-compiler's `sexpr::validate::test_arity_mismatch`).

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn temp_path(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "orb-source-map-test-{}-{}-{}.{}",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        ext
    ))
}

const PROBE_LOLO: &str = r#"orbital ProbeOrbital {
  entity Probe [runtime] {
    flag : boolean = false
  }
  trait ProbeGuard -> Probe [interaction, instance] {
    initial: idle
    state idle {
      INIT -> idle
        (set @entity.flag false)
      NEXT -> idle when (not @entity.flag @entity.flag)
    }
  }
  page "/" as P -> ProbeGuard
}
"#;

fn validate_json(path: &PathBuf) -> serde_json::Value {
    let output = Command::new(orb_bin())
        .args(["validate", "--json", path.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");
    assert!(
        output.status.success(),
        "orb validate exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("validate --json output is not valid JSON")
}

fn arity_error(result: &serde_json::Value) -> &serde_json::Value {
    result
        .get("errors")
        .and_then(|v| v.as_array())
        .expect("errors array missing")
        .iter()
        .find(|e| e["code"] == "SEXPR_ARITY_MISMATCH")
        .expect("expected a SEXPR_ARITY_MISMATCH error")
}

#[test]
fn lolo_input_resolves_line_col_orb_json_input_does_not() {
    let lolo_path = temp_path("probe", "lolo");
    std::fs::write(&lolo_path, PROBE_LOLO).expect("write temp lolo");

    let lolo_result = validate_json(&lolo_path);
    let lolo_err = arity_error(&lolo_result);
    let line = lolo_err["line"]
        .as_u64()
        .expect("`.lolo` input must resolve a line number via the source map");
    assert!(lolo_err["column"].as_u64().is_some());

    // The guard `(not @entity.flag @entity.flag)` sits on line 10 of
    // PROBE_LOLO (1-based) — pin the exact line so a regression in trail
    // accounting (not just "some" line) is caught.
    assert_eq!(line, 10);

    // Emit the lowered `.orb` JSON for the SAME schema and re-validate —
    // no `.lolo` source exists for JSON input, so line/column must be null.
    let emit_output = Command::new(orb_bin())
        .args(["emit", "orb", lolo_path.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary for emit");
    assert!(
        emit_output.status.success(),
        "orb emit orb exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&emit_output.stderr)
    );
    let orb_path = temp_path("probe", "orb");
    std::fs::write(&orb_path, &emit_output.stdout).expect("write emitted orb json");

    let orb_result = validate_json(&orb_path);
    let orb_err = arity_error(&orb_result);
    assert!(
        orb_err["line"].is_null(),
        "`.orb` JSON input must NOT resolve a line (no source map): {orb_err}"
    );
    assert!(orb_err["column"].is_null());

    let _ = std::fs::remove_file(&lolo_path);
    let _ = std::fs::remove_file(&orb_path);
}
