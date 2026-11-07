//! `orb resolve` of a program with `locales` writes each locale's merged
//! catalog beside the resolved `.orb`; stdout has nowhere to put them.

mod common;

use std::path::PathBuf;
use std::process::Command;

const SITE: &str = r#"app site [ version: "1.0.0", description: "Site", locales: [en, ar] ]

orbital Home {
  entity HomeView [runtime] {
    id : string!
  }

  trait HomePage -> HomeView {
    initial: idle

    state idle {
      INIT -> idle
        (render-ui main { type: typography, content: (i18n/t "hero.title") })
    }
  }

  page "/" as HomeRoute -> HomePage
}
"#;

fn folder(tag: &str, locales: bool) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("orb-resolve-catalogs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = if locales {
        SITE.to_string()
    } else {
        SITE.replace(", locales: [en, ar]", "")
            .replace(r#"(i18n/t "hero.title")"#, r#""Hello""#)
    };
    std::fs::write(dir.join("site.lolo"), source).unwrap();
    if locales {
        for (locale, message) in [("en", "Hello"), ("ar", "مرحبا")] {
            std::fs::create_dir_all(dir.join("locales").join(locale)).unwrap();
            std::fs::write(
                dir.join("locales").join(locale).join("site.json"),
                format!(r#"{{"hero.title":"{message}"}}"#),
            )
            .unwrap();
        }
    }
    dir
}

fn orb() -> Command {
    Command::new(common::orb())
}

#[test]
fn emit_orb_to_a_file_writes_each_locales_catalog_beside_it() {
    let dir = folder("emit", true);
    let registry = dir.join("registry");
    std::fs::create_dir_all(&registry).unwrap();
    let out = registry.join("site.orb");
    let status = orb()
        .args(["emit", "orb", dir.join("site.lolo").to_str().unwrap(), "--output", out.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    // The behavior's own catalogs travel with it, so a program importing it from the registry finds them.
    for (locale, message) in [("en", "Hello"), ("ar", "مرحبا")] {
        let own = std::fs::read_to_string(registry.join("locales").join(locale).join("site.json")).unwrap();
        assert!(own.contains("hero.title") && own.contains(message), "{locale}: {own}");
    }
    assert!(!registry.join("site.en.json").exists(), "no flattened sidecar: hosts read locales/<locale>/<name>.json");
}

#[test]
fn control_emit_orb_of_a_program_without_locales_writes_no_catalog() {
    let dir = folder("emit-plain", false);
    let out = dir.join("registry.orb");
    let status = orb()
        .args(["emit", "orb", dir.join("site.lolo").to_str().unwrap(), "--output", out.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(!dir.join("registry.en.json").exists());
}
