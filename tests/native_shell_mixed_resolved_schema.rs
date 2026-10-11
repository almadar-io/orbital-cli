//! `orb compile --frontend ios|android` (the mixed path) bundles the same
//! `resolved_schema.json` the `--shell` path does — without it the in-app
//! kernel has nothing to mount. Uses `01-basic-entity.orb`, which has no
//! `uses` peers, so it runs in a standalone checkout.

mod common;

use orbital_core::schema::TraitReference;
use std::path::PathBuf;
use std::process::Command;

fn orbital_rust_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn compile_frontend(frontend: &str, tag: &str) -> PathBuf {
    let out_dir = std::env::temp_dir().join(format!(
        "mixed-resolved-schema-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    let output = Command::new(common::orbital())
        .args([
            "compile",
            orbital_rust_root()
                .join("test-schemas/01-basic-entity.orb")
                .to_str()
                .unwrap(),
            "--frontend",
            frontend,
            "-o",
            out_dir.to_str().unwrap(),
        ])
        .current_dir(orbital_rust_root())
        .output()
        .expect("spawn orbital");
    assert!(
        output.status.success(),
        "compile --frontend {frontend} failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    out_dir
}

fn assert_resolved_schema(resource: &std::path::Path) {
    assert!(
        resource.is_file(),
        "expected resolved_schema.json at {}",
        resource.display()
    );
    let schema: orbital_core::OrbitalSchema =
        serde_json::from_str(&std::fs::read_to_string(resource).unwrap())
            .expect("valid OrbitalSchema");
    let traits: Vec<_> = schema
        .orbitals
        .iter()
        .flat_map(|o| o.traits.iter())
        .collect();
    assert!(!traits.is_empty());
    assert!(
        traits
            .iter()
            .all(|t| matches!(t, TraitReference::Inline(_))),
        "schema not fully resolved"
    );
}

#[test]
fn frontend_ios_output_contains_resolved_schema_json() {
    let out = compile_frontend("ios", "ios");
    assert_resolved_schema(&out.join("frontend/Sources/OrbitalApp/Resources/resolved_schema.json"));
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn frontend_android_output_contains_resolved_schema_json() {
    let out = compile_frontend("android", "android");
    assert_resolved_schema(&out.join("frontend/app/src/main/assets/resolved_schema.json"));
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn control_frontend_typescript_output_carries_no_native_resolved_schema() {
    let out = compile_frontend("typescript", "ts");
    assert!(!out
        .join("frontend/Sources/OrbitalApp/Resources/resolved_schema.json")
        .exists());
    assert!(!out
        .join("frontend/app/src/main/assets/resolved_schema.json")
        .exists());
    let _ = std::fs::remove_dir_all(&out);
}
