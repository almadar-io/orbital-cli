//! Relative `uses … from "./x"` imports resolve against the importing file's
//! folder whatever the process cwd is, and load a sibling that exists only as
//! `.lolo` (G-ORB-056). `validate --json` used to re-inline the inlined schema
//! without its source path, resolving `./x` against the cwd instead.

use std::path::{Path, PathBuf};
use std::process::Command;

const CART: &str = r#"orbital ShoppingCart {
  entity Cart [persistent: carts] {
    id : string!
    total : number = 0
  }

  trait CartView -> @rebindable Cart [interaction, instance] {
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
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
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
fn resolve_lowers_a_lolo_only_sibling() {
    let dir = folder("resolve", &[("Cart.lolo", CART), ("Orders.lolo", ORDERS)]);
    let out_file = dir.join("resolved.orb");
    let out = orb()
        .args(["resolve", dir.join("Orders.lolo").to_str().unwrap(), "-o"])
        .arg(&out_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "resolve failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(resolved["orbitals"][0]["name"], "OrderProcessing");
    assert_eq!(resolved["orbitals"][0]["traits"][0]["name"], "CartView");
}

#[test]
fn a_missing_sibling_still_fails_validation() {
    let dir = folder("missing", &[("Orders.lolo", ORDERS)]);
    let report = validate_json(&dir.join("Orders.lolo"));
    assert_eq!(report["valid"], false, "{report:#}");
}
