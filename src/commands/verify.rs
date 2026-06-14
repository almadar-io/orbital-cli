//! `orb verify` — layered headless verification of an Orbital app.
//!
//! Layer 1 (kernel circuit, `orbital-verify`) + Layer 2 (client render contract,
//! `orbital_client::render_probe`). Runs on a single `.orb` or across the
//! `almadar-std`/`almadar-behaviors` registry corpus (`--registry [--topic <t>]`).
//! Fail-closed: non-zero exit if any organism fails either layer.

use anyhow::Result;
use std::path::{Path, PathBuf};

pub(crate) fn run_verify(
    path: Option<PathBuf>,
    registry: bool,
    topic: Option<String>,
    report_dir: Option<PathBuf>,
) -> Result<()> {
    let targets = collect_targets(path, registry, topic.as_deref());
    if targets.is_empty() {
        anyhow::bail!("no .orb targets (pass a FILE, or --registry [--topic <t>])");
    }

    let mut passed = 0usize;
    let mut failed = 0usize;
    for target in &targets {
        match verify_one(target, report_dir.as_deref()) {
            Ok(true) => passed += 1,
            Ok(false) => failed += 1,
            Err(e) => {
                failed += 1;
                eprintln!("[ERROR] {}: {e}", display(target));
            }
        }
    }

    println!("\nverify: {passed}/{} organism(s) passed", passed + failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn verify_one(path: &Path, report_dir: Option<&Path>) -> Result<bool> {
    let schema = orbital_verify::corpus::resolve_file(path)?;
    let item = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Layer 1 — kernel circuit.
    let l1 = orbital_verify::verify_schema(&item, schema.clone())?;
    println!("{}", l1.headline());
    for v in &l1.verdicts {
        if !v.passed {
            println!("    L1 FAIL {} — {}", v.name, v.detail);
        }
    }

    // Layer 2 — client render contract (enforces the bindings L1 deferred).
    let l2 = orbital_client::render_probe::probe_schema(&item, schema)?;
    println!("{}", l2.headline());
    for v in &l2.verdicts {
        if !v.passed {
            println!("    L2 FAIL {} — {}", v.name, v.detail);
        }
    }

    if let Some(dir) = report_dir {
        std::fs::create_dir_all(dir).ok();
        let report = serde_json::json!({
            "item": item,
            "layer1": l1.to_json(),
            "layer2": serde_json::to_value(&l2).unwrap_or(serde_json::Value::Null),
        });
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(dir.join(format!("{item}.verify-report.json")), json).ok();
        }
    }

    Ok(l1.ok() && l2.ok())
}

fn collect_targets(path: Option<PathBuf>, registry: bool, topic: Option<&str>) -> Vec<PathBuf> {
    if let Some(p) = path {
        return vec![p];
    }
    if registry || topic.is_some() {
        let mut all = orbital_verify::corpus::discover_registry(&repo_root());
        if let Some(t) = topic {
            let needle = format!("/{t}/");
            all.retain(|p| p.to_string_lossy().contains(&needle));
        }
        return all;
    }
    Vec::new()
}

fn repo_root() -> PathBuf {
    // .../orbital-rust/crates/orbital-cli → ../../.. = monorepo root (packages/ lives there).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn display(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}
