//! `orb execute --json` — headless state-machine execution DTO.
//!
//! Fixture (`fixtures/execute_guarded.orb`): one trait (`GateSwitch`), one
//! guarded `INIT` transition (`idle` -> `active`) carrying two effects. Both
//! cases fire the same event against the same schema, varying only whether
//! the entity satisfies the guard.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/execute_guarded.orb")
}

fn execute_json(entity: &str) -> serde_json::Value {
    let output = Command::new(orb_bin())
        .args([
            "execute",
            fixture_path().to_str().unwrap(),
            "--event",
            "INIT",
            "--entity",
            entity,
            "--json",
        ])
        .output()
        .expect("failed to spawn orb binary");

    assert!(
        output.status.success(),
        "orb execute exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("execute --json output is not valid JSON")
}

#[test]
fn guard_passes_fires_transition_with_two_effects() {
    let result = execute_json(r#"{"ready": true}"#);

    assert_eq!(result["success"], true, "expected success:true, got: {result}");
    assert_eq!(result["initialState"], "idle");
    assert_eq!(result["finalState"], "active");

    let transitions = result["transitions"].as_array().expect("transitions array");
    assert_eq!(transitions.len(), 1, "expected 1 transition, got: {transitions:#?}");
    assert_eq!(transitions[0]["from"], "idle");
    assert_eq!(transitions[0]["to"], "active");
    assert_eq!(transitions[0]["event"], "INIT");

    let effects = result["effects"].as_array().expect("effects array");
    assert_eq!(effects.len(), 2, "expected 2 effects, got: {effects:#?}");
    assert_eq!(effects[0]["operator"], "set");
    assert_eq!(effects[1]["operator"], "render-ui");

    let guards = result["guardEvaluations"].as_array().expect("guardEvaluations array");
    assert_eq!(guards.len(), 1, "expected 1 guard evaluation, got: {guards:#?}");
    assert_eq!(guards[0]["result"], true);
    assert_eq!(guards[0]["path"], "traits[GateSwitch].transitions[INIT]");
}

#[test]
fn guard_fails_fires_no_transition() {
    let result = execute_json(r#"{"ready": false}"#);

    assert_eq!(result["success"], true, "expected success:true, got: {result}");
    assert_eq!(result["initialState"], "idle");
    assert_eq!(result["finalState"], "idle");

    let transitions = result["transitions"].as_array().expect("transitions array");
    assert_eq!(transitions.len(), 0, "expected 0 transitions, got: {transitions:#?}");

    let effects = result["effects"].as_array().expect("effects array");
    assert_eq!(effects.len(), 0, "expected 0 effects, got: {effects:#?}");

    let guards = result["guardEvaluations"].as_array().expect("guardEvaluations array");
    assert_eq!(guards.len(), 1, "expected 1 guard evaluation, got: {guards:#?}");
    assert_eq!(guards[0]["result"], false);
}

#[test]
fn unresolvable_schema_reports_success_false() {
    let output = Command::new(orb_bin())
        .args([
            "execute",
            "tests/fixtures/does-not-exist-execute.orb",
            "--event",
            "INIT",
            "--json",
        ])
        .output()
        .expect("failed to spawn orb binary");

    assert!(
        output.status.success(),
        "orb execute exited non-zero even for a data-level failure. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("execute --json output is not valid JSON");
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().is_some(), "expected an error message, got: {result}");
}

fn execute_required_payload(payload: &str) -> serde_json::Value {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/execute_required_payload.orb");
    let output = Command::new(orb_bin())
        .args(["execute", fixture.to_str().unwrap(), "--event", "START_SPAN", "--payload", payload, "--json"])
        .output()
        .expect("failed to spawn orb binary");
    serde_json::from_slice(&output.stdout).expect("execute --json output is not valid JSON")
}

#[test]
fn a_missing_required_payload_field_is_refused_before_any_transition() {
    let result = execute_required_payload("{}");
    assert_eq!(result["success"], false, "{result}");
    assert_eq!(
        result["error"],
        "Payload validation failed for event 'START_SPAN': name (missing, expected string)"
    );
    assert_eq!(result["transitions"].as_array().map(Vec::len), Some(0));
}

#[test]
fn control_a_present_required_payload_field_fires_the_transition() {
    let result = execute_required_payload(r#"{"name": "root"}"#);
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["finalState"], "active");
}
