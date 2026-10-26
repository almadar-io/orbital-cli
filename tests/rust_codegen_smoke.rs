//! Codegen smoke + emit→verify for the Rust shell (`orb compile --shell rust`).
//!
//! Two guarantees:
//!   1. **Scaffold emission** — compiling a schema to the Rust shell produces a complete,
//!      *syntactically valid* Cargo project (every generated `.rs` parses with `syn`), plus
//!      the bundled `schema.orb` / `resolved.orb` the emitted binary runs.
//!   2. **Emit→verify** — the schema the emitted app actually runs (`run_desktop(schema.orb)`)
//!      passes the same Layer-1 + Layer-2 verifier as `orb verify`, so our emissions are
//!      verified end-to-end, not just well-formed.
//!
//! Runs the compiled `orb` binary via `CARGO_BIN_EXE_orb` (no in-process crate wiring).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

fn orb_bin() -> PathBuf {
    common::orb()
}

/// std-modal composed with the std HTTP server: a server compile needs the program to declare its own endpoint.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std-modal-served.lolo")
}

/// A unique output dir under cargo's per-test-target tmp dir (auto-located, no collisions).
fn out_dir(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn run_orb(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(orb_bin())
        .args(args)
        .output()
        .expect("spawn orb");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn compile_rust(out: &Path) -> (String, String, bool) {
    run_orb(&[
        "compile",
        fixture().to_str().unwrap(),
        "--shell",
        "rust",
        "-o",
        out.to_str().unwrap(),
    ])
}

/// Every generated `.rs` under `dir` (recursively).
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn rust_shell_emits_complete_scaffold() {
    let out = out_dir("rs_scaffold");
    let (_stdout, stderr, ok) = compile_rust(&out);
    assert!(
        ok,
        "compile --shell rust exited non-zero. stderr:\n{stderr}"
    );

    // The thin host (v18: the server is the composed std-http-server program, no bespoke codegen) + the bundled schemas.
    for required in [
        "Cargo.toml",
        "src/main.rs",
        "src/bin/desktop.rs",
        "src/bin/game.rs",
        "src/bin/terminal.rs",
        "schema.orb",   // source reference copy
        "resolved.orb", // the fully-resolved bundle the clients load
        "program.orb",  // the app composed with std-http-server, which the server host runs
    ] {
        assert!(
            out.join(required).exists(),
            "missing emitted file: {required}\n(emitted: {:?})",
            rust_files(&out)
        );
    }

    // The runtime must load the RESOLVED schema — reading the raw `schema.orb` 404s every
    // event ("Trait not found: X.traits.Y") for any `uses:`-based organism.
    for entry in [
        "src/bin/desktop.rs",
        "src/bin/game.rs",
        "src/bin/terminal.rs",
    ] {
        let src = std::fs::read_to_string(out.join(entry)).unwrap();
        assert!(
            src.contains("resolved.orb"),
            "{entry} must load resolved.orb (loading raw schema.orb 404s on uses:-based schemas)"
        );
    }
    let server = std::fs::read_to_string(out.join("src/main.rs")).unwrap();
    assert!(
        server.contains("program.orb"),
        "the server host runs the composed program.orb"
    );
}

#[test]
fn emitted_rust_is_syntactically_valid() {
    let out = out_dir("rs_synvalid");
    let (_o, stderr, ok) = compile_rust(&out);
    assert!(ok, "compile failed: {stderr}");

    let files = rust_files(&out);
    assert!(
        files.len() >= 4,
        "suspiciously few generated .rs files: {}",
        files.len()
    );
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read generated .rs");
        // `syn` with `full` parses a whole file — a parse error is malformed codegen.
        if let Err(e) = syn::parse_file(&src) {
            panic!(
                "generated Rust is not syntactically valid: {}\n{e}",
                f.display()
            );
        }
    }
}
