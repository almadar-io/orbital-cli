//! Relative `uses … from "./x"` imports resolve against the importing file's
//! folder whatever the process cwd is, and load a sibling that exists only as
//! `.lolo` (G-ORB-056). `validate --json` used to re-inline the inlined schema
//! without its source path, resolving `./x` against the cwd instead.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

const CART: &str = r#"orbital ShoppingCart {
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

const ORDERS: &str = r#"orbital OrderProcessing {
  uses ShoppingCart from "./Cart"

  entity Order [persistent: orders] {
    id : string!
    total : number = 0
  }

  trait CartView = ShoppingCart.traits.CartView -> Order {
  }

  page "/orders" as OrdersPage -> CartView
}
"#;

fn orb() -> Command {
    // cwd stays the crate dir: never the fixture folder.
    Command::new(common::orb())
}

fn folder(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orb-relative-lolo-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

fn emit_orb(lolo: &Path) {
    let out = orb()
        .args(["emit", "orb", lolo.to_str().unwrap(), "-o"])
        .arg(lolo.with_extension("orb"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn validate_json(file: &Path) -> serde_json::Value {
    let out = orb()
        .args(["validate", "--json", file.to_str().unwrap()])
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "validate --json is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn validate_json_resolves_a_sibling_orb_from_the_file_folder() {
    let dir = folder("orb", &[("Cart.lolo", CART), ("Orders.lolo", ORDERS)]);
    emit_orb(&dir.join("Cart.lolo"));
    let report = validate_json(&dir.join("Orders.lolo"));
    assert_eq!(report["valid"], true, "{report:#}");
}

#[test]
fn validate_json_lowers_a_lolo_only_sibling() {
    let dir = folder("lolo", &[("Cart.lolo", CART), ("Orders.lolo", ORDERS)]);
    let report = validate_json(&dir.join("Orders.lolo"));
    assert_eq!(report["valid"], true, "{report:#}");
}

#[test]
fn a_missing_sibling_still_fails_validation() {
    let dir = folder("missing", &[("Orders.lolo", ORDERS)]);
    let report = validate_json(&dir.join("Orders.lolo"));
    assert_eq!(report["valid"], false, "{report:#}");
}

fn lazy_child(page: &str) -> String {
    format!("app child [ version: \"1.0.0\" ]\norbital ChildOrbital {{\n  entity Note [runtime] {{\n    id : string!\n  }}\n  trait ChildView -> Note {{\n    initial: a\n    state a {{\n      INIT -> a\n        (render-ui main {{ type: typography, content: \"child\" }})\n    }}\n  }}\n  page \"{page}\" as ChildPage -> ChildView\n}}\n")
}

const LAZY_PARENT: &str = "app main [ version: \"1.0.0\" ]\nuses lazy Child from \"./child\"\norbital MainOrbital {\n  entity Item [runtime] {\n    id : string!\n  }\n  trait MainView -> Item {\n    initial: a\n    state a {\n      INIT -> a\n        (render-ui main { type: typography, content: \"main\" })\n    }\n  }\n  page \"/\" as Home -> MainView\n}\norbital Kid = Child.orbitals.ChildOrbital {\n}\n";

fn lazy_child_findings(report: &serde_json::Value) -> Vec<String> {
    report["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["path"].as_str())
        .filter(|p| p.starts_with("lazy[Kid]."))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_lazy_childs_lint_finding_is_reported_under_its_orbital() {
    let dir = folder(
        "lazy-bad",
        &[
            ("child.lolo", &lazy_child("child")),
            ("main.lolo", LAZY_PARENT),
        ],
    );
    let report = validate_json(&dir.join("main.lolo"));
    assert_eq!(
        lazy_child_findings(&report),
        vec!["lazy[Kid].orbitals[0].pages[0].path".to_string(); 2],
        "the page-path lint runs on the lazy child: {report:#}"
    );
}

#[test]
fn control_a_healthy_lazy_child_adds_no_child_finding() {
    let dir = folder(
        "lazy-ok",
        &[
            ("child.lolo", &lazy_child("/child")),
            ("main.lolo", LAZY_PARENT),
        ],
    );
    let report = validate_json(&dir.join("main.lolo"));
    assert!(
        report.is_object(),
        "validate answers rather than hanging: {report:#}"
    );
    assert_eq!(
        lazy_child_findings(&report),
        Vec::<String>::new(),
        "{report:#}"
    );
}
