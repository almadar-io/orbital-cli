//! Integration tests for the Phase 4 / Phase 7 subset: `--map` emission
//! and the `orb emit-trace` reverse-lookup subcommand.
//!
//! See `docs/Almadar_Compiler_Emit_Code.md` §17. These cover file-level
//! granularity only — per-expression spans land with Phase 5.

use std::path::PathBuf;
use std::process::Command;

fn monorepo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_path_buf()
}

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn std_modal_orb() -> PathBuf {
    monorepo_root().join("packages/almadar-std/behaviors/registry/atoms/std-modal.orb")
}

fn run(args: &[&str]) -> (String, String, bool) {
    let o = Command::new(orb_bin())
        .args(args)
        .output()
        .expect("failed to spawn orb");
    (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
        o.status.success(),
    )
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "orb-emit-map-test-{}-{}",
        std::process::id(),
        name
    ));
    p
}

// --- Phase 4a: --map sidecar emission --------------------------------------

#[test]
fn map_sidecar_is_valid_source_map_v3() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let out = tmp("valid.ts");
    let map = PathBuf::from(format!("{}.map", out.display()));

    let (_, stderr, ok) = run(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "traits.ModalRecordModal",
        "--only",
        "hooks",
        "-o",
        out.to_str().unwrap(),
        "--map",
    ]);
    assert!(ok, "emit --map failed: {}", stderr);
    assert!(out.exists());
    assert!(map.exists(), "sidecar map not written");

    let map_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&map).unwrap())
            .expect("map is not valid JSON");
    assert_eq!(
        map_json.get("version").and_then(|v| v.as_u64()),
        Some(3),
        "not a v3 source map"
    );
    let sources = map_json
        .get("sources")
        .and_then(|s| s.as_array())
        .expect("sources missing");
    assert!(
        !sources.is_empty(),
        "sources array must contain at least the .orb path"
    );
    assert!(
        sources[0].as_str().unwrap_or("").ends_with("std-modal.orb"),
        "sources[0] should point at the schema file; got: {:?}",
        sources[0]
    );
    assert!(map_json.get("mappings").and_then(|m| m.as_str()).is_some());

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&map);
}

#[test]
fn inline_map_on_stdout_ts() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let (stdout, _, ok) = run(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "entities.ModalRecord",
        "--map",
    ]);
    assert!(ok);
    assert!(
        stdout.contains("//# sourceMappingURL=data:application/json;base64,"),
        "inline sourceMappingURL comment missing from stdout"
    );
}

#[test]
fn map_ignored_for_orb_target() {
    // `--map` on `emit orb` is a no-op (orb → orb round-trip has no mapping).
    // We just assert it doesn't error.
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let out = tmp("orb-target.orb");
    let map = PathBuf::from(format!("{}.map", out.display()));
    let _ = std::fs::remove_file(&map);

    let (_, _, ok) = run(&[
        "emit",
        "orb",
        orb.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--map",
    ]);
    assert!(ok);
    assert!(out.exists());
    assert!(!map.exists(), "orb target should not produce a .map sidecar");

    let _ = std::fs::remove_file(&out);
}

// --- Phase 7a: orb emit-trace ----------------------------------------------

#[test]
fn emit_trace_resolves_generated_line() {
    let orb = std_modal_orb();
    if !orb.exists() {
        return;
    }
    let out = tmp("trace.ts");
    let map = PathBuf::from(format!("{}.map", out.display()));

    let (_, _, ok1) = run(&[
        "emit",
        "ts",
        orb.to_str().unwrap(),
        "--section",
        "traits.ModalRecordModal",
        "--only",
        "hooks",
        "-o",
        out.to_str().unwrap(),
        "--map",
    ]);
    assert!(ok1);

    let target = format!("{}:{}", out.display(), 1);
    let (stdout, _, ok2) = run(&["emit-trace", &target]);
    assert!(ok2);
    assert!(
        stdout.contains("std-modal.orb"),
        "trace output should reference the .orb source; got: {}",
        stdout
    );
    assert!(
        stdout.contains("→"),
        "trace output should include the arrow separator; got: {}",
        stdout
    );

    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&map);
}

#[test]
fn emit_trace_errors_without_map() {
    let tmp_file = tmp("no-map.ts");
    std::fs::write(&tmp_file, "// no map here\n").unwrap();
    let _ = std::fs::remove_file(PathBuf::from(format!("{}.map", tmp_file.display())));

    let (_, stderr, ok) = run(&["emit-trace", &format!("{}:1:1", tmp_file.display())]);
    assert!(!ok);
    assert!(
        stderr.contains("no source map found"),
        "expected clear missing-map error; got: {}",
        stderr
    );

    let _ = std::fs::remove_file(&tmp_file);
}
