//! Live behaviors (W3): a behavior package that declares `orb.source` (and no
//! `orb.registry`) is read from its `.lolo` sources, lowered on read — so a
//! `std/behaviors/<name>` import compiles with no emitted `.orb`, and an edit to
//! the dependency's `.lolo` is visible to the very next run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CART: &str = r#"app std-cartx [ version: "1.0.0" ]
orbital ShoppingCart {
  entity Cart [persistent: carts] {
    id : string!
    total : number = 0
  }

  trait CartView -> @rebindable Cart {
    initial: idle

    state idle {
      INIT -> idle
        (fetch Cart)
        (render-ui main { type: typography, content: "Cart" })
    }
  }

  page "/cart" as CartPage -> CartView
}
"#;

const ORDERS: &str = r#"app orders-app [ version: "1.0.0" ]
orbital OrderProcessing {
  uses ShoppingCart from "std/behaviors/std-cartx"

  entity Order [persistent: orders] {
    id : string!
    total : number = 0
  }

  trait CartView = ShoppingCart.traits.CartView -> Order {
  }

  page "/orders" as OrdersPage -> CartView
}
"#;

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"live-test","private":true}"#,
    )
    .unwrap();
    orbital_test_support::link_installed_language(
        dir.path(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    dir
}

/// Turn the language-only std stub into a live behavior package: `orb.source`
/// points at its `.lolo` tree; the surface files are declared (empty).
fn live_std(project: &Path) -> PathBuf {
    let std = project.join("node_modules/@almadar/std");
    let manifest_path = std.join("package.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["orb"] = serde_json::json!({
        "prefix": "almadar-std",
        "source": "behaviors/lolo",
        "compiler": ">=0.0.0"
    });
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    for (rel, body) in [("behaviors/lolo/app/atoms/std-cartx.lolo", CART)] {
        let path = std.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    orbital_test_support::link_installed_std_programs(&std, Path::new(env!("CARGO_MANIFEST_DIR")));
    std.join("behaviors/lolo/app/atoms/std-cartx.lolo")
}

fn validate(project: &Path) -> Output {
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
        .args(["validate", "--json", "app.lolo"])
        .current_dir(project)
        .output()
        .unwrap()
}

fn verdict(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn a_live_std_behavior_resolves_from_its_lolo_with_no_emitted_orb() {
    let p = project();
    let source = live_std(p.path());
    std::fs::write(p.path().join("app.lolo"), ORDERS).unwrap();
    let v = verdict(&validate(p.path()));
    assert_eq!(v["valid"], true, "{v}");
    assert!(
        !source.with_extension("orb").exists(),
        "nothing was emitted"
    );
}

#[test]
fn an_edit_to_the_dependency_lolo_is_seen_by_the_next_run() {
    let p = project();
    let source = live_std(p.path());
    std::fs::write(p.path().join("app.lolo"), ORDERS).unwrap();
    assert_eq!(verdict(&validate(p.path()))["valid"], true);

    std::fs::write(&source, CART.replace("CartView", "BasketView")).unwrap();
    let v = verdict(&validate(p.path()));
    assert_eq!(
        v["valid"], false,
        "the importer's `ShoppingCart.traits.CartView` no longer exists: {v}"
    );
}

#[test]
fn control_a_package_declaring_neither_registry_nor_source_is_refused() {
    let p = project();
    live_std(p.path());
    let manifest_path = p.path().join("node_modules/@almadar/std/package.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["orb"].as_object_mut().unwrap().remove("source");
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    std::fs::write(p.path().join("app.lolo"), ORDERS).unwrap();
    let out = validate(p.path());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("neither `orb.registry`"), "{text}");
}

/// The real std corpus, read live: a live std package whose `source` is the
/// workspace's own `.lolo` tree (surface files declared from the workspace too)
/// validates real std organisms with no emitted `.orb` in play.
#[cfg(unix)]
#[test]
fn live_std_corpus_validates_real_organisms() {
    let std_root = orbital_test_support::installed_package(
        "almadar-std",
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .root;
    let p = project();
    let std = p.path().join("node_modules/@almadar/std");
    let manifest_path = std.join("package.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    // Link exactly what the installed std declares (the workspace and a published
    // tarball lay their surface files out differently), minus `registry`: read live.
    let installed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(std_root.join("package.json")).unwrap())
            .unwrap();
    let declared = &installed["orb"];
    let mut orb = serde_json::json!({
        "prefix": declared["prefix"],
        "source": declared["source"],
        "compiler": ">=0.0.0"
    });
    let mut linked = vec![declared["source"].as_str().unwrap().to_string()];
    if let Some(rel) = declared["signatures"].as_str() {
        orb["signatures"] = serde_json::Value::from(rel);
        linked.push(rel.to_string());
    }
    manifest["orb"] = orb;
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    for rel in &linked {
        let to = std.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(std_root.join(rel), &to).unwrap();
    }
    for organism in [
        "agent/organisms/std-agent-assistant.lolo",
        "agent/organisms/std-agent-builder.lolo",
    ] {
        let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
            .args(["validate", "--json"])
            .arg(std_root.join("behaviors/lolo").join(organism))
            .current_dir(p.path())
            .output()
            .unwrap();
        let v = verdict(&out);
        assert_eq!(v["valid"], true, "{organism}: {v}");
    }
}

#[test]
fn lower_prints_one_ndjson_line_per_file_in_order_with_per_file_errors() {
    let p = project();
    let source = live_std(p.path());
    let missing = p.path().join("missing.lolo");
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
        .arg("lower")
        .arg(&source)
        .arg(&missing)
        .current_dir(p.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["orb"]["name"], "std-cartx");
    assert!(
        lines[0]["orb"]["ledger"].is_object(),
        "stamped like the loader's output"
    );
    assert!(lines[1]["error"].is_string());
    assert!(lines[1].get("orb").is_none());
}

#[test]
fn lower_keeps_input_order_across_many_files() {
    let p = project();
    let source = live_std(p.path());
    let mut paths = Vec::new();
    for i in 0..12 {
        if i % 3 == 0 {
            paths.push(p.path().join(format!("missing-{i}.lolo")));
        } else {
            let copy = p.path().join(format!("copy-{i}.lolo"));
            std::fs::copy(&source, &copy).unwrap();
            paths.push(copy);
        }
    }
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
        .arg("lower")
        .args(&paths)
        .current_dir(p.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let got: Vec<String> = lines
        .iter()
        .map(|l| l["path"].as_str().unwrap().to_string())
        .collect();
    let want: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    assert_eq!(got, want);
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(line.get("error").is_some(), i % 3 == 0, "line {i}: {line}");
    }
}
