//! An Arabic-keyed `.orb` document (produced by translating an English
//! fixture's structural keys/types/sigils into Arabic via the baked
//! `orbital_core::i18n` tables) validates identically to the English
//! original — `orb validate --json` reports the same orbital/entity/trait
//! counts and the same (empty) error list for both.
//!
//! It also proves `orb emit orb` translates the same document (the emit path
//! has its own parse dispatch for prior-ledger anchoring).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

fn orb_bin() -> PathBuf {
    common::orb()
}

fn temp_path(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "orb-i18n-orb-test-{}-{}-{}.{}",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        ext
    ))
}

const ENGLISH_LOLO: &str = r#"app probe [ version: "0.1.0" ]
orbital Probe {
  entity Item [runtime, shared] {
    id : string!
    count : int = 0
    active : boolean = true
  }
  trait Counter -> Item {
    initial: idle
    state idle {
      INIT -> idle (render-ui main { type: "typography", content: "probe" })
      BUMP -> idle when (< @entity.count 10) (set @entity.count (+ @entity.count 1))
      RESET -> idle (set @entity.count 0) (emit CLEARED { at: now })
    }
    emits { CLEARED -> external { at : timestamp } }
  }
  page "/probe" as Probe -> Counter
}
"#;

/// A real English `.orb`: the `.lolo` above lowered by the binary under test.
fn english_orb() -> serde_json::Value {
    let lolo = temp_path("en", "lolo");
    let orb = temp_path("en-lowered", "orb");
    std::fs::write(&lolo, ENGLISH_LOLO).expect("write english lolo");
    emit_orb(&lolo, &orb);
    let value = serde_json::from_str(&std::fs::read_to_string(&orb).unwrap()).unwrap();
    let _ = std::fs::remove_file(&lolo);
    let _ = std::fs::remove_file(&orb);
    value
}

fn validate_json(path: &Path) -> serde_json::Value {
    let output = Command::new(orb_bin())
        .args(["validate", "--json", path.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "validate --json output is not valid JSON: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn arabic_keyed_orb_validates_like_english() {
    let ar = orbital_core::i18n::lang::all()
        .iter()
        .find(|l| l.code == "ar")
        .expect("ar language must be registered");

    let need = [
        ("orb.orbitals", ar.def.orb.get("orbitals")),
        ("orb.name", ar.def.orb.get("name")),
        ("orb.orbitalId", ar.def.orb.get("orbitalId")),
        ("orb.entity", ar.def.orb.get("entity")),
        ("orb.fields", ar.def.orb.get("fields")),
        ("orb.traits", ar.def.orb.get("traits")),
        ("orb.required", ar.def.orb.get("required")),
        ("orb.type", ar.def.orb.get("type")),
        ("types.string", ar.def.types.get("string")),
    ];
    let missing: Vec<&str> = need
        .iter()
        .filter(|(_, v)| v.map(|s| s.is_empty()).unwrap_or(true))
        .map(|(k, _)| *k)
        .collect();
    assert!(
        missing.is_empty(),
        "ar.json must translate every key this fixture uses; missing: {missing:?}"
    );

    let mut arabic = english_orb();
    let inverted = english_to_native(ar);
    orbital_core::i18n::translate::translate_orb_value(&mut arabic, &inverted);

    let en_path = temp_path("en", "orb");
    let ar_path = temp_path("ar", "orb");
    std::fs::write(
        &en_path,
        serde_json::to_string_pretty(&english_orb()).unwrap(),
    )
    .expect("write english fixture");
    std::fs::write(&ar_path, serde_json::to_string_pretty(&arabic).unwrap())
        .expect("write arabic fixture");

    let en_result = validate_json(&en_path);
    let ar_result = validate_json(&ar_path);

    assert_eq!(
        en_result["valid"],
        serde_json::Value::Bool(true),
        "the english fixture must validate clean, else the parity below is vacuous: {en_result}"
    );
    assert_eq!(
        en_result["errors"], ar_result["errors"],
        "english vs arabic-keyed validate errors differ:\nen: {en_result}\nar: {ar_result}"
    );
    assert_eq!(en_result["summary"], ar_result["summary"]);

    // `orb emit orb` has its own parse dispatch (prior-ledger anchoring); a
    // native-keyed document must reach the translator there too.
    let en_out = temp_path("en-out", "orb");
    let ar_out = temp_path("ar-out", "orb");
    emit_orb(&en_path, &en_out);
    emit_orb(&ar_path, &ar_out);
    let mut en_emitted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&en_out).unwrap()).unwrap();
    let mut ar_emitted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ar_out).unwrap()).unwrap();
    strip_identity(&mut en_emitted);
    strip_identity(&mut ar_emitted);
    assert_eq!(
        en_emitted, ar_emitted,
        "`orb emit orb` output differs between english and arabic-keyed input"
    );

    for p in [&en_path, &ar_path, &en_out, &ar_out] {
        let _ = std::fs::remove_file(p);
    }
}

fn emit_orb(input: &Path, output: &Path) {
    let status = Command::new(orb_bin())
        .args(["emit", "orb"])
        .arg(input)
        .arg("--output")
        .arg(output)
        .status()
        .expect("run orb emit orb");
    assert!(
        status.success(),
        "`orb emit orb {}` failed",
        input.display()
    );
}

/// Drop V4 identity fields (ids, ledger, schemaVersion) — minted per lowering, not semantic content.
fn strip_identity(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|k, _| {
                !(k == "id"
                    || k == "ledger"
                    || k == "schemaVersion"
                    || k.ends_with("Id")
                    || k.ends_with("Ids"))
            });
            for v in map.values_mut() {
                strip_identity(v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_identity),
        _ => {}
    }
}

/// Build a throwaway `Lang`-shaped translator that goes the OPPOSITE
/// direction from `Lang::canon_*` (English -> native) by swapping every
/// section's key/value — `translate_orb_value` only reads `Lang`'s public
/// canon_* methods, so a `Lang` built from a definition whose maps are
/// inverted (native/English swapped) translates English -> native, which is
/// exactly the fixture-construction step this test needs (mirrors how the
/// `translate` module's own unit test builds its native fixture, but here a
/// whole `.orb` document needs the full structural walk).
fn english_to_native(
    ar: &'static orbital_core::i18n::lang::Lang,
) -> orbital_core::i18n::lang::Lang {
    let swap = |m: &std::collections::BTreeMap<String, String>| {
        let mut out = std::collections::BTreeMap::new();
        for (en, native) in m {
            out.insert(native.clone(), en.clone());
        }
        out
    };
    let def = orbital_core::i18n::tables::I18nDefinition {
        meta: ar.def.meta.clone(),
        keywords: swap(&ar.def.keywords),
        shapes: swap(&ar.def.shapes),
        tags: swap(&ar.def.tags),
        categories: swap(&ar.def.categories),
        capabilities: swap(&ar.def.capabilities),
        annotations: swap(&ar.def.annotations),
        sigils: swap(&ar.def.sigils),
        effects: swap(&ar.def.effects),
        types: swap(&ar.def.types),
        units: swap(&ar.def.units),
        literals: swap(&ar.def.literals),
        reserved_events: swap(&ar.def.reserved_events),
        orb: swap(&ar.def.orb),
        operators: swap(&ar.def.operators),
    };
    orbital_core::i18n::lang::Lang::from_definition("ar-inverted", Box::leak(Box::new(def)))
}
