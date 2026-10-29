//! Integration proof for the cascade-suppression pass (owner ruling,
//! Stage C 2026-09-06: "suppress any downstream diagnostic at a site/entity
//! an `ORB_O_*` diagnostic already names"), wired into `orb validate --json`
//! via `orbital-cli/src/utils/diagnostics.rs::merge_post_inline_diagnostics`
//! → `orbital_compiler::phases::validation::orbital_ref_config::suppress_cascaded_by_orbital_ref`.
//!
//! Fixture (same shape as `orbital-compiler/tests/orbital_import_validation.rs`'s
//! `UPSTREAM_STAGE_B` — a Stage-B out-of-orbital `Other` relation left
//! unmapped): a consumer imports `MainOrbital`, never maps `entities { Other:
//! … }`. Left unmapped, `Other` is unrenamed in the materialized schema, so
//! comprehensive validation independently rediscovers it broken at every
//! site that touches it — `ORB_E_INVALID_RELATION` on the relation field and
//! `ORB_EFF_FETCH_INVALID_ENTITY` on the `fetch` effect — on top of the
//! `ORB_O_ENTITY_UNMAPPED` rows the orbital-import diagnostics already
//! produce for the same entity. Before this fix, `orb validate --json`
//! reported ALL of these as independent errors; after, only the `ORB_O_*`
//! rows survive, each with a "downstream finding(s) suppressed" note.

use std::path::{Path, PathBuf};
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

const UPSTREAM: &str = r#"{
  "name": "test-cli-cascade-suppression-upstream",
  "version": "1.0.0",
  "orbitals": [
    {
      "name": "MainOrbital",
      "entity": {
        "name": "Widget",
        "persistence": "persistent",
        "fields": [
          { "name": "id", "type": "string", "required": true },
          { "name": "otherId", "type": "relation", "relation": { "entity": "Other", "cardinality": "one" } }
        ]
      },
      "traits": [
        {
          "name": "WidgetView",
          "linkedEntity": "Widget",
          "stateMachine": {
            "states": [ { "name": "idle", "isInitial": true } ],
            "events": [ { "key": "OPEN" } ],
            "transitions": [ { "from": "idle", "to": "idle", "event": "OPEN", "effects": [] } ]
          }
        },
        {
          "name": "OtherFetcher",
          "linkedEntity": "Widget",
          "stateMachine": {
            "states": [ { "name": "idle", "isInitial": true } ],
            "events": [ { "key": "LOAD" } ],
            "transitions": [
              {
                "from": "idle", "to": "idle", "event": "LOAD",
                "effects": [ ["fetch", "Other", {}] ]
              }
            ]
          }
        }
      ],
      "pages": [
        { "name": "MainPage", "path": "/main", "primaryEntity": "Widget", "traits": [ { "ref": "WidgetView" } ] }
      ]
    },
    {
      "name": "OtherOrbital",
      "entity": {
        "name": "Other",
        "persistence": "persistent",
        "fields": [
          { "name": "id", "type": "string", "required": true },
          { "name": "name", "type": "string" }
        ]
      },
      "traits": []
    }
  ]
}"#;

/// `entity_target_unknown` (`Bogus2` unmapped) is a SEPARATE, genuinely
/// unrelated finding in `UnrelatedOrbital` — must survive suppression
/// untouched, proving the pass is scoped to the SAME orbital + named
/// entity, not global.
const CONSUMER: &str = r#"{
  "name": "test-cli-cascade-suppression-consumer",
  "version": "1.0.0",
  "orbitals": [
    {
      "name": "Local",
      "uses": [ { "from": "./upstream.orb", "as": "Up" } ],
      "entity": "Up.orbitals.MainOrbital.entity",
      "traits": [],
      "reference": { "ref": "Up.orbitals.MainOrbital" }
    },
    {
      "name": "UnrelatedOrbital",
      "entity": {
        "name": "Standalone",
        "persistence": "persistent",
        "fields": [
          { "name": "id", "type": "string", "required": true },
          { "name": "bogusId", "type": "relation", "relation": { "entity": "Bogus2", "cardinality": "one" } }
        ]
      },
      "traits": []
    }
  ]
}"#;

fn write_fixture(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("upstream.orb"), UPSTREAM).expect("write upstream");
    let consumer_path = dir.join("consumer.orb");
    std::fs::write(&consumer_path, CONSUMER).expect("write consumer");
    consumer_path
}

fn validate_json(consumer_path: &Path) -> serde_json::Value {
    let output = Command::new(orb_bin())
        .args(["validate", "--json", consumer_path.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");
    assert!(
        output.status.success(),
        "orb validate exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("validate --json output is not valid JSON")
}

fn codes(rows: &serde_json::Value) -> Vec<String> {
    rows.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("code").and_then(|c| c.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Rows of the given `code` whose `message` mentions `entity` (quoted or
/// not — good enough to attribute a row to the entity under test).
fn rows_naming<'a>(
    rows: &'a serde_json::Value,
    code: &str,
    entity: &str,
) -> Vec<&'a serde_json::Value> {
    rows.as_array()
        .map(|a| {
            a.iter()
                .filter(|e| {
                    e["code"] == code && e["message"].as_str().is_some_and(|m| m.contains(entity))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn unmapped_out_of_orbital_entity_suppresses_its_downstream_cascade() {
    let dir = std::env::temp_dir().join(format!(
        "orb-cascade-suppression-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let consumer_path = write_fixture(&dir);

    let result = validate_json(&consumer_path);
    let error_codes = codes(&result["errors"]);
    let warning_codes = codes(&result["warnings"]);

    assert!(
        error_codes.iter().any(|c| c == "ORB_O_ENTITY_UNMAPPED"),
        "expected at least one ORB_O_ENTITY_UNMAPPED root row, got errors: {:#?}",
        result["errors"]
    );
    // Every downstream code that independently rediscovers 'Other' broken
    // (the SAME import-left-unmapped entity `ORB_O_ENTITY_UNMAPPED` already
    // names) must have zero rows quoting it — suppressed.
    for cascaded in [
        "ORB_E_INVALID_RELATION",
        "ORB_X_UNRESOLVED_RELATION_TARGET",
        "ORB_EFF_FETCH_INVALID_ENTITY",
    ] {
        let hits = [
            rows_naming(&result["errors"], cascaded, "Other"),
            rows_naming(&result["warnings"], cascaded, "Other"),
        ]
        .concat();
        assert!(
            hits.is_empty(),
            "{} naming 'Other' must be suppressed (same orbital, same unmapped entity) — found: {:#?}",
            cascaded,
            hits
        );
    }
    let _ = &error_codes;
    let _ = &warning_codes;

    // The unrelated orbital's own broken relation ('Bogus2') is a DIFFERENT
    // entity, never named by any ORB_O_* diagnostic — must survive
    // untouched, proving suppression is scoped, not global.
    let unrelated_hits = [
        rows_naming(&result["errors"], "ORB_E_INVALID_RELATION", "Bogus2"),
        rows_naming(
            &result["warnings"],
            "ORB_X_UNRESOLVED_RELATION_TARGET",
            "Bogus2",
        ),
    ]
    .concat();
    assert!(
        !unrelated_hits.is_empty(),
        "the unrelated orbital's own 'Bogus2' relation finding must survive: errors {:#?}, warnings {:#?}",
        result["errors"],
        result["warnings"]
    );

    // The root row records what it absorbed.
    let root_note_present = result["errors"].as_array().unwrap().iter().any(|e| {
        e["code"] == "ORB_O_ENTITY_UNMAPPED"
            && e["message"]
                .as_str()
                .is_some_and(|m| m.contains("downstream finding"))
    });
    assert!(
        root_note_present,
        "at least one ORB_O_ENTITY_UNMAPPED row must record the suppressed count: {:#?}",
        result["errors"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}
