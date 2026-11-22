//! `orb __resolve` (P-049): the child a serving orb resolves a program in. It stores the resolved program in the derived
//! disk cache before it exits and answers `{ stored }`, or `{ error }` for a program it cannot resolve.

use std::path::PathBuf;
use std::process::Command;

const PROGRAM: &str = r#"{
  "name": "note",
  "version": "1.0.0",
  "orbitals": [
    {
      "name": "Note",
      "entity": { "name": "NoteState", "persistence": "runtime", "fields": [ { "name": "id", "type": "string", "required": true } ] },
      "traits": [
        {
          "name": "NoteView",
          "linkedEntity": "NoteState",
          "stateMachine": {
            "states": [ { "name": "idle", "isInitial": true } ],
            "events": [],
            "transitions": [
              { "from": "idle", "to": "idle", "event": "INIT", "effects": [ ["render-ui", "main", { "type": "typography", "content": "A note" }] ] }
            ]
          }
        }
      ],
      "pages": [ { "name": "NotePage", "path": "/", "traits": [ { "ref": "NoteView" } ] } ]
    }
  ]
}"#;

fn resolve_child(program: &std::path::Path, derived: &std::path::Path) -> serde_json::Value {
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
        .arg("__resolve")
        .arg(program)
        .env("ALMADAR_DERIVED_CACHE", derived)
        .env("ORB_HANDED_OFF", "1")
        .output()
        .expect("orb runs");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("a JSON answer ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn stored_programs(derived: &std::path::Path) -> usize {
    walk(derived).into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("program-"))).count()
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] })
        .collect()
}

#[test]
fn a_resolve_child_stores_the_program_before_it_answers() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let program = dir.path().join("note.orb");
    std::fs::write(&program, PROGRAM).expect("writes the program");
    let derived = dir.path().join("derived");
    assert_eq!(resolve_child(&program, &derived), serde_json::json!({ "stored": true }));
    assert_eq!(stored_programs(&derived), 1, "the resolved program is on disk when the child has answered");
}

#[test]
fn control_a_program_the_child_cannot_read_is_an_error_and_nothing_is_stored() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let derived = dir.path().join("derived");
    let answer = resolve_child(&dir.path().join("missing.orb"), &derived);
    assert!(answer.get("error").and_then(serde_json::Value::as_str).is_some_and(|e| e.contains("missing.orb")), "{answer}");
    assert_eq!(stored_programs(&derived), 0);
}
