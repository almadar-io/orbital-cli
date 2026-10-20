// `orb behaviors catalog | describe | source` — drives the real `orb` binary
// against a temp project holding two tiny fake behavior packages whose
// `dist/vectors.vec` snapshots hold known vectors, so the decode math, the
// scope/exposure filtering, the mixed-model refusal, the embed-on-miss
// contract and the shapes are asserted exactly.

use orbital_core::vector_store::{quantize, text_key, VectorStore};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CANON: &str = "baai/bge-base-en-v1.5";

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

/// The known description vector of a fixture card; stored int8-quantized, so a read decodes the quantized values.
const CARD_VECTOR: [f32; 4] = [1.27, -0.01, -1.28, 0.1];
const CAPABILITY_VECTOR: [f32; 4] = [0.5, 0.25, -0.5, 1.0];

fn decoded(v: &[f32]) -> Vec<f64> {
    let q = quantize(v);
    let scale: f64 = q.scale.to_string().parse().unwrap();
    q.bytes
        .iter()
        .map(|b| (f64::from(*b as i8) * scale) / 127.0)
        .collect()
}

/// The embedding text of a fixture card (`index`) and of its capabilities.
fn card_texts(prefix: &str) -> (String, String) {
    (
        format!(
            "{prefix}-card\nA card.\nCapabilities (organism): display\nEntity: Card\nEvents: OPEN"
        ),
        "Capabilities (organism): display".to_string(),
    )
}

fn board_text(prefix: &str) -> String {
    format!("{prefix}-board\nA board.\nEntity: Thing")
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const ORB: &str = r#"{"name":"fake","description":"A board.","orbitals":[{"name":"FakeOrbital","entity":{"name":"Thing","fields":[]},"traits":[]}]}"#;

/// A card behavior whose `CardView` declares the knobs and events the signature fixture lists.
const CARD_ORB: &str = r#"{"name":"card","description":"A card.","orbitals":[{"name":"CardOrbital","entity":{"name":"Card","persistence":"runtime","fields":[]},"traits":[{"name":"CardView","linkedEntity":"Card","config":{"title":{"type":"string","default":"Hello","label":"Title","tier":"domain"},"size":{"type":"number","default":3}},"emits":[{"event":"CARD_OPENED"}],"listens":[{"event":"OPEN","triggers":"OPEN"}],"stateMachine":{"states":[{"name":"shown","isInitial":true}],"events":[{"key":"OPEN","name":"Open"},{"key":"INIT","name":"Initialize"}],"transitions":[{"from":"shown","to":"shown","event":"OPEN","effects":[["emit","CARD_OPENED"]]},{"from":"shown","to":"shown","event":"INIT"}]}}]}]}"#;

fn package(root: &Path, dir: &str, prefix: &str, model: &str, extra: &[(&str, &str)]) {
    let pkg = root.join("node_modules").join(dir);
    write(
        &pkg.join("package.json"),
        &format!(
            r#"{{"name":"{dir}","version":"1.0.0","orb":{{"prefix":"{prefix}","registry":"behaviors/registry","source":"behaviors/lolo","vectors":"dist/vectors.vec","signatures":"behaviors/signatures.lolo","compiler":">=0.0.0"}}}}"#
        ),
    );
    let (card, capability) = card_texts(prefix);
    VectorStore::insert_many(
        &pkg.join("dist/vectors.vec"),
        model,
        4,
        vec![
            (text_key(model, &card), quantize(&CARD_VECTOR)),
            (text_key(model, &capability), quantize(&CAPABILITY_VECTOR)),
            (text_key(model, &board_text(prefix)), quantize(&CARD_VECTOR)),
        ],
    )
    .unwrap();
    for (rel, content) in extra {
        write(&pkg.join(rel), content);
    }
}

/// The card's `.lolo` header: the tags its exposure and capabilities are read from.
const CARD_LOLO: &str =
    "app card [\n  version: \"1.0.0\"\n]\n;; @exposure palette\n;; @capabilities \"display\"\n";

fn project(second_model: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        r#"{"name":"catalog-test","private":true}"#,
    );
    orbital_test_support::link_installed_language(
        dir.path(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    package(
        dir.path(),
        "alpha",
        "alpha",
        "m1",
        &[
            ("behaviors/lolo/ui/core/atoms/alpha-card.lolo", CARD_LOLO),
            ("behaviors/registry/ui/core/atoms/alpha-card.orb", CARD_ORB),
            ("behaviors/registry/app/organisms/alpha-board.orb", ORB),
        ],
    );
    package(
        dir.path(),
        "beta",
        "beta",
        second_model,
        &[
            ("behaviors/lolo/ui/core/atoms/beta-card.lolo", CARD_LOLO),
            ("behaviors/registry/ui/core/atoms/beta-card.orb", CARD_ORB),
            ("behaviors/registry/app/organisms/beta-board.orb", ORB),
        ],
    );
    dir
}

/// No embedding provider, and a user vector store inside `dir`: a text with no stored vector is an error, never a network call.
fn orb(dir: &Path, args: &[&str]) -> Output {
    Command::new(orb_bin())
        .args(args)
        .current_dir(dir)
        .env("ALMADAR_VECTORS_DIR", dir.join(".user-vectors"))
        .env("ALMADAR_DERIVED_CACHE", dir.join(".derived"))
        .env_remove("EMBEDDING_PROVIDER")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("OPEN_ROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("spawn orb")
}

fn json(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "orb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

fn assert_close(actual: &serde_json::Value, expected: &[f64]) {
    let got: Vec<f64> = actual
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(expected) {
        assert!((g - e).abs() < 1e-12, "{g} != {e}");
    }
}

#[test]
fn catalog_decodes_int8_vectors_and_shapes_entries() {
    let p = project("m1");
    let v = json(&orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha/ui/core"],
    ));
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e["behavior"], "alpha/alpha-card");
    assert_eq!(e["package"], "alpha");
    assert_eq!(e["topic"], "ui/core");
    assert_eq!(e["level"], "atom");
    assert_eq!(e["exposure"], "palette");
    assert_eq!(
        e["traits"],
        serde_json::json!([{"behavior":"alpha/alpha-card","trait":"CardView"}])
    );
    assert_eq!(
        e["value"],
        serde_json::json!({"behavior":"alpha/alpha-card"})
    );
    assert_eq!(e["model"], "m1");
    assert_eq!(e["capabilities"], serde_json::json!(["display"]));
    assert_close(&e["vectors"]["description"], &decoded(&CARD_VECTOR));
    assert_close(&e["vectors"]["capabilities"], &decoded(&CAPABILITY_VECTOR));
    let keys: Vec<&String> = e.as_object().unwrap().keys().collect();
    assert!(keys.len() == 11, "strict key set, got {keys:?}");
}

#[test]
fn catalog_filters_by_topic_tier_and_exposure_and_sorts() {
    let p = project("m1");
    let all = json(&orb(p.path(), &["behaviors", "catalog", "--json", "alpha"]));
    let names: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["behavior"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha/alpha-board", "alpha/alpha-card"]);
    // The board's package declares no exposure defaults and the board no tag: the key is omitted, and an exposure filter drops it.
    assert!(all[0].get("exposure").is_none());

    let tier = json(&orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha/ui/core/atoms"],
    ));
    assert_eq!(tier.as_array().unwrap().len(), 1);
    let wrong_tier = json(&orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha/ui/core/organisms"],
    ));
    assert!(wrong_tier.as_array().unwrap().is_empty());
    let topic = json(&orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha/app"],
    ));
    assert_eq!(topic[0]["behavior"], "alpha/alpha-board");

    let exposed = json(&orb(
        p.path(),
        &[
            "behaviors",
            "catalog",
            "--json",
            "alpha",
            "--exposure",
            "palette",
        ],
    ));
    assert_eq!(exposed.as_array().unwrap().len(), 1);
    let none = json(&orb(
        p.path(),
        &[
            "behaviors",
            "catalog",
            "--json",
            "alpha",
            "--exposure",
            "app",
        ],
    ));
    assert!(none.as_array().unwrap().is_empty());
}

#[test]
fn catalog_without_vectors_needs_no_store_and_carries_the_pattern_tag() {
    let p = project("m2");
    let beta_card = p
        .path()
        .join("node_modules/beta/behaviors/lolo/ui/core/atoms/beta-card.lolo");
    write(&beta_card, &format!("{CARD_LOLO};; @pattern \"card\"\n"));
    let v = json(&orb(
        p.path(),
        &[
            "behaviors",
            "catalog",
            "--json",
            "--no-vectors",
            "alpha",
            "beta",
        ],
    ));
    let entries = v.as_array().unwrap();
    assert_eq!(
        entries.len(),
        4,
        "two packages of two behaviors, whatever their vector models"
    );
    for e in entries {
        assert_eq!(e["vectors"], serde_json::json!({}));
        assert_eq!(e["model"], "");
    }
    let beta = entries
        .iter()
        .find(|e| e["behavior"] == "beta/beta-card")
        .unwrap();
    assert_eq!(beta["pattern"], "card");
    assert_eq!(beta["capabilities"], serde_json::json!(["display"]));
    // Control: a behavior without the tag carries no pattern key.
    let alpha = entries
        .iter()
        .find(|e| e["behavior"] == "alpha/alpha-card")
        .unwrap();
    assert!(alpha.get("pattern").is_none());
    // Control: with vectors the same two packages are refused (mixed models), so the flag is what lifted it.
    assert!(!orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha", "beta"]
    )
    .status
    .success());
}

#[test]
fn the_derived_index_is_cached_by_content_and_follows_an_edit() {
    let p = project("m1");
    let derived = p.path().join(".derived/behavior-index");
    let run = || {
        json(&orb(
            p.path(),
            &["behaviors", "catalog", "--json", "--no-vectors", "alpha"],
        ))
    };
    let cached = || -> Vec<String> {
        std::fs::read_dir(&derived)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.starts_with("alpha-") && n.ends_with(".json"))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(cached().is_empty());
    let first = run();
    let once = cached();
    assert_eq!(once.len(), 1, "one derived index per package: {once:?}");
    assert_eq!(run(), first, "a warm read answers the same");
    assert_eq!(cached(), once);
    // An edit to a source changes what is derived, and replaces the cached file.
    let card = p
        .path()
        .join("node_modules/alpha/behaviors/lolo/ui/core/atoms/alpha-card.lolo");
    write(&card, &CARD_LOLO.replace("palette", "both"));
    let edited = run();
    let card_of = |v: &serde_json::Value| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|e| e["behavior"] == "alpha/alpha-card")
            .unwrap()["exposure"]
            .clone()
    };
    assert_eq!(card_of(&first), "palette");
    assert_eq!(card_of(&edited), "both");
    let after = cached();
    assert_eq!(after.len(), 1);
    assert_ne!(after, once);
}

#[test]
fn a_lolo_header_tag_outside_the_vocabulary_is_a_named_error() {
    let p = project("m1");
    let card = p
        .path()
        .join("node_modules/alpha/behaviors/lolo/ui/core/atoms/alpha-card.lolo");
    write(&card, &CARD_LOLO.replace("palette", "sideways"));
    let out = orb(
        p.path(),
        &["behaviors", "catalog", "--json", "--no-vectors", "alpha"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("alpha/alpha-card") && err.contains("sideways"),
        "{err}"
    );
}

#[test]
fn catalog_refuses_mixed_embedding_models() {
    let p = project("m2");
    let out = orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha", "beta"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("ORB_VECTOR_MODEL_MISMATCH") && err.contains("m1") && err.contains("m2"),
        "{err}"
    );
    // Control: either package alone is fine.
    assert!(orb(p.path(), &["behaviors", "catalog", "--json", "beta"])
        .status
        .success());
    // Control: equal models across packages are fine.
    let same = project("m1");
    let v = json(&orb(
        same.path(),
        &["behaviors", "catalog", "--json", "alpha", "beta"],
    ));
    assert_eq!(v.as_array().unwrap().len(), 4);
}

#[test]
fn catalog_rejects_unknown_scope_and_exposure() {
    let p = project("m1");
    let out = orb(p.path(), &["behaviors", "catalog", "--json", "gamma/ui"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("gamma/ui"));
    let out = orb(
        p.path(),
        &[
            "behaviors",
            "catalog",
            "--json",
            "alpha",
            "--exposure",
            "nope",
        ],
    );
    assert!(!out.status.success());
}

#[test]
fn describe_returns_knobs_and_events_without_knob_vectors_by_default() {
    let p = project("m1");
    let v = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "alpha/alpha-card.traits.CardView",
        ],
    ));
    assert_eq!(
        v["value"],
        serde_json::json!({"behavior":"alpha/alpha-card","trait":"CardView"})
    );
    assert_eq!(v["model"], "");
    assert_eq!(
        v["events"],
        serde_json::json!({"emits":["CARD_OPENED"],"listens":["OPEN"],"transitions":["OPEN","INIT"],"domain":[]})
    );
    let knobs = v["knobs"].as_array().unwrap();
    assert_eq!(knobs.len(), 2);
    // Knob vectors are attached only on request: no knob carries a `vector` key.
    assert_eq!(knobs[0]["key"], "size");
    assert!(knobs[0].get("vector").is_none());
    assert_eq!(knobs[1]["key"], "title");
    assert_eq!(knobs[1]["type"], "string");
    assert_eq!(knobs[1]["default"], "Hello");
    assert_eq!(knobs[1]["tier"], "domain");
    assert!(knobs[1].get("vector").is_none());
    assert!(v["machine"]["stateMachine"]["transitions"].is_array());
    assert!(v["source"]["text"]
        .as_str()
        .unwrap()
        .starts_with("trait CardView"));
    let top: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(top.len(), 6, "strict key set, got {top:?}");
}

#[test]
fn describe_attaches_knob_vectors_only_with_the_flag_and_reads_them_from_the_store() {
    let p = project("m1");
    let spec = "alpha/alpha-card.traits.CardView";
    // The knob texts `orb` builds, stored as the user store would hold them after an embed.
    let texts = json(&orb(
        p.path(),
        &["behaviors", "texts", "--json", "alpha", "--kind", "knob"],
    ));
    let knob_texts: Vec<(String, String)> = texts
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["key"].as_str().unwrap().starts_with("alpha-card/"))
        .map(|t| {
            (
                t["key"].as_str().unwrap().to_string(),
                t["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(knob_texts.len(), 2, "{knob_texts:?}");
    let user = p.path().join(".user-vectors");
    VectorStore::insert_many(
        &user.join("m1.vec"),
        "m1",
        4,
        knob_texts
            .iter()
            .map(|(_, text)| (text_key("m1", text), quantize(&CARD_VECTOR)))
            .collect(),
    )
    .unwrap();

    let with = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", spec, "--knob-vectors"],
    ));
    assert_eq!(with["model"], "m1");
    for knob in with["knobs"].as_array().unwrap() {
        assert_close(&knob["vector"], &decoded(&CARD_VECTOR));
    }
    // Control: the same describe without the flag carries none.
    let without = json(&orb(p.path(), &["behaviors", "describe", "--json", spec]));
    assert!(without["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|k| k.get("vector").is_none()));
}

#[test]
fn a_text_with_no_vector_and_no_provider_is_an_error_naming_the_variables() {
    let p = project("m1");
    // Beta's user store is empty and its snapshot lacks the knob texts.
    let out = orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "beta/beta-card.traits.CardView",
            "--knob-vectors",
        ],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ORB_VECTOR_PROVIDER_MISSING"), "{err}");
    assert!(err.contains("OPEN_ROUTER_API_KEY"), "{err}");
    assert!(err.contains("beta-card/CardOrbital/CardView/"), "{err}");
    // Control: a catalog whose every text is in the snapshot needs no provider.
    assert!(orb(p.path(), &["behaviors", "catalog", "--json", "beta"])
        .status
        .success());
}

#[test]
fn describe_reads_a_trait_value_from_stdin_and_rejects_unknowns() {
    let p = project("m1");
    let mut child = Command::new(orb_bin())
        .args(["behaviors", "describe", "--json", "-"])
        .current_dir(p.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"behavior":"alpha/alpha-card","trait":"CardView"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(json(&out)["value"]["trait"], "CardView");

    for spec in [
        "alpha/alpha-card.traits.Nope",
        "alpha/missing.traits.CardView",
        "alpha/missing",
    ] {
        let out = orb(p.path(), &["behaviors", "describe", "--json", spec]);
        assert!(!out.status.success(), "{spec} must fail");
    }
}

#[test]
fn source_prints_the_resolved_orb_and_accepts_workspace_relative_specs() {
    let p = project("m1");
    let v = json(&orb(
        p.path(),
        &["behaviors", "source", "--json", "alpha/alpha-card"],
    ));
    assert_eq!(v["name"], "card");
    write(
        &p.path().join("orbitals/mine.orb"),
        r#"{"name":"mine","orbitals":[]}"#,
    );
    let v = json(&orb(
        p.path(),
        &["behaviors", "source", "--json", "./orbitals/mine"],
    ));
    assert_eq!(v["name"], "mine");
    let out = orb(
        p.path(),
        &["behaviors", "source", "--json", "alpha/missing"],
    );
    assert!(!out.status.success());
}

#[test]
fn describe_derives_a_workspace_relative_trait_from_its_schema() {
    let p = project("m1");
    let schema = r#"{"name":"mine","orbitals":[{"name":"O","entity":{"name":"E","fields":[]},"traits":[{"name":"T","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"GO"}]},"emits":[{"event":"DONE"}],"config":{"title":{"type":"string","default":"x"}}}]}]}"#;
    write(&p.path().join("orbitals/mine.orb"), schema);
    let v = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/mine.traits.T",
        ],
    ));
    assert_eq!(v["value"]["behavior"], "./orbitals/mine");
    assert_eq!(v["knobs"][0]["key"], "title");
    assert_eq!(v["events"]["emits"], serde_json::json!(["DONE"]));
    assert_eq!(v["events"]["transitions"], serde_json::json!(["GO"]));
}

#[test]
fn describe_a_whole_behavior_and_one_orbital_from_the_resolved_program() {
    let p = project("m1");
    let schema = r#"{"name":"shop","config":{"appName":{"type":"string","default":"Shop"}},"orbitals":[{"name":"ItemOrbital","entity":{"name":"Item","fields":[{"name":"id","type":"string","required":true},{"name":"tone","type":"enum","values":["warm","cool"]},{"name":"note","type":"relation","relation":{"entity":"Note","cardinality":"one"}}]},"traits":[{"name":"ItemList","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"OPEN"}]},"config":{"layout":{"type":"string","values":["grid","rows"],"default":"grid"}}}],"pages":[{"name":"ItemsPage","path":"/items","traits":[{"ref":"ItemList"}]}]},{"name":"NoteOrbital","entity":{"name":"Note","fields":[{"name":"id","type":"string","required":true}]},"traits":[]}]}"#;
    write(&p.path().join("orbitals/shop.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/shop"],
    ));
    assert_eq!(v["behavior"], "./orbitals/shop");
    assert_eq!(v["config"]["appName"]["default"], "Shop");
    assert_eq!(v["orbitals"].as_array().unwrap().len(), 2);
    let item = &v["orbitals"][0];
    assert_eq!(
        item["value"],
        serde_json::json!({"behavior": "./orbitals/shop", "orbital": "ItemOrbital"})
    );
    assert_eq!(item["entity"]["name"], "Item");
    assert_eq!(
        item["entity"]["fields"][1]["values"],
        serde_json::json!(["warm", "cool"])
    );
    assert_eq!(item["traits"][0]["trait"], "ItemList");
    assert_eq!(
        item["traits"][0]["config"]["layout"]["values"],
        serde_json::json!(["grid", "rows"])
    );
    assert_eq!(
        item["traits"][0]["knobs"],
        serde_json::json!([{"name": "layout", "type": "string", "values": ["grid", "rows"], "default": "grid"}])
    );
    assert_eq!(
        v["knobs"],
        serde_json::json!([{"name": "appName", "type": "string", "default": "Shop"}])
    );
    assert_eq!(
        item["traits"][0]["events"]["transitions"],
        serde_json::json!(["OPEN"])
    );
    assert_eq!(
        item["pages"],
        serde_json::json!([{"name": "ItemsPage", "path": "/items", "traits": ["ItemList"]}])
    );

    let one = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/shop.orbitals.NoteOrbital",
        ],
    ));
    assert_eq!(one["orbitals"].as_array().unwrap().len(), 1);
    assert_eq!(one["orbitals"][0]["value"]["orbital"], "NoteOrbital");

    let mut child = Command::new(orb_bin())
        .args(["behaviors", "describe", "--json", "-"])
        .current_dir(p.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        child.stdin.as_mut().unwrap(),
        br#"{"behavior":"./orbitals/shop","orbital":"ItemOrbital","config":{"x":1}}"#,
    )
    .unwrap();
    let piped = json(&child.wait_with_output().unwrap());
    assert_eq!(piped["orbitals"][0]["value"]["orbital"], "ItemOrbital");

    let missing = orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/shop.orbitals.Nope",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("has no orbital `Nope`"));
}

#[test]
fn describe_knobs_carry_their_default_and_the_events_an_event_knob_may_name() {
    let p = project("m1");
    let schema = r#"{"name":"desk","config":{"title":{"type":"string","default":"Desk"},"links":{"type":"[Link]","items":{"type":"object","properties":{"label":{"name":"label","type":"string","required":true},"href":{"name":"href","type":"string"}}},"default":[{"label":"Home","href":"/"}]}},"orbitals":[{"name":"TaskOrbital","entity":{"name":"Task","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"TaskList","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"OPEN"}]},"emits":[{"event":"TASK_PICKED"}],"config":{"onPick":{"type":"event","default":"OPEN"},"actions":{"type":"[Action]","items":{"type":"object","properties":{"label":{"name":"label","type":"string","required":true},"event":{"name":"event","type":"event","required":true}}},"default":[{"label":"Close","event":"CLOSE"}]},"plain":{"type":"string"}}},{"name":"TaskSide","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"CLOSE"}]}}],"pages":[{"name":"TasksPage","path":"/tasks","traits":[{"ref":"TaskList"},{"ref":"TaskSide"}]}]},{"name":"NoteOrbital","entity":{"name":"Note","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"NoteList","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"NOTE_ONLY"}]}}]}]}"#;
    write(&p.path().join("orbitals/desk.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/desk"],
    ));
    let knob = |knobs: &serde_json::Value, name: &str| {
        knobs
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no knob {name} in {knobs}"))
    };
    assert_eq!(knob(&v["knobs"], "title")["default"], "Desk");
    assert_eq!(
        knob(&v["knobs"], "links")["default"],
        serde_json::json!([{"label": "Home", "href": "/"}])
    );
    let list = &v["orbitals"][0]["traits"][0]["knobs"];
    let on_pick = knob(list, "onPick");
    assert_eq!(on_pick["default"], "OPEN");
    assert_eq!(
        on_pick["values"],
        serde_json::json!(["OPEN", "TASK_PICKED"])
    );
    let actions = knob(list, "actions");
    let member = actions["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "event")
        .cloned()
        .unwrap();
    assert_eq!(
        member["values"],
        serde_json::json!(["CLOSE", "OPEN", "TASK_PICKED"])
    );
    let plain = knob(list, "plain");
    assert!(
        plain.get("default").is_none(),
        "no default declared: {plain}"
    );
    assert!(plain.get("values").is_none(), "not an event knob: {plain}");
}

#[test]
fn describe_reports_each_orbitals_derived_expectations() {
    let p = project("m1");
    let schema = r#"{"name":"shop","orbitals":[{"name":"OrderOrbital","entity":{"name":"Order","fields":[{"name":"id","type":"string","required":true},{"name":"total","type":"number"}]},"traits":[{"name":"OrderFeed","linkedEntity":"Order","emits":[{"event":"ORDER_PLACED"}],"stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"OPEN"}]}}],"pages":[{"name":"OrdersPage","path":"/orders/:id","traits":[{"ref":"OrderFeed"}]}]},{"name":"DeskOrbital","entity":{"name":"Desk","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"DeskBoard","linkedEntity":"Desk","listens":[{"event":"ORDER_PLACED","source":{"kind":"orbital","orbital":"OrderOrbital","trait":"OrderFeed"},"triggers":"OPEN"}],"stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"OPEN","effects":[["persist","update","Order",{"total":1}],["navigate",["str/concat","/orders/","@payload.id"]]]}]}}],"pages":[{"name":"DeskPage","path":"/desk","traits":[{"ref":"DeskBoard"}]}]}]}"#;
    write(&p.path().join("orbitals/shop.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/shop"],
    ));
    let desk = v["orbitals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["value"]["orbital"] == "DeskOrbital")
        .unwrap();
    assert_eq!(
        desk["expects"],
        serde_json::json!([
            { "kind": "page", "path": "/orders/:id" },
            { "kind": "entity", "name": "Order", "shape": [{ "name": "total", "type": "number" }] },
            { "kind": "event", "traitName": "OrderFeed", "event": "ORDER_PLACED" }
        ])
    );
    let order = v["orbitals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["value"]["orbital"] == "OrderOrbital")
        .unwrap();
    assert_eq!(
        order["expects"],
        serde_json::json!([]),
        "control: the provider expects nothing"
    );
}

#[test]
fn expects_derives_every_orbital_of_the_organism_as_written() {
    let p = project("m1");
    let schema = r#"{"name":"shop","orbitals":[{"name":"OrderOrbital","entity":{"name":"Order","fields":[{"name":"id","type":"string","required":true},{"name":"total","type":"number"}]},"traits":[],"pages":[]},{"name":"DeskOrbital","entity":{"name":"Desk","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"DeskBoard","linkedEntity":"Desk","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"OPEN","effects":[["persist","update","Order",{"total":1}]]}]}}],"pages":[]}]}"#;
    write(&p.path().join("orbitals/shop.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "expects", "--json", "./orbitals/shop"],
    ));
    assert_eq!(
        v["DeskOrbital"]["expectations"],
        serde_json::json!([{ "kind": "entity", "name": "Order", "shape": [{ "name": "total", "type": "number" }] }])
    );
    assert_eq!(v["DeskOrbital"]["diagnostics"], serde_json::json!([]));
    assert_eq!(
        v["OrderOrbital"]["expectations"],
        serde_json::json!([]),
        "control: the provider expects nothing"
    );
    let path = p.path().join("orbitals/shop.orb");
    let by_path = json(&orb(
        p.path(),
        &["behaviors", "expects", "--json", path.to_str().unwrap()],
    ));
    assert_eq!(by_path, v, "a .orb path reads the same program");
}

/// Smoke against the real monorepo std package, answered from its own `orb.vectors` snapshot with an
/// empty user store; skipped in a standalone checkout or before the package declares a snapshot.
#[test]
fn smoke_real_std_package() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let std_dir = root.join("packages/almadar-std");
    let declares_vectors = std::fs::read_to_string(std_dir.join("package.json"))
        .map(|p| p.contains("\"vectors\""))
        .unwrap_or(false);
    if !std_dir.join("dist/vectors.vec").is_file() || !declares_vectors {
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let real = |args: &[&str]| {
        Command::new(orb_bin())
            .args(args)
            .current_dir(&root)
            .env("ALMADAR_VECTORS_DIR", store.path())
            .env("ALMADAR_DERIVED_CACHE", store.path())
            .env_remove("EMBEDDING_PROVIDER")
            .env_remove("OPEN_ROUTER_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("EMBEDDING_API_KEY")
            .output()
            .expect("spawn orb")
    };
    let v = json(&real(&[
        "behaviors",
        "catalog",
        "--json",
        "std/ui/core/molecules",
    ]));
    let entries = v.as_array().unwrap();
    assert!(!entries.is_empty());
    let first = &entries[0];
    assert!(first["behavior"]
        .as_str()
        .unwrap()
        .starts_with("almadar-std/"));
    assert_eq!(
        first["vectors"]["description"].as_array().unwrap().len(),
        768
    );
    let name = first["traits"][0]["trait"].as_str().unwrap().to_string();
    let spec = format!("{}.traits.{}", first["behavior"].as_str().unwrap(), name);
    let d = json(&real(&["behaviors", "describe", "--json", &spec]));
    assert_eq!(d["value"]["trait"], name.as_str());
    assert!(d["events"]["transitions"].is_array());
}

#[test]
fn snapshot_writes_exactly_the_package_texts_deterministically_and_needs_a_provider_for_misses() {
    let p = project("m1");
    let root = p.path();
    let pkg = root.join("node_modules/alpha");
    let out = root.join("alpha.vec");
    let snapshot = || {
        orb(
            root,
            &[
                "behaviors",
                "vectors",
                "snapshot",
                "--package",
                pkg.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        )
    };
    // Control: no stored vector and no provider fails and writes nothing.
    let failed = snapshot();
    assert!(
        !failed.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("ORB_VECTOR_PROVIDER_MISSING"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(!out.exists());
    // Seed the user store with the package's own organism + capability texts.
    let mut records = Vec::new();
    for kind in ["organism", "capability"] {
        let texts = json(&orb(
            root,
            &["behaviors", "texts", "--json", "--kind", kind, "alpha"],
        ));
        for t in texts.as_array().unwrap() {
            records.push((
                text_key(CANON, t["text"].as_str().unwrap()),
                quantize(&CARD_VECTOR),
            ));
        }
    }
    let wanted = records.len();
    assert!(wanted >= 3, "two organisms and one capability text");
    VectorStore::insert_many(
        &root.join(".user-vectors/baai_bge-base-en-v1.5.vec"),
        CANON,
        4,
        records,
    )
    .unwrap();
    // A vector the package does not want stays out of the snapshot.
    VectorStore::insert_many(
        &root.join(".user-vectors/baai_bge-base-en-v1.5.vec"),
        CANON,
        4,
        vec![(text_key(CANON, "unrelated"), quantize(&CAPABILITY_VECTOR))],
    )
    .unwrap();
    let ok = snapshot();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let first = std::fs::read(&out).unwrap();
    let store = VectorStore::open(&out).unwrap();
    assert_eq!(store.len(), wanted);
    assert!(store.get(&text_key(CANON, "unrelated")).is_none());
    assert!(snapshot().status.success());
    assert_eq!(
        std::fs::read(&out).unwrap(),
        first,
        "same inputs, same bytes"
    );
}

#[test]
fn snapshot_reuses_locked_package_vectors_with_warm_cache_and_no_provider() {
    use sha2::{Digest, Sha256};

    const MODEL: &str = "m1";
    let p = project(MODEL);
    let root = p.path();
    let pkg = root.join("node_modules/alpha");
    let out = root.join("locked-alpha.vec");
    let user = root.join(".user-vectors");
    let cache = root.join(".snapshot-sidecars");
    let mut records = Vec::new();
    for kind in ["organism", "capability"] {
        let texts = json(&orb(
            root,
            &["behaviors", "texts", "--json", "--kind", kind, "alpha"],
        ));
        for text in texts.as_array().unwrap() {
            records.push((
                text_key(MODEL, text["text"].as_str().unwrap()),
                quantize(&CARD_VECTOR),
            ));
        }
    }
    let expected = VectorStore::from_records(MODEL, 4, records.clone()).unwrap();
    assert!(expected.len() >= 3, "two organisms and one capability text");
    records.push((
        text_key(MODEL, "unrelated locked text"),
        quantize(&CAPABILITY_VECTOR),
    ));
    let oracle = root.join("lock-oracle.vec");
    VectorStore::from_records(MODEL, 4, records)
        .unwrap()
        .write(&oracle)
        .unwrap();
    let body = std::fs::read(&oracle).unwrap();
    let sha: String = Sha256::digest(&body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    std::fs::create_dir_all(&cache).unwrap();
    let cached = cache.join(&sha);
    std::fs::write(&cached, &body).unwrap();
    write(
        &pkg.join("sidecars.lock.json"),
        &serde_json::json!({
            "version": 1,
            "files": {
                "dist/vectors.vec": {
                    "sha256": sha,
                    "bytes": body.len(),
                    "url": "https://storage.example.invalid/vectors?token=LOCK_FIXTURE_TOKEN"
                }
            }
        })
        .to_string(),
    );
    std::fs::remove_file(pkg.join("dist/vectors.vec")).unwrap();
    assert!(!pkg.join("dist/vectors.vec").exists());
    assert!(!out.exists());
    assert!(!user.exists());
    let snapshot = || {
        Command::new(orb_bin())
            .args([
                "behaviors",
                "vectors",
                "snapshot",
                "--package",
                pkg.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ])
            .current_dir(root)
            .env("ORB_NO_AUTO_SETUP", "1")
            .env("ALMADAR_VECTORS_DIR", &user)
            .env("ALMADAR_DERIVED_CACHE", root.join(".derived"))
            .env("ALMADAR_SIDECAR_CACHE", &cache)
            .env_remove("EMBEDDING_PROVIDER")
            .env_remove("EMBEDDING_API_KEY")
            .env_remove("OPEN_ROUTER_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("EMBEDDING_MODEL")
            .output()
            .expect("spawn orb")
    };
    let result = snapshot();
    assert!(
        result.status.success(),
        "the declared locked snapshot must answer without a provider: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let actual = VectorStore::open(&out).unwrap();
    assert_eq!(actual.model(), MODEL);
    assert_eq!(actual.dims(), 4);
    assert_eq!(
        actual.iter().collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    let first = std::fs::read(&out).unwrap();
    assert!(snapshot().status.success());
    assert_eq!(std::fs::read(&out).unwrap(), first);
    assert_eq!(std::fs::read(&cached).unwrap(), body);
    assert!(!pkg.join("dist/vectors.vec").exists());
    assert!(!user.exists());
}

#[test]
fn describe_marks_which_traits_another_record_may_rebind() {
    let p = project("m1");
    let schema = r#"{"name":"kit","orbitals":[{"name":"RowOrbital","entity":{"name":"Row","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"RowBrowse","linkedEntity":"Row","entityRebindable":true,"stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}},{"name":"RowFixed","linkedEntity":"Row","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}}],"pages":[{"name":"RowsPage","path":"/rows","traits":[{"ref":"RowBrowse"}]}]}]}"#;
    write(&p.path().join("orbitals/kit.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/kit"],
    ));
    let traits = &v["orbitals"][0]["traits"];
    assert_eq!(traits[0]["trait"], "RowBrowse");
    assert_eq!(traits[0]["rebindable"], true);
    assert_eq!(traits[1]["trait"], "RowFixed");
    assert_eq!(traits[1]["rebindable"], false);
    assert_eq!(
        v["orbitals"][0]["pages"],
        serde_json::json!([{"name": "RowsPage", "path": "/rows", "traits": ["RowBrowse"]}])
    );
}

#[test]
fn describe_reports_each_pages_menu_label_and_icon() {
    let p = project("m1");
    let schema = r#"{"name":"menu","orbitals":[{"name":"RowOrbital","entity":{"name":"Row","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"RowView","linkedEntity":"Row","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}}],"pages":[{"name":"RowsPage","path":"/rows","label":"Rows","icon":"list","traits":[{"ref":"RowView"}]},{"name":"RowDetailPage","path":"/rows/:id","traits":[{"ref":"RowView"}]}]}]}"#;
    write(&p.path().join("orbitals/menu.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/menu"],
    ));
    let pages = &v["orbitals"][0]["pages"];
    assert_eq!(pages[0]["label"], "Rows");
    assert_eq!(pages[0]["icon"], "list");
    assert!(pages[1].get("label").is_none(), "{pages}");
    assert!(pages[1].get("icon").is_none(), "{pages}");
}

#[test]
fn describe_marks_a_knob_whose_value_forwards_another_knob() {
    let p = project("m1");
    let schema = r#"{"name":"fwd","config":{"title":{"type":"string","default":"Rows"}},"orbitals":[{"name":"RowOrbital","entity":{"name":"Row","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"RowView","linkedEntity":"Row","config":{"heading":{"type":"string","default":"@config.title"},"note":{"type":"string","default":"plain"}},"stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}}],"pages":[{"name":"RowsPage","path":"/rows","traits":[{"ref":"RowView"}]}]}]}"#;
    write(&p.path().join("orbitals/fwd.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/fwd"],
    ));
    let knobs = v["orbitals"][0]["traits"][0]["knobs"]
        .as_array()
        .expect("knobs")
        .clone();
    let knob = |n: &str| knobs.iter().find(|k| k["name"] == n).cloned().expect(n);
    assert_eq!(knob("heading")["default"], "Rows");
    assert_eq!(knob("heading")["forwardedFrom"], "@config.title");
    assert!(knob("note").get("forwardedFrom").is_none(), "{knobs:?}");
}

#[test]
fn describe_carries_each_orbitals_forwarded_controls() {
    let p = project("m1");
    let schema = r#"{"name":"plain","orbitals":[{"name":"RowOrbital","entity":{"name":"Row","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"RowView","linkedEntity":"Row","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}}],"pages":[{"name":"RowsPage","path":"/rows","traits":[{"ref":"RowView"}]}]}]}"#;
    write(&p.path().join("orbitals/plain.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/plain"],
    ));
    assert_eq!(v["orbitals"][0]["forwards"], serde_json::json!([]));
}

#[test]
fn describe_lists_each_traits_domain_events() {
    let p = project("m1");
    let schema = r#"{"name":"club","orbitals":[{"name":"VoteOrbital","entity":{"name":"Vote","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"VoteBoard","linkedEntity":"Vote","emits":[{"event":"VOTED","tier":"domain"}],"listens":[{"event":"VOTE","tier":"domain"},{"event":"LOADED","tier":"internal"},{"event":"BARE"}],"stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"},{"from":"s","to":"s","event":"VOTE"},{"from":"s","to":"s","event":"LOADED"},{"from":"s","to":"s","event":"BARE"}]}}],"pages":[{"name":"VotesPage","path":"/votes","traits":[{"ref":"VoteBoard"}]}]}]}"#;
    write(&p.path().join("orbitals/club.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/club"],
    ));
    assert_eq!(
        v["orbitals"][0]["traits"][0]["events"]["domain"],
        serde_json::json!(["VOTED", "VOTE"])
    );
}

#[test]
fn describe_reports_each_record_persistence() {
    let p = project("m1");
    let schema = r#"{"name":"kinds","orbitals":[{"name":"PetOrbital","entity":{"name":"Pet","persistence":"persistent","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]},{"name":"NoteOrbital","entity":{"name":"Note","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]},{"name":"VisitView","entity":{"name":"PageVisit","persistence":"runtime","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]}]}"#;
    write(&p.path().join("orbitals/kinds.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/kinds"],
    ));
    assert_eq!(v["orbitals"][0]["entity"]["persistence"], "persistent");
    assert_eq!(v["orbitals"][1]["entity"]["persistence"], "persistent");
    assert_eq!(v["orbitals"][2]["entity"]["persistence"], "runtime");
}

#[test]
fn describe_marks_the_identity_record() {
    let p = project("m1");
    let schema = r#"{"name":"people","orbitals":[{"name":"PersonOrbital","entity":{"name":"Person","persistence":"persistent","identity":true,"fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]},{"name":"NoteOrbital","entity":{"name":"Note","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]}]}"#;
    write(&p.path().join("orbitals/people.orb"), schema);
    let v = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/people"],
    ));
    assert_eq!(v["orbitals"][0]["entity"]["identity"], true);
    assert_eq!(v["orbitals"][1]["entity"]["identity"], false);
}

const DOOR_ORB: &str = r#"{"name":"door","orbitals":[{"name":"DoorOrbital","entity":{"name":"Door","persistence":"runtime","fields":[{"name":"open","type":"boolean","default":false}]},"traits":[{"name":"DoorLife","linkedEntity":"Door","stateMachine":{"states":[{"name":"closed","isInitial":true},{"name":"opened"}],"events":[{"key":"INIT","name":"Initialize"},{"key":"OPEN","name":"Open"},{"key":"CLOSE","name":"Close"}],"transitions":[{"from":"closed","to":"closed","event":"INIT"},{"from":"closed","to":"opened","event":"OPEN","guard":["not","@entity.open"],"effects":[["set","@entity.open",true]]},{"from":"opened","to":"closed","event":"CLOSE","effects":[["set","@entity.open",false]]}]}}]}]}"#;

const PORCH_ORB: &str = r#"{"name":"porch","orbitals":[{"name":"PorchOrbital","uses":[{"from":"./door","as":"Door"}],"entity":{"name":"Porch","persistence":"runtime","fields":[]},"traits":[{"ref":"Door.traits.DoorLife","name":"FrontDoor","events":{"OPEN":"UNLOCK"}}]}]}"#;

const TAGGED_DOOR_ORB: &str = r#"{"name":"door","orbitals":[{"name":"DoorOrbital","entity":{"name":"Door","persistence":"runtime","fields":[{"name":"open","type":"boolean","default":false}]},"traits":[{"name":"DoorLife","linkedEntity":"Door","capabilities":["hinged","lockable"],"stateMachine":{"states":[{"name":"closed","isInitial":true}],"transitions":[{"from":"closed","to":"closed","event":"INIT"}]}}]}]}"#;

#[test]
fn describe_reports_each_traits_composed_ref_and_capabilities() {
    let p = project("m1");
    write(&p.path().join("orbitals/door.orb"), TAGGED_DOOR_ORB);
    write(&p.path().join("orbitals/porch.orb"), PORCH_ORB);

    let porch = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/porch"],
    ));
    let front = &porch["orbitals"][0]["traits"][0];
    assert_eq!(front["trait"], "FrontDoor");
    assert_eq!(front["ref"], "Door.traits.DoorLife");
    assert_eq!(
        front["capabilities"],
        serde_json::json!(["hinged", "lockable"])
    );

    // Control: the atom defines its own trait, so it composes nothing and keeps its tags.
    let door = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/door"],
    ));
    let life = &door["orbitals"][0]["traits"][0];
    assert!(life.get("ref").is_none(), "{life}");
    assert_eq!(
        life["capabilities"],
        serde_json::json!(["hinged", "lockable"])
    );
}

const LIB_ORB: &str = r#"{"name":"lib","orbitals":[{"name":"ItemOrbital","entity":{"name":"Item","fields":[{"name":"id","type":"string","required":true},{"name":"note","type":"relation","relation":{"entity":"Note","cardinality":"one"}}]},"traits":[],"pages":[]},{"name":"NoteOrbital","entity":{"name":"Note","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]}]}"#;

const SHELF_ORB: &str = r#"{"name":"shelf","orbitals":[{"name":"ShelfItem","uses":[{"from":"./lib","as":"Lib"}],"entity":"Lib.orbitals.ItemOrbital.entity","traits":[],"pages":[],"reference":{"ref":"Lib.orbitals.ItemOrbital"}}]}"#;

#[test]
fn describe_a_behavior_whose_own_imports_leave_a_record_unmapped() {
    let p = project("m1");
    write(&p.path().join("orbitals/lib.orb"), LIB_ORB);
    write(&p.path().join("orbitals/shelf.orb"), SHELF_ORB);
    let shelf = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/shelf"],
    ));
    assert_eq!(shelf["orbitals"][0]["value"]["orbital"], "ShelfItem");

    // Control: the library itself still names what an import of its item must map.
    let lib = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/lib"],
    ));
    assert_eq!(lib["orbitals"][0]["borrows"], serde_json::json!(["Note"]));
}

const THING_ORB: &str = r#"{"name":"thing","orbitals":[{"name":"ThingOrbital","entity":{"name":"Thing","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"ThingView","linkedEntity":"Thing","stateMachine":{"states":[{"name":"s","isInitial":true}],"transitions":[{"from":"s","to":"s","event":"INIT"}]}}],"pages":[{"name":"ThingPage","path":"/things","traits":[{"ref":"ThingView"}]}]}]}"#;

const HUB_ORB: &str = r#"{"name":"hub","orbitals":[{"name":"HubOrbital","entity":{"name":"Hub","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]},{"name":"LazyThing","uses":[{"from":"./thing","as":"Thing","lazy":true}],"entity":"Thing.orbitals.ThingOrbital.entity","traits":[],"pages":[],"reference":{"ref":"Thing.orbitals.ThingOrbital"}}]}"#;

#[test]
fn describe_a_behavior_that_uses_another_lazily() {
    let p = project("m1");
    write(&p.path().join("orbitals/thing.orb"), THING_ORB);
    write(&p.path().join("orbitals/hub.orb"), HUB_ORB);
    let hub = json(&orb(
        p.path(),
        &["behaviors", "describe", "--json", "./orbitals/hub"],
    ));
    let orbitals = hub["orbitals"].as_array().unwrap();
    assert_eq!(
        orbitals.len(),
        1,
        "the lazy orbital loads on its own, not here"
    );
    assert_eq!(orbitals[0]["value"]["orbital"], "HubOrbital");
    assert_eq!(orbitals[0]["borrows"], serde_json::json!([]));
}

#[test]
fn describe_carries_the_traits_state_machine_as_the_program_runs_it() {
    let p = project("m1");
    write(&p.path().join("orbitals/door.orb"), DOOR_ORB);
    write(&p.path().join("orbitals/porch.orb"), PORCH_ORB);

    // A composed trait: its machine is the atom's, with the call site's rename applied.
    let v = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/porch.traits.FrontDoor",
        ],
    ));
    assert_eq!(v["machine"]["linkedEntity"], "Door");
    let transitions = v["machine"]["stateMachine"]["transitions"]
        .as_array()
        .unwrap();
    let unlock = transitions
        .iter()
        .find(|t| t["event"] == "UNLOCK")
        .expect("the renamed OPEN arm");
    assert_eq!(unlock["from"], "closed");
    assert_eq!(unlock["to"], "opened");
    assert_eq!(unlock["guard"], serde_json::json!(["not", "@entity.open"]));
    assert_eq!(
        unlock["effects"][0],
        serde_json::json!(["set", "@entity.open", true])
    );
    assert!(transitions.iter().all(|t| t["event"] != "OPEN"));
    assert_eq!(
        v["events"]["transitions"],
        serde_json::json!(["INIT", "UNLOCK", "CLOSE"])
    );

    // Its printed source locates each transition by its index in the machine.
    let unlock_index = transitions
        .iter()
        .position(|t| t["event"] == "UNLOCK")
        .unwrap();
    let text = v["source"]["text"].as_str().expect("printed source");
    assert!(text.starts_with("trait FrontDoor"), "{text}");
    let at = v["source"]["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["index"] == unlock_index)
        .expect("the UNLOCK arm is located");
    let first = at["start"].as_u64().unwrap() as usize;
    let line = text.lines().nth(first - 1).unwrap();
    assert!(
        line.trim_start()
            .starts_with("UNLOCK -> opened when (not @entity.open)"),
        "{line}"
    );

    // Control: the atom's own trait keeps OPEN.
    let atom = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/door.traits.DoorLife",
        ],
    ));
    let names: Vec<&str> = atom["machine"]["stateMachine"]["states"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, ["closed", "opened"]);
    assert!(atom["machine"]["stateMachine"]["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["event"] == "OPEN"));
}

const DIAGRAM_ORB: &str = r#"{"name":"diagram","orbitals":[{"name":"DiagramOrbital","entity":{"name":"View","persistence":"runtime","fields":[]},"traits":[{"name":"Diagram","linkedEntity":"View","stateMachine":{"states":[{"name":"loading","isInitial":true},{"name":"shown"}],"events":[{"key":"INIT","name":"Initialize"},{"key":"DESCRIBED","name":"Described"}],"transitions":[{"from":"loading","to":"loading","event":"INIT","effects":[["behavior/describe",{"behavior":"./door","trait":"DoorLife"},{"emit":{"success":"DESCRIBED","failure":"DESCRIBE_FAILED"}}],["behavior/describe","@entity.picked",{"emit":{"success":"DESCRIBED"}}]]},{"from":"loading","to":"shown","event":"DESCRIBED"}]}}]}]}"#;

#[test]
fn a_literal_describe_is_answered_when_the_program_is_resolved() {
    let p = project("m1");
    write(&p.path().join("orbitals/door.orb"), DOOR_ORB);
    write(&p.path().join("orbitals/diagram.orb"), DIAGRAM_ORB);
    let resolved = json(&orb(p.path(), &["resolve", "orbitals/diagram.orb"]));
    let effects =
        &resolved["orbitals"][0]["traits"][0]["stateMachine"]["transitions"][0]["effects"];
    assert_eq!(effects[0][0], "emit");
    assert_eq!(effects[0][1], "DESCRIBED");
    let quoted = effects[0][2]["result"].as_array().expect("a quote call");
    assert_eq!(quoted[0], "quote");
    let mut answer: serde_json::Value = serde_json::from_str(quoted[1].as_str().unwrap()).unwrap();
    let mut direct = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "./orbitals/door.traits.DoorLife",
        ],
    ));
    // Each answer echoes the value as its caller spelled it: the program names the file relative to itself.
    assert_eq!(
        answer["value"],
        serde_json::json!({"behavior":"./door","trait":"DoorLife"})
    );
    answer.as_object_mut().unwrap().remove("value");
    direct.as_object_mut().unwrap().remove("value");
    assert_eq!(
        answer, direct,
        "the build answers exactly what the host would"
    );
    // Control: a value known only at runtime is still the host's to answer.
    assert_eq!(effects[1][0], "behavior/describe");
}

#[test]
fn a_registered_trait_missing_from_stale_signatures_is_described_from_its_program() {
    // `beta` ships an empty (stale) signatures file; its card is still installed and resolves.
    let p = project("m1");
    let v = json(&orb(
        p.path(),
        &[
            "behaviors",
            "describe",
            "--json",
            "beta/beta-card.traits.CardView",
        ],
    ));
    let keys: Vec<&str> = v["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|k| k["key"].as_str())
        .collect();
    assert_eq!(keys, ["size", "title"]);
    assert_eq!(v["events"]["emits"], serde_json::json!(["CARD_OPENED"]));
    assert!(v["machine"]["stateMachine"]["transitions"].is_array());
}
