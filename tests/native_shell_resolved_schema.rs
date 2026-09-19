//! B6 (`docs/Almadar_Compiler_Gaps.md` §159, plan `groovy-inventing-cloud.md`):
//! `orb compile --shell ios|android` bundles the schema's RESOLVED form
//! (every `uses` import inlined, every `TraitReference` flattened to
//! `Inline`) as an app resource, so the in-app `RuntimeKernel` mounts on
//! device with no compiler present. Drives the real `orbital`/`orb` binary
//! against `test-schemas/std-notes.orb`, which `uses` `almadar-behaviors/*`
//! peers — the same monorepo dependency `compat_differential.rs` has, so
//! both tests are `#[ignore]`d for a standalone orbital-rust checkout and
//! run explicitly here.

use orbital_core::schema::TraitReference;
use std::path::PathBuf;
use std::process::Command;

fn orbital_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orbital"))
}

/// CARGO_MANIFEST_DIR = <repo-root>/orbital-rust/crates/orbital-cli
fn orbital_rust_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn repo_root() -> PathBuf {
    orbital_rust_root().parent().unwrap().to_path_buf()
}

fn std_notes_orb() -> PathBuf {
    orbital_rust_root().join("test-schemas/std-notes.orb")
}

/// Every `TraitReference` in the resolved schema must be `Inline` — the
/// concrete invariant "resolved" means (`orbital_core::schema::TraitReference`
/// doc comment): imports/`uses` collapsed, nothing left for a device-side
/// compiler to expand.
fn assert_fully_resolved(schema: &orbital_core::OrbitalSchema) {
    assert!(!schema.orbitals.is_empty(), "resolved schema has no orbitals");
    let mut total_traits = 0;
    for orbital in &schema.orbitals {
        for trait_ref in &orbital.traits {
            total_traits += 1;
            assert!(
                matches!(trait_ref, TraitReference::Inline(_)),
                "orbital '{}' carries a non-inline TraitReference::Reference — \
                 the schema is not fully resolved",
                orbital.name
            );
        }
    }
    assert!(total_traits > 0, "resolved schema has orbitals but no traits");
}

fn compile_native(shell: &str, out_dir: &std::path::Path) {
    let repo = repo_root();
    for rel in [
        "packages/almadar-std/behaviors/registry",
        "packages/almadar-behaviors/behaviors/registry",
    ] {
        let root = repo.join(rel);
        assert!(
            root.is_dir(),
            "monorepo registry root missing: {} — this test needs the full \
             kflow.ai.builder workspace, not a standalone orbital-rust checkout",
            root.display()
        );
    }

    let output = Command::new(orbital_bin())
        .args([
            "compile",
            std_notes_orb().to_str().unwrap(),
            "--shell",
            shell,
            "-o",
            out_dir.to_str().unwrap(),
        ])
        .env("ALMADAR_DEV", "1")
        .env("ALMADAR_ROOT", &repo)
        .output()
        .expect("failed to spawn orbital binary");

    assert!(
        output.status.success(),
        "orbital compile --shell {shell} failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "requires the full kflow.ai.builder workspace (packages/almadar-std, \
            packages/almadar-behaviors) — not present in a standalone orbital-rust \
            checkout; run explicitly with --ignored"]
fn ios_compile_embeds_resolved_schema_resource() {
    let out_dir = std::env::temp_dir().join(format!(
        "b6-ios-resolved-schema-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    compile_native("ios", &out_dir);

    let resource = out_dir.join("Sources/OrbitalApp/Resources/resolved_schema.json");
    assert!(
        resource.is_file(),
        "expected resolved_schema.json at {}",
        resource.display()
    );
    let content = std::fs::read_to_string(&resource).unwrap();
    let schema: orbital_core::OrbitalSchema =
        serde_json::from_str(&content).expect("resolved_schema.json is not a valid OrbitalSchema");
    assert_fully_resolved(&schema);

    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "requires the full kflow.ai.builder workspace (packages/almadar-std, \
            packages/almadar-behaviors) — not present in a standalone orbital-rust \
            checkout; run explicitly with --ignored"]
fn android_compile_embeds_resolved_schema_resource() {
    let out_dir = std::env::temp_dir().join(format!(
        "b6-android-resolved-schema-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    compile_native("android", &out_dir);

    let resource = out_dir.join("app/src/main/assets/resolved_schema.json");
    assert!(
        resource.is_file(),
        "expected resolved_schema.json at {}",
        resource.display()
    );
    let content = std::fs::read_to_string(&resource).unwrap();
    let schema: orbital_core::OrbitalSchema =
        serde_json::from_str(&content).expect("resolved_schema.json is not a valid OrbitalSchema");
    assert_fully_resolved(&schema);

    let _ = std::fs::remove_dir_all(&out_dir);
}
