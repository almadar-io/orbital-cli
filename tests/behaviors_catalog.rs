// `orb behaviors catalog | describe | source` — drives the real `orb` binary
// against a temp project holding two tiny fake behavior packages with known
// embedding bytes, so the decode math, scope/exposure filtering, the
// mixed-model refusal and the contract shapes are asserted exactly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

/// base64 of the signed bytes [127, -1, -128, 10] — scale 1.27 decodes to
/// [1.27, -0.01, -1.28, 0.1].
const QUANTIZED: &str = "f/+ACg==";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const ORB: &str = r#"{"name":"fake","orbitals":[{"name":"FakeOrbital","entity":{"name":"Thing","fields":[]},"traits":[]}]}"#;

fn package(
    root: &Path,
    dir: &str,
    prefix: &str,
    model: &str,
    behaviors: &str,
    extra: &[(&str, &str)],
) {
    let pkg = root.join("node_modules").join(dir);
    write(
        &pkg.join("package.json"),
        &format!(
            r#"{{"name":"{dir}","version":"1.0.0","orb":{{"prefix":"{prefix}","registry":"behaviors/registry","index":"behaviors/index.json","factorySignatures":"behaviors/registry/factory-signatures.json","loloContracts":"behaviors/contracts.json","signatures":"behaviors/signatures.lolo","embeddings":["dist/embeddings.json","dist/knobs.json"],"compiler":">=0.0.0"}}}}"#
        ),
    );
    write(&pkg.join("behaviors/index.json"), behaviors);
    write(&pkg.join("behaviors/contracts.json"), "{}");
    write(&pkg.join("behaviors/signatures.lolo"), "");
    write(
        &pkg.join("dist/embeddings.json"),
        &format!(
            r#"{{"version":"1","model":"{model}","dimensions":4,"encoding":"int8-b64","vectors":{{"{prefix}-card":{{"s":1.27,"d":"{QUANTIZED}"}},"{prefix}-card#capabilities":[0.5,0.25]}}}}"#
        ),
    );
    write(
        &pkg.join("dist/knobs.json"),
        &format!(
            r#"{{"version":"1","model":"{model}","dimensions":4,"encoding":"int8-b64","table":[{{"s":1.27,"d":"{QUANTIZED}"}}],"keys":{{"{prefix}-card/CardOrbital/CardView/title":0}}}}"#
        ),
    );
    for (rel, content) in extra {
        write(&pkg.join(rel), content);
    }
}

fn index(prefix: &str) -> String {
    format!(
        r#"{{"behaviors":{{
 "{prefix}-card":{{"name":"{prefix}-card","level":"atom","topic":"ui/core","exposure":"palette","description":"A card.","capabilities":["display"],"traits":[{{"name":"CardView"}}]}},
 "{prefix}-board":{{"name":"{prefix}-board","level":"organism","topic":"app","description":"A board.","traits":[{{"name":"BoardMain"}},{{"name":"BoardSide"}}]}}
}}}}"#
    )
}

const SIGNATURES: &str = r#"{"generatedFromStdVersion":"1","knobDefs":[{"key":"title","type":"string","default":"Hello","label":"Title","tier":"domain"},{"key":"size","type":"number","default":3}],"signatures":[{"organism":"alpha-card","orbital":"CardOrbital","traits":[{"name":"CardView","emittedEvents":["CARD_OPENED"],"listenedEvents":["OPEN"],"transitionEvents":["OPEN","INIT"],"overridableConfigKeyRefs":[0,1]}]}]}"#;

fn project(second_model: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        r#"{"name":"catalog-test","private":true}"#,
    );
    package(
        dir.path(),
        "alpha",
        "alpha",
        "m1",
        &index("alpha"),
        &[
            ("behaviors/registry/ui/core/atoms/alpha-card.orb", ORB),
            ("behaviors/registry/app/organisms/alpha-board.orb", ORB),
            ("behaviors/registry/factory-signatures.json", SIGNATURES),
        ],
    );
    package(
        dir.path(),
        "beta",
        "beta",
        second_model,
        &index("beta"),
        &[
            ("behaviors/registry/ui/core/atoms/beta-card.orb", ORB),
            ("behaviors/registry/app/organisms/beta-board.orb", ORB),
            (
                "behaviors/registry/factory-signatures.json",
                r#"{"signatures":[]}"#,
            ),
        ],
    );
    dir
}

fn orb(dir: &Path, args: &[&str]) -> Output {
    Command::new(orb_bin())
        .args(args)
        .current_dir(dir)
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
    assert_eq!(e["capabilities"], serde_json::json!(["display"]));
    assert_eq!(
        e["traits"],
        serde_json::json!([{"behavior":"alpha/alpha-card","trait":"CardView"}])
    );
    assert_eq!(
        e["value"],
        serde_json::json!({"behavior":"alpha/alpha-card"})
    );
    assert_eq!(e["model"], "m1");
    assert_close(&e["vectors"]["description"], &[1.27, -0.01, -1.28, 0.1]);
    assert_close(&e["vectors"]["capabilities"], &[0.5, 0.25]);
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
    // The board has no index exposure: the key is omitted, and an exposure filter drops it.
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
fn catalog_refuses_mixed_embedding_models() {
    let p = project("m2");
    let out = orb(
        p.path(),
        &["behaviors", "catalog", "--json", "alpha", "beta"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("different models") && err.contains("m1") && err.contains("m2"),
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
fn describe_returns_knobs_events_and_vectors() {
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
    assert_eq!(v["model"], "m1");
    assert_eq!(
        v["events"],
        serde_json::json!({"emits":["CARD_OPENED"],"listens":["OPEN"],"transitions":["OPEN","INIT"]})
    );
    let knobs = v["knobs"].as_array().unwrap();
    assert_eq!(knobs.len(), 2);
    assert_eq!(knobs[0]["key"], "title");
    assert_eq!(knobs[0]["type"], "string");
    assert_eq!(knobs[0]["default"], "Hello");
    assert_eq!(knobs[0]["tier"], "domain");
    assert_close(&knobs[0]["vector"], &[1.27, -0.01, -1.28, 0.1]);
    // A knob without a shipped vector carries no `vector` key.
    assert_eq!(knobs[1]["key"], "size");
    assert!(knobs[1].get("vector").is_none());
    let top: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(top.len(), 4, "strict key set, got {top:?}");
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
    assert_eq!(v["name"], "fake");
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

/// Smoke against the real monorepo std package; skipped in a standalone checkout.
#[test]
fn smoke_real_std_package() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    if !root
        .join("packages/almadar-std/dist/behaviors-embeddings.json")
        .is_file()
    {
        return;
    }
    let v = json(&orb(
        &root,
        &["behaviors", "catalog", "--json", "std/ui/core/molecules"],
    ));
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
    let d = json(&orb(&root, &["behaviors", "describe", "--json", &spec]));
    assert_eq!(d["value"]["trait"], name.as_str());
    assert!(d["events"]["transitions"].is_array());
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
