//! A command that runs as an installed program refuses language data this orb cannot read with the typed error,
//! never a panic (G-CROSS-765).

use orbital_core::json_value::from_json_str;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn project(unreadable_access: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), r#"{"name":"preflight-test","private":true}"#).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let start = Path::new(env!("CARGO_MANIFEST_DIR"));
    orbital_test_support::link_installed_language(dir.path(), start);
    let std = dir.path().join("node_modules/@almadar/std");
    let manifest_path = std.join("package.json");
    let mut manifest: serde_json::Value = from_json_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["orb"] = serde_json::json!({ "prefix": "almadar-std", "source": "behaviors/lolo", "language": ">=0.0.0" });
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    orbital_test_support::link_installed_std_programs(&std, start);
    if unreadable_access {
        let ops_path = std.join("canonical-operators.json");
        let mut ops: serde_json::Value = from_json_str(&std::fs::read_to_string(&ops_path).unwrap()).unwrap();
        ops["operators"]["kernel/commit"]["grant"][0]["access"] = serde_json::json!("an-access-this-orb-cannot-read");
        std::fs::write(&ops_path, ops.to_string()).unwrap();
    }
    std::fs::write(dir.path().join("a.lolo"), "orbital A {\n  entity A [runtime] {\n    n : number = 0\n  }\n}\n").unwrap();
    dir
}

fn orb(dir: &Path, args: &[&str]) -> Output {
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_orb")))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("ALMADAR_DERIVED_CACHE", dir.join(".derived"))
        .output()
        .expect("run orb")
}

#[test]
fn a_command_program_refuses_an_operator_table_it_cannot_read_with_the_typed_error() {
    let dir = project(true);
    let out = orb(dir.path(), &["validate", "a.lolo"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "orb validate ran on unreadable language data: {stderr}");
    assert!(!stderr.contains("panicked"), "orb panicked instead of refusing: {stderr}");
    assert!(stderr.contains("ORB_LANGUAGE_DATA_INCOMPATIBLE"), "no typed refusal: {stderr}");
}

#[test]
fn control_a_command_program_on_readable_language_data_is_not_refused_by_the_preflight() {
    let dir = project(false);
    let out = orb(dir.path(), &["validate", "a.lolo"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("ORB_LANGUAGE_DATA_"), "readable language data refused: {stderr}");
    assert!(!stderr.contains("panicked"), "orb panicked: {stderr}");
}
