//! `orb verify` — layered headless verification of an Orbital app.
//!
//! Layer 1 (kernel circuit, `orbital-verify`) + Layer 2 (client render contract,
//! `orbital_client::render_probe`). Runs on a single `.orb` or across the
//! `almadar-std`/`almadar-behaviors` registry corpus (`--registry [--topic <t>]`).
//! Fail-closed: non-zero exit if any organism fails either layer.

use anyhow::Result;
use std::path::{Path, PathBuf};

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_verify(
    path: Option<PathBuf>,
    registry: bool,
    topic: Option<String>,
    report_dir: Option<PathBuf>,
    screenshot_dir: Option<PathBuf>,
    frames_dir: Option<PathBuf>,
    server: bool,
) -> Result<()> {
    let targets = collect_targets(path, registry, topic.as_deref());
    if targets.is_empty() {
        anyhow::bail!("no .orb targets (pass a FILE, or --registry [--topic <t>])");
    }

    // Per-frame (state-transition) screenshot capture mode — drives each planned
    // transition and writes PNGs + a frames.json manifest for the vision-review workflow.
    if let Some(dir) = frames_dir {
        #[cfg(feature = "screenshot")]
        {
            let mut ok = true;
            for target in &targets {
                println!("frames: {}", display(target));
                match run_screenshot_frames(target, &dir) {
                    Ok(_) => {}
                    Err(e) => {
                        ok = false;
                        eprintln!("[ERROR] {}: {e}", display(target));
                    }
                }
            }
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = dir;
            anyhow::bail!("--screenshot-frames requires `orb` built with `--features screenshot`");
        }
    }

    let mut passed = 0usize;
    let mut failed = 0usize;
    for target in &targets {
        match verify_one(target, report_dir.as_deref(), screenshot_dir.as_deref(), server) {
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

fn verify_one(
    path: &Path,
    report_dir: Option<&Path>,
    screenshot_dir: Option<&Path>,
    server: bool,
) -> Result<bool> {
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

    // Layer 2 — client render contract (enforces the bindings L1 deferred), in-process kernel.
    let l2 = orbital_client::render_probe::probe_schema(&item, schema.clone())?;
    println!("{}", l2.headline());
    for v in &l2.verdicts {
        if !v.passed {
            println!("    L2 FAIL {} — {}", v.name, v.detail);
        }
    }

    // Layer 2 (server) — the SAME render contract driven over real HTTP against a live
    // `orbital-server`, verifying server routes + serialization + client together.
    let mut l2_server_ok = true;
    if server {
        let l2s = super::verify_server::probe_over_http(&item, schema.clone())?;
        println!("{} [server]", l2s.headline());
        for v in &l2s.verdicts {
            if !v.passed {
                l2_server_ok = false;
                println!("    L2/server FAIL {} — {}", v.name, v.detail);
            }
        }
    }

    // Layer 3 — headless screenshot (opt-in; only when the binary was built with it).
    if let Some(dir) = screenshot_dir {
        #[cfg(feature = "screenshot")]
        match orbital_client::screenshot::screenshot_schema(&item, schema.clone(), dir) {
            Ok(paths) => {
                for p in paths {
                    println!("    📸 {}", p.display());
                }
            }
            Err(e) => println!("    📸 screenshot FAILED — {e}"),
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = dir;
            println!("    📸 (rebuild orb with `--features screenshot` to capture)");
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

    Ok(l1.ok() && l2.ok() && l2_server_ok)
}

/// Per-frame (state-transition) screenshot capture: for the primary orbital's richest
/// trait, drive each planned transition in sequence and capture the resulting UI, writing
/// a `frames.json` manifest (the input to the vision-review workflow). Returns true on success.
#[cfg(feature = "screenshot")]
pub(crate) fn run_screenshot_frames(path: &Path, out_dir: &Path) -> Result<bool> {
    use orbital_client::bridge::{KernelBridge, ServerBridge};
    use std::sync::Arc;

    let schema = orbital_verify::corpus::resolve_file(path)?;
    let item = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();
    std::fs::create_dir_all(out_dir)?;

    // Pick the trait that drives the VISIBLE `main` slot the most (so frames differ as
    // it transitions) — a backend/lifecycle trait with no `render-ui main` would yield
    // identical frames. Tie-break on state count.
    let traits = orbital_verify::planner::inline_traits(&schema);
    let renders_main = |t: &orbital_core::TraitDefinition| -> usize {
        t.state_machine
            .transitions
            .iter()
            .filter(|tr| {
                tr.effects.as_ref().is_some_and(|effs| {
                    effs.iter().any(|e| {
                        let s = serde_json::to_string(e).unwrap_or_default();
                        s.contains("render-ui") && s.contains("\"main\"")
                    })
                })
            })
            .count()
    };
    let Some(td) = traits
        .iter()
        .max_by_key(|t| (renders_main(t), t.state_machine.states.len()))
        .copied()
    else {
        anyhow::bail!("no inline traits to walk");
    };
    let trait_name = td.name.clone();
    let steps = orbital_verify::planner::plan_trait(td);

    let mut frames = Vec::new();
    let mut idx = 0usize;
    // Frame 0 — the initial full UI (all traits INIT'd).
    let mut entry = |i: usize, from: &str, event: &str, to: &str, png: &Path| {
        frames.push(serde_json::json!({
            "frame": i, "trait": trait_name, "from": from, "event": event, "to": to,
            "png": png.to_string_lossy(),
        }));
    };

    let build_app = || -> Result<orbital_client::rendering::desktop::DesktopApp> {
        let bridge = Arc::new(KernelBridge::new(schema.clone()).map_err(|e| anyhow::anyhow!("bridge: {e}"))?);
        orbital_client::screenshot::seed_entities(&bridge, &schema);
        let mut app = orbital_client::rendering::desktop::DesktopApp::new_headless(
            schema.clone(),
            Arc::clone(&bridge) as Arc<dyn ServerBridge>,
        );
        // Drive (and display) the trait we're walking, not the orbital's first trait.
        app.set_active_trait(&trait_name);
        app.init_all_traits_blocking();
        Ok(app)
    };

    {
        let mut app = build_app()?;
        // Pump INIT's fetch→`<X>Loaded` emits to quiescence (as the real frame loop does)
        // so the initial frame shows the natural *settled* state — a populated data table,
        // not a frozen spinner. The per-transition frames below stay hermetic.
        app.settle(8);
        let png = out_dir.join(format!("{item}.frame{idx:02}.initial.png"));
        orbital_client::screenshot::capture(&mut app, &png)?;
        entry(idx, "·", "INIT", app.current_state_name(), &png);
        println!("    📸 frame {idx} INIT → {} — {}", app.current_state_name(), png.display());
        idx += 1;
    }

    // One hermetic frame per planned transition (replay to `from`, then fire the event).
    for step in steps.iter().take(10) {
        let mut app = build_app()?;
        if let Some(path) = orbital_verify::planner::replay_path(td, &step.from) {
            for (ev, pl) in path {
                let v = serde_json::from_value(pl).unwrap_or(orbital_core::Value::Null);
                app.drive_blocking(&ev, Some(&trait_name), v);
            }
        }
        let payload = serde_json::from_value(step.payload.clone()).unwrap_or(orbital_core::Value::Null);
        app.drive_blocking(&step.event, Some(&trait_name), payload);
        // The planner's expected target for THIS trait (current_state_name tracks the
        // active/first trait, not the one being driven).
        let to = step.to.clone();
        let png = out_dir.join(format!("{item}.frame{idx:02}.{}.png", step.event));
        orbital_client::screenshot::capture(&mut app, &png)?;
        entry(idx, &step.from, &step.event, &to, &png);
        println!("    📸 frame {idx} {} --{}--> {to} — {}", step.from, step.event, png.display());
        idx += 1;
    }

    let manifest = serde_json::json!({ "item": item, "trait": td.name, "frames": frames });
    let mpath = out_dir.join(format!("{item}.frames.json"));
    std::fs::write(&mpath, serde_json::to_string_pretty(&manifest)?)?;
    println!("    📋 manifest: {}", mpath.display());
    Ok(true)
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
