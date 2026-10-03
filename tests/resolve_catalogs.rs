//! `orb resolve` of a program with `locales` writes each locale's merged
//! catalog beside the resolved `.orb`; stdout has nowhere to put them.

use std::path::PathBuf;
use std::process::Command;

const SITE: &str = r#"app site "1.0.0"
"Site"
locales [en, ar]

orbital Home {
  entity HomeView [runtime] {
    id : string!
  }

  trait HomePage -> HomeView [interaction, instance] {
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
        SITE.replace("locales [en, ar]\n", "")
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
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
}

#[test]
fn resolve_to_a_file_writes_each_locales_catalog_beside_it() {
    let dir = folder("file", true);
    let out = dir.join("out.orb");
    let status = orb()
        .args([
            "resolve",
            dir.join("site.lolo").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let ar = std::fs::read_to_string(dir.join("out.ar.json")).unwrap();
    assert!(
        ar.contains("site:hero.title") && ar.contains("مرحبا"),
        "{ar}"
    );
}

#[test]
fn resolve_to_stdout_refuses_a_program_with_locales() {
    let dir = folder("stdout", true);
    let out = orb()
        .args(["resolve", dir.join("site.lolo").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--output"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn control_resolve_to_stdout_still_prints_a_program_without_locales() {
    let dir = folder("plain", false);
    let out = orb()
        .args(["resolve", dir.join("site.lolo").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout)
        .trim_start()
        .starts_with('{'));
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
    let en = std::fs::read_to_string(registry.join("site.en.json")).unwrap();
    assert!(en.contains("site:hero.title") && en.contains("Hello"), "{en}");
    // The behavior's own catalogs travel with it, so a program importing it from the registry finds them.
    let own = std::fs::read_to_string(registry.join("locales").join("ar").join("site.json")).unwrap();
    assert!(own.contains("hero.title") && own.contains("مرحبا"), "{own}");
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
