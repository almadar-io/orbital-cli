//! `orb verify` — layered headless verification of an Orbital app, played through
//! up to three engines (`--rust` in-process, `--runtime` JS interpreter,
//! `--compiled` codegen'd TS app) at one of three tiers (`--smoke`/standard/`--deep`).
//!
//! The flagless invocation (`orb verify FILE`) is EXACTLY today's Layer 1 (kernel
//! circuit, `orbital-verify`) + Layer 2 (client render contract,
//! `orbital_client::render_probe`) run — `run_verify_classic` below is that path,
//! untouched. Requesting another engine, a non-standard tier, or `--json` routes
//! through `run_verify_unified`, which folds every engine's findings into one
//! `UnifiedVerifyReport` (`commands/engines.rs`). Fail-closed: non-zero exit if
//! any organism fails any requested engine.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::engines::{self, Engine, Tier};

/// CLI-facing bundle for `orb verify` — built once in `main.rs` from the clap
/// `Commands::Verify` variant so this function doesn't grow another arg on every
/// new tier/engine/option.
pub(crate) struct VerifyCliArgs {
    pub path: Option<PathBuf>,
    pub registry: bool,
    pub topic: Option<String>,
    pub report_dir: Option<PathBuf>,
    pub screenshot_dir: Option<PathBuf>,
    pub frames_dir: Option<PathBuf>,
    pub server: bool,
    pub shell: Option<String>,
    pub smoke: bool,
    pub deep: bool,
    pub rust: bool,
    pub runtime: bool,
    pub compiled: bool,
    pub all: bool,
    pub json: bool,
    pub port: u16,
    pub min_coverage: Option<f64>,
    pub engine_timeout: Option<u64>,
    pub all_personas: bool,
    /// `--trait NAME` (repeatable/comma-separated): restrict the walk to one or more
    /// named traits — empty means no filter (today's full walk).
    pub traits: Vec<String>,
}

/// Route a human-readable line to stdout, unless `--json` is active — the unified
/// report must be the ONLY thing on stdout in that mode, so human lines move to stderr.
fn emit(json_mode: bool, s: impl std::fmt::Display) {
    if json_mode {
        eprintln!("{s}");
    } else {
        println!("{s}");
    }
}

fn resolve_engines(rust: bool, runtime: bool, compiled: bool, all: bool) -> Vec<Engine> {
    if all {
        return vec![Engine::Rust, Engine::Runtime, Engine::Compiled];
    }
    let mut v = Vec::new();
    if rust {
        v.push(Engine::Rust);
    }
    if runtime {
        v.push(Engine::Runtime);
    }
    if compiled {
        v.push(Engine::Compiled);
    }
    if v.is_empty() {
        v.push(Engine::Rust);
    }
    v
}

pub(crate) fn run_verify(args: VerifyCliArgs) -> Result<()> {
    let VerifyCliArgs {
        path,
        registry,
        topic,
        report_dir,
        screenshot_dir,
        frames_dir,
        server,
        shell,
        smoke,
        deep,
        rust,
        runtime,
        compiled,
        all,
        json,
        port,
        min_coverage,
        engine_timeout,
        all_personas,
        traits,
    } = args;
    let engine_timeout = engine_timeout.map(std::time::Duration::from_secs);
    let traits: Option<&[String]> = if traits.is_empty() { None } else { Some(&traits) };

    // Map `--shell` up front so an unknown value fails fast before any target is walked.
    let shell_lang = shell.as_deref().map(parse_shell_lang).transpose()?;
    let tier = if smoke {
        Tier::Smoke
    } else if deep {
        Tier::Deep
    } else {
        Tier::Standard
    };
    let engine_list = resolve_engines(rust, runtime, compiled, all);
    if traits.is_some() && engine_list != vec![Engine::Rust] {
        anyhow::bail!(
            "--trait only scopes the in-process rust engine (L1 kernel + L2 client render); \
             drop --runtime/--compiled/--all to use it"
        );
    }
    let repo_root = repo_root();

    let resolved_path = match path {
        Some(p) => Some(resolve_target(p, &repo_root)?),
        None => None,
    };

    // Per-frame (state-transition) screenshot capture mode — orthogonal to
    // tier/engine, unchanged from before the unified CLI.
    if let Some(dir) = frames_dir {
        if traits.is_some() {
            anyhow::bail!("--trait is not supported with --screenshot-frames");
        }
        let targets = collect_targets(resolved_path, registry, topic.as_deref());
        if targets.is_empty() {
            anyhow::bail!(
                "no .orb targets (pass a FILE, a behavior NAME, or --registry [--topic <t>])"
            );
        }
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

    let targets = collect_targets(resolved_path, registry, topic.as_deref());
    if targets.is_empty() {
        anyhow::bail!(
            "no .orb targets (pass a FILE, a behavior NAME, or --registry [--topic <t>])"
        );
    }

    if all_personas {
        if engine_list != vec![Engine::Rust] || tier != Tier::Standard || json || traits.is_some() {
            anyhow::bail!(
                "--all-personas is only supported with the default L1 kernel walk \
                 (no --runtime/--compiled/--all/--smoke/--deep/--json/--trait)"
            );
        }
        return run_verify_all_personas(&targets);
    }

    // Flagless (and today's already-existing-flag) invocations stay byte-for-byte
    // identical: only the rust engine, standard tier, human output, no unified fold.
    if engine_list == vec![Engine::Rust] && tier == Tier::Standard && !json {
        return run_verify_classic(
            &targets,
            report_dir.as_deref(),
            screenshot_dir.as_deref(),
            server,
            shell_lang,
            traits,
        );
    }

    run_verify_unified(
        &targets,
        tier,
        &engine_list,
        report_dir.as_deref(),
        screenshot_dir.as_deref(),
        server,
        json,
        port,
        min_coverage,
        engine_timeout,
        shell_lang,
        &repo_root,
        traits,
    )
}

fn run_verify_classic(
    targets: &[PathBuf],
    report_dir: Option<&Path>,
    screenshot_dir: Option<&Path>,
    server: bool,
    shell_lang: Option<orbital_verify::shell::ShellLang>,
    traits: Option<&[String]>,
) -> Result<()> {
    let mut passed = 0usize;
    let mut failed = 0usize;
    for target in targets {
        match verify_one(
            target,
            report_dir,
            screenshot_dir,
            server,
            shell_lang,
            Tier::Standard,
            false,
            traits,
        ) {
            Ok(outcome) => {
                if outcome.ok {
                    passed += 1;
                } else {
                    failed += 1;
                }
            }
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

/// `--all-personas`: walk the target's declared `[identity]` roster, driving one
/// full L1 kernel circuit per persona and reporting a per-persona pass/fail with
/// declared access-policy denials bucketed separately from genuine failures
/// (task #129). A target with no `[identity]` entity falls back to today's
/// single env-driven run (`ALMADAR_PERSONA`, or none).
fn run_verify_all_personas(targets: &[PathBuf]) -> Result<()> {
    let mut overall_ok = true;
    for target in targets {
        let item = display(target);
        let schema = match orbital_verify::corpus::resolve_file(target) {
            Ok(s) => s,
            Err(e) => {
                overall_ok = false;
                eprintln!("[ERROR] {item}: {e}");
                continue;
            }
        };

        let roster = orbital_core::runtime::identity_roster(
            &schema,
            orbital_core::runtime::DEFAULT_IDENTITY_ROSTER_COUNT,
        );
        if roster.is_empty() {
            match orbital_verify::verify_schema(&item, schema) {
                Ok(l1) => {
                    let ok = l1.ok();
                    overall_ok &= ok;
                    println!(
                        "{item}: no [identity] roster declared — single run, {}",
                        if ok { "PASS" } else { "FAIL" }
                    );
                }
                Err(e) => {
                    overall_ok = false;
                    eprintln!("[ERROR] {item}: {e}");
                }
            }
            continue;
        }

        println!(
            "\n{item} --all-personas ({} declared persona(s)):",
            roster.len()
        );
        let mut item_ok = true;
        for persona in &roster {
            let label = persona_label(persona);
            match orbital_verify::verify_schema_as(&item, schema.clone(), persona.clone()) {
                Ok(l1) => {
                    let breakdown = l1.classify_persona_failures();
                    item_ok &= breakdown.ok();
                    println!(
                        "  {label}: {} — {}/{} checks, {} frames",
                        if breakdown.ok() { "PASS" } else { "FAIL" },
                        l1.summary.passed,
                        l1.summary.total_checks,
                        l1.summary.frames,
                    );
                    // Denial buckets alone cannot see OWNERSHIP scoping: read
                    // filtering shrinks the row set silently and denies nothing, so
                    // a correctly owner-scoped organism reports empty buckets and
                    // reads exactly like an un-retrofitted one
                    // (S-C2-ASYMMETRY-BLIND-TO-OWNERSHIP-ONLY-SCOPING). Visible row
                    // counts are what actually differ per persona, so print them.
                    let mut visible: std::collections::BTreeMap<&str, usize> =
                        std::collections::BTreeMap::new();
                    for f in &l1.frames {
                        for (entity, n) in &f.entity_counts {
                            let slot = visible.entry(entity.as_str()).or_insert(*n);
                            *slot = (*slot).max(*n);
                        }
                    }
                    if !visible.is_empty() {
                        println!(
                            "    visible rows: {}",
                            visible
                                .iter()
                                .map(|(e, n)| format!("{e}={n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !breakdown.access_denied.is_empty() {
                        println!(
                            "    ({} access-denied, bucketed not FAILed: {})",
                            breakdown.access_denied.len(),
                            breakdown.access_denied.join("; ")
                        );
                    }
                    for g in &breakdown.genuine {
                        println!("    FAIL {g}");
                    }
                }
                Err(e) => {
                    item_ok = false;
                    eprintln!("  {label}: [ERROR] {e}");
                }
            }
        }
        println!("  overall: {}", if item_ok { "PASS" } else { "FAIL" });
        overall_ok &= item_ok;
    }

    if !overall_ok {
        std::process::exit(1);
    }
    Ok(())
}

/// `"<id>/<role>"`, or just `<id>` when the roster row declares no role.
fn persona_label(persona: &orbital_core::Value) -> String {
    let id = persona
        .get_path("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    match persona.get_path("role").and_then(|v| v.as_str()) {
        Some(role) => format!("{id}/{role}"),
        None => id.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_verify_unified(
    targets: &[PathBuf],
    tier: Tier,
    engine_list: &[Engine],
    report_dir: Option<&Path>,
    screenshot_dir: Option<&Path>,
    server: bool,
    json: bool,
    port: u16,
    min_coverage: Option<f64>,
    engine_timeout: Option<std::time::Duration>,
    shell_lang: Option<orbital_verify::shell::ShellLang>,
    repo_root: &Path,
    traits: Option<&[String]>,
) -> Result<()> {
    let mut reports = Vec::with_capacity(targets.len());
    let mut worst_exit = 0i32;

    for (idx, target) in targets.iter().enumerate() {
        let item = display(target);
        let mut engine_reports = Vec::new();

        if engine_list.contains(&Engine::Rust) {
            match verify_one(
                target,
                report_dir,
                screenshot_dir,
                server,
                shell_lang,
                tier,
                json,
                traits,
            ) {
                Ok(outcome) => engine_reports.push(engines::fold_rust_report(&outcome)),
                Err(e) => {
                    engine_reports.push(engines::EngineReport::not_available(
                        Engine::Rust,
                        e.to_string(),
                    ));
                }
            }
        }

        if engine_list.contains(&Engine::Runtime) {
            let runtime_port = port.saturating_add(idx as u16);
            engine_reports.push(engines::run_runtime_engine(
                target,
                tier,
                repo_root,
                runtime_port,
                min_coverage,
                engine_timeout,
            ));
        }

        if engine_list.contains(&Engine::Compiled) {
            let compiled_port = port.saturating_add(10).saturating_add(idx as u16);
            let client_port = port.saturating_add(20).saturating_add(idx as u16);
            engine_reports.push(engines::run_compiled_engine(
                target,
                tier,
                repo_root,
                compiled_port,
                client_port,
                engine_timeout,
            ));
        }

        let unified = engines::UnifiedVerifyReport::new(item.clone(), tier, engine_reports);
        worst_exit = worst_exit.max(unified.exit_code());

        // Rust-engine lines are already printed by `verify_one` above (untouched
        // per-item lines); only the node engines get a headline row here.
        for er in unified
            .engines
            .iter()
            .filter(|e| e.engine != Engine::Rust.as_str())
        {
            let unaccounted = unaccounted_checks(er);
            let status = engine_status(er);
            let unresolved = if unaccounted > 0 {
                format!(", {unaccounted} neither passed nor failed")
            } else {
                String::new()
            };
            let cov = er
                .coverage
                .as_ref()
                .map(|c| format!(", coverage {:.1}%", c.ratio * 100.0))
                .unwrap_or_default();
            let detail = er
                .error
                .as_deref()
                .map(|e| format!(" — {e}"))
                .unwrap_or_default();
            emit(
                json,
                format!(
                    "[{}] {status} {}/{} checks{unresolved}{cov}{detail}",
                    er.engine, er.passed, er.checks
                ),
            );
        }
        emit(
            json,
            format!(
                "verify: {item} — {}/{} engines green",
                unified.summary.engines_green, unified.summary.engines_requested
            ),
        );

        reports.push(unified);
    }

    if json {
        let value = if reports.len() == 1 {
            serde_json::to_value(&reports[0])?
        } else {
            serde_json::to_value(&reports)?
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
    }

    if worst_exit != 0 {
        std::process::exit(worst_exit);
    }
    Ok(())
}

/// Outcome of the in-process rust engine for one target — carries the underlying
/// Reports (not just pass/fail) so `commands::engines::fold_rust_report` can build
/// a `EngineReport` with real circuit findings instead of a bare bool.
pub(crate) struct VerifyOneOutcome {
    pub ok: bool,
    pub l0_errors: Vec<String>,
    pub l1: Option<orbital_verify::report::Report>,
    pub l2: Option<orbital_client::render_probe::RenderReport>,
    pub l2_server: Option<orbital_client::render_probe::RenderReport>,
    pub shell: Option<orbital_verify::observers::Verdict>,
}

#[allow(clippy::too_many_arguments)]
fn verify_one(
    path: &Path,
    report_dir: Option<&Path>,
    screenshot_dir: Option<&Path>,
    server: bool,
    shell_lang: Option<orbital_verify::shell::ShellLang>,
    tier: Tier,
    json_mode: bool,
    traits: Option<&[String]>,
) -> Result<VerifyOneOutcome> {
    let item = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Tier gate: smoke = L0+L1 only (no client render, no server probe, no
    // screenshots); deep forces the server probe + a TypeScript shell gate
    // (unless the user already asked for a specific --shell); standard is
    // today's behavior, unchanged.
    let run_l2 = tier != Tier::Smoke;
    let server_effective = match tier {
        Tier::Smoke => false,
        Tier::Standard => server,
        Tier::Deep => true,
    };
    let shell_lang_effective = match tier {
        Tier::Deep => shell_lang.or(Some(orbital_verify::shell::ShellLang::TypeScript)),
        _ => shell_lang,
    };

    // Layer 0 — static validation (the `orb validate` phase: structural + type +
    // config + OIR checks). verify is a strict superset of validate, so a schema
    // that fails static validation fails fast HERE — before being executed
    // through the L1/L2 kernel circuits. Shares the one validation pipeline
    // (`run_full_validation`) with `orb validate`; the two can never diverge.
    {
        let extra_patterns = crate::utils::load_extra_patterns(path, None);
        let service_registry = crate::utils::load_services_registry(None);
        let l0 =
            crate::commands::validate::run_full_validation(path, extra_patterns, service_registry)?;
        let l0_ok = l0.errors.is_empty();
        let l0_icon = if l0_ok { "✓" } else { "✗" };
        emit(
            json_mode,
            format!(
                "[{l0_icon}] {item} — validate: {} error(s), {} warning(s)",
                l0.errors.len(),
                l0.warnings.len()
            ),
        );
        if !l0_ok {
            for e in &l0.errors {
                emit(json_mode, format!("    L0 FAIL {e}"));
            }
            return Ok(VerifyOneOutcome {
                ok: false,
                l0_errors: l0.errors,
                l1: None,
                l2: None,
                l2_server: None,
                shell: None,
            });
        }
    }

    let schema = orbital_verify::corpus::resolve_file(path)?;

    // `--trait NAME`: resolve once against this schema (errors here list the schema's
    // available trait names) and reuse the SAME canonical name(s) for L1 and L2, rather
    // than each layer re-resolving (and potentially disagreeing).
    let resolved_traits = traits
        .map(|names| orbital_verify::planner::resolve_trait_names(&schema, names))
        .transpose()?;

    // Layer 1 — kernel circuit.
    let l1 = orbital_verify::verify_schema_scoped(&item, schema.clone(), resolved_traits.as_deref())?;
    // A transition the app's OWN `@create`/`@update`/`@delete` policy denied is
    // declared behavior, not a defect — `--all-personas` has always bucketed it
    // (`classify_persona_failures`). The default single run did not, so every
    // organism that declares directives reported FAIL for doing exactly what it
    // was told to do. Same bucketing, same helper, both paths.
    let l1_breakdown = l1.classify_persona_failures();
    let l1_ok = l1.ok() || l1_breakdown.ok();
    // `headline()` reports the raw verdict, which reads "[FAIL] … " on a run the
    // bucketing then passes — contradictory next to "verify: 1/1 passed". Restate
    // the status when the only failures were declared denials.
    emit(
        json_mode,
        if !l1.ok() && l1_ok {
            l1.headline().replacen("[FAIL]", "[PASS]", 1)
        } else {
            l1.headline()
        },
    );
    if !l1.ok() && l1_ok {
        emit(
            json_mode,
            format!(
                "    ({} access-denied, bucketed not FAILed: {})",
                l1_breakdown.access_denied.len(),
                l1_breakdown.access_denied.join("; ")
            ),
        );
    }
    for v in &l1.verdicts {
        if !v.passed && !l1_ok {
            emit(json_mode, format!("    L1 FAIL {} — {}", v.name, v.detail));
        }
    }

    // Native-shell compile gate — emit the resolved schema to a temp dir with
    // the requested shell, then run the (toolchain-agnostic) gate from
    // `orbital_verify::shell`. This is the home of the Python `py_compile`
    // gate that used to live in `orbital-verify-unified`, plus android/ios.
    let mut shell_ok = true;
    let mut shell_verdict: Option<orbital_verify::observers::Verdict> = None;
    if let Some(lang) = shell_lang_effective {
        match emit_shell_to_tmp(path, lang) {
            Ok(tmp_dir) => {
                let verdict = orbital_verify::shell::shell_compile_verdict(&tmp_dir, lang);
                let icon = if verdict.passed { "✓" } else { "✗" };
                emit(
                    json_mode,
                    format!(
                        "  [{icon}] shell:{} — {}",
                        lang.display_name(),
                        verdict.detail
                    ),
                );
                if !verdict.passed {
                    shell_ok = false;
                    emit(
                        json_mode,
                        format!("    SHELL FAIL {} — {}", verdict.name, verdict.detail),
                    );
                }
                std::fs::remove_dir_all(&tmp_dir).ok();
                shell_verdict = Some(verdict);
            }
            Err(e) => {
                shell_ok = false;
                emit(
                    json_mode,
                    format!("    SHELL FAIL emit:{} — {e}", lang.display_name()),
                );
                shell_verdict = Some(orbital_verify::observers::Verdict::from_offenders(
                    &format!("shell_emit_{}", lang.display_name()),
                    vec![e.to_string()],
                ));
            }
        }
    }

    // Layer 2 — client render contract (enforces the bindings L1 deferred), in-process
    // kernel. Skipped entirely at the smoke tier (no browser/client layer at all).
    let mut l2: Option<orbital_client::render_probe::RenderReport> = None;
    let mut l2_server: Option<orbital_client::render_probe::RenderReport> = None;
    let mut l2_server_ok = true;
    if run_l2 {
        let report = orbital_client::render_probe::probe_schema_scoped(
            &item,
            schema.clone(),
            resolved_traits.as_deref(),
        )?;
        emit(json_mode, report.headline());
        for v in &report.verdicts {
            if !v.passed {
                emit(json_mode, format!("    L2 FAIL {} — {}", v.name, v.detail));
            }
        }
        if !report.access_scoped_empty.is_empty() {
            emit(
                json_mode,
                format!(
                    "    ({} empty by declared @read, bucketed not FAILed: {})",
                    report.access_scoped_empty.len(),
                    report.access_scoped_empty.join("; ")
                ),
            );
        }

        // Layer 2 (server) — the SAME render contract driven over real HTTP against a
        // live `orbital-server`, verifying server routes + serialization + client together.
        if server_effective {
            let l2s = super::verify_server::probe_over_http(
                &item,
                schema.clone(),
                resolved_traits.as_deref(),
            )?;
            emit(json_mode, format!("{} [server]", l2s.headline()));
            for v in &l2s.verdicts {
                if !v.passed {
                    l2_server_ok = false;
                    emit(
                        json_mode,
                        format!("    L2/server FAIL {} — {}", v.name, v.detail),
                    );
                }
            }
            l2_server = Some(l2s);

            // §156 fixture pass: every recorded events-wire fixture request
            // must round-trip through this same live HTTP server as an
            // `orbital_core::EventResponse` — the wire-shape gate for
            // `orb verify --server` named in Program A's verification plan.
            let orbital_name = schema
                .orbitals
                .first()
                .map(|o| o.name.clone())
                .unwrap_or_else(|| item.clone());
            let fixture_report = super::verify_server::run_fixture_pass(&orbital_name, &schema)?;
            if fixture_report.skipped {
                emit(json_mode, "    L2/server fixture-pass SKIPPED (no fixtures dir)".to_string());
            } else {
                emit(
                    json_mode,
                    format!(
                        "    L2/server fixture-pass {}/{} — {}",
                        fixture_report.checked.len(),
                        fixture_report.checked.len() + fixture_report.failures.len(),
                        if fixture_report.is_green() { "green" } else { "FAIL" }
                    ),
                );
                if !fixture_report.is_green() {
                    l2_server_ok = false;
                    for failure in &fixture_report.failures {
                        emit(json_mode, format!("    L2/server fixture-pass FAIL — {failure}"));
                    }
                }
            }
        }

        // Layer 3 — headless screenshot (opt-in; only when the binary was built with it).
        if let Some(dir) = screenshot_dir {
            #[cfg(feature = "screenshot")]
            match orbital_client::screenshot::screenshot_schema(&item, schema.clone(), dir) {
                Ok(paths) => {
                    for p in paths {
                        emit(json_mode, format!("    📸 {}", p.display()));
                    }
                }
                Err(e) => emit(json_mode, format!("    📸 screenshot FAILED — {e}")),
            }
            #[cfg(not(feature = "screenshot"))]
            {
                let _ = dir;
                emit(
                    json_mode,
                    "    📸 (rebuild orb with `--features screenshot` to capture)",
                );
            }
        }

        l2 = Some(report);
    }

    if let Some(dir) = report_dir {
        std::fs::create_dir_all(dir).ok();
        let report = serde_json::json!({
            "item": item,
            "layer1": l1.to_json(),
            "layer2": serde_json::to_value(&l2).unwrap_or(serde_json::Value::Null),
            "traits": resolved_traits,
        });
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(dir.join(format!("{item}.verify-report.json")), json).ok();
        }
    }

    let ok = l1_ok && l2.as_ref().is_none_or(|r| r.ok()) && l2_server_ok && shell_ok;

    Ok(VerifyOneOutcome {
        ok,
        l0_errors: Vec::new(),
        l1: Some(l1),
        l2,
        l2_server,
        shell: shell_verdict,
    })
}

/// Map the `--shell` arg value to a native compile-gate language. Errors on an
/// unknown value (fail-fast, before any target walk).
fn parse_shell_lang(s: &str) -> Result<orbital_verify::shell::ShellLang> {
    use orbital_verify::shell::ShellLang;
    match s {
        "python" => Ok(ShellLang::Python),
        "android" => Ok(ShellLang::Kotlin),
        "ios" => Ok(ShellLang::Swift),
        "rust" => Ok(ShellLang::Rust),
        "typescript" | "ts" => Ok(ShellLang::TypeScript),
        other => anyhow::bail!(
            "unknown --shell {other:?} (expected: python | android | ios | rust | typescript)"
        ),
    }
}

/// Emit the schema at `path` to a fresh temp dir using the native shell that
/// matches `lang`, returning the dir. Reuses the in-process `generate_from_file`
/// path that `orb compile --shell X` uses — no codegen reimplementation, no
/// shelling out to the `orbital` binary.
fn emit_shell_to_tmp(path: &Path, lang: orbital_verify::shell::ShellLang) -> Result<PathBuf> {
    use orbital_compiler::{generate_from_file, GenerateOptions};
    use orbital_verify::shell::ShellLang;

    let tmp_dir = std::env::temp_dir().join(format!(
        "orbital-verify-shell-emit-{}-{}",
        lang.display_name(),
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("schema")
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir)?;

    // `generate_from_file` reads `.orb` only; a `.lolo` target lowers through
    // L1 into a sibling temp `.orb` first — the same source `orb validate`
    // and the L1/L2 walk already accepted.
    let lowered: PathBuf;
    let path: &Path = if path.extension().and_then(|e| e.to_str()) == Some("lolo") {
        let schema = orbital_lolo::parse_and_lower(path)
            .map_err(|e| anyhow::anyhow!("lower {}: {e}", path.display()))?;
        lowered = tmp_dir.join("lowered.orb");
        std::fs::write(&lowered, serde_json::to_string(&schema)?)?;
        &lowered
    } else {
        path
    };

    let options = GenerateOptions {
        // from_env, not Default — same dev-registry rule as `orb compile`
        // (C-COMPILE-IGNORES-DEV-REGISTRIES).
        compiler: orbital_compiler::CompilerOptions::from_env(),
        copy_template: true,
        clean: false,
        verbose: false,
        mode: orbital_compiler::ShellMode::Full,
        ..Default::default()
    };

    match lang {
        ShellLang::Python => {
            use orbital_compiler::shell::python_shell;
            use orbital_shell_python::PythonBackend;
            let shells_dir = crate::utils::find_shells_dir();
            let shell = python_shell(&shells_dir);
            let backend = PythonBackend::new();
            generate_from_file(path, &tmp_dir, &shell, &backend, &options)?;
        }
        ShellLang::Kotlin => {
            use orbital_shell_android::{AndroidBackend, AndroidShell};
            let shell = AndroidShell::new();
            let backend = AndroidBackend::new();
            generate_from_file(path, &tmp_dir, &shell, &backend, &options)?;
        }
        ShellLang::Swift => {
            use orbital_shell_ios::{IosBackend, IosShell};
            let shell = IosShell::new();
            let backend = IosBackend::new();
            generate_from_file(path, &tmp_dir, &shell, &backend, &options)?;
        }
        ShellLang::Rust => {
            use orbital_shell_rust::{RustBackend, RustShell};
            let shell = RustShell::new();
            let backend = RustBackend::new();
            generate_from_file(path, &tmp_dir, &shell, &backend, &options)?;
        }
        ShellLang::TypeScript => {
            use orbital_compiler::shell::typescript_shell;
            use orbital_shell_typescript::TypeScriptBackend;
            let shells_dir = crate::utils::find_shells_dir();
            let shell = typescript_shell(&shells_dir);
            let backend = TypeScriptBackend::new();
            generate_from_file(path, &tmp_dir, &shell, &backend, &options)?;
        }
    }
    Ok(tmp_dir)
}

/// Per-frame (state-transition) screenshot capture: for the primary orbital's richest
/// trait, drive each planned transition in sequence and capture the resulting UI, writing
/// a `frames.json` manifest (the input to the vision-review workflow). Returns true on success.
#[cfg(feature = "screenshot")]
pub(crate) fn run_screenshot_frames(path: &Path, out_dir: &Path) -> Result<bool> {
    use orbital_client::bridge::{KernelBridge, ServerBridge};
    use std::sync::Arc;

    let schema = orbital_verify::corpus::resolve_file(path)?;
    let item = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
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
    let wl_entities = orbital_verify::planner::entity_names(&schema);
    let steps = orbital_verify::planner::plan_trait(td, &wl_entities);

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
        let bridge = Arc::new(
            KernelBridge::new(schema.clone()).map_err(|e| anyhow::anyhow!("bridge: {e}"))?,
        );
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
        println!(
            "    📸 frame {idx} INIT → {} — {}",
            app.current_state_name(),
            png.display()
        );
        idx += 1;
    }

    // One hermetic frame per planned transition (replay to `from`, then fire the event).
    for step in steps.iter().take(10) {
        let mut app = build_app()?;
        if let Some(path) = orbital_verify::planner::replay_path(td, &step.from, &wl_entities) {
            for (ev, pl) in path {
                let v = serde_json::from_value(pl).unwrap_or(orbital_core::Value::Null);
                app.drive_blocking(&ev, Some(&trait_name), v);
            }
        }
        let payload =
            serde_json::from_value(step.payload.clone()).unwrap_or(orbital_core::Value::Null);
        app.drive_blocking(&step.event, Some(&trait_name), payload);
        // The planner's expected target for THIS trait (current_state_name tracks the
        // active/first trait, not the one being driven).
        let to = step.to.clone();
        let png = out_dir.join(format!("{item}.frame{idx:02}.{}.png", step.event));
        orbital_client::screenshot::capture(&mut app, &png)?;
        entry(idx, &step.from, &step.event, &to, &png);
        println!(
            "    📸 frame {idx} {} --{}--> {to} — {}",
            step.from,
            step.event,
            png.display()
        );
        idx += 1;
    }

    let manifest = serde_json::json!({ "item": item, "trait": td.name, "frames": frames });
    let mpath = out_dir.join(format!("{item}.frames.json"));
    std::fs::write(&mpath, serde_json::to_string_pretty(&manifest)?)?;
    println!("    📋 manifest: {}", mpath.display());
    Ok(true)
}

/// Resolve a TARGET argument: an existing path is used as-is; an explicit
/// `.lolo`/`.orb` path that doesn't exist is passed through unchanged (the normal
/// L0/resolve failure downstream reports it, same as before name resolution
/// existed); anything else is looked up by file stem across the registry corpus
/// — a unique match resolves, zero or ambiguous matches are a hard error listing
/// the candidates (deterministic order, since `discover_registry` sorts).
fn resolve_target(raw: PathBuf, repo_root: &Path) -> Result<PathBuf> {
    if raw.exists() {
        return Ok(raw);
    }
    let ext = raw.extension().and_then(|e| e.to_str());
    if matches!(ext, Some("lolo") | Some("orb")) {
        return Ok(raw);
    }

    let stem = raw.to_string_lossy().into_owned();
    let mut candidates: Vec<PathBuf> = orbital_verify::corpus::discover_registry(repo_root)
        .into_iter()
        .filter(|p| p.file_stem().and_then(|s| s.to_str()) == Some(stem.as_str()))
        .collect();
    candidates.sort();

    match candidates.len() {
        0 => anyhow::bail!(
            "no behavior named {stem:?} found under {}",
            orbital_verify::corpus::REGISTRY_ROOTS.join(" or ")
        ),
        1 => Ok(candidates.into_iter().next().expect("len checked")),
        _ => {
            let list: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            anyhow::bail!(
                "ambiguous behavior name {stem:?} — {} matches:\n  {}",
                list.len(),
                list.join("\n  ")
            )
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_registry(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("orbital-verify-target-resolve-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("packages/almadar-std/behaviors/registry/ui/atoms"))
            .unwrap();
        root
    }

    #[test]
    fn resolve_target_existing_path_passes_through() {
        let root = tmp_registry("existing-path");
        let file = root.join("some-file.orb");
        std::fs::write(&file, "{}").unwrap();
        let resolved = resolve_target(file.clone(), &root).unwrap();
        assert_eq!(resolved, file);
    }

    #[test]
    fn resolve_target_missing_schema_extension_passes_through() {
        let root = tmp_registry("missing-schema-ext");
        let missing = root.join("does-not-exist.orb");
        let resolved = resolve_target(missing.clone(), &root).unwrap();
        assert_eq!(resolved, missing);
    }

    #[test]
    fn resolve_target_unique_name_resolves() {
        let root = tmp_registry("unique-name");
        let file =
            root.join("packages/almadar-std/behaviors/registry/ui/atoms/ui-timer-display.orb");
        std::fs::write(&file, "{}").unwrap();
        let resolved = resolve_target(PathBuf::from("ui-timer-display"), &root).unwrap();
        assert_eq!(resolved, file);
    }

    #[test]
    fn resolve_target_missing_name_errors() {
        let root = tmp_registry("missing-name");
        let err = resolve_target(PathBuf::from("no-such-behavior"), &root).unwrap_err();
        assert!(err.to_string().contains("no behavior named"));
    }

    #[test]
    fn resolve_target_ambiguous_name_errors_with_deterministic_candidates() {
        let root = tmp_registry("ambiguous-name");
        std::fs::create_dir_all(
            root.join("packages/almadar-behaviors/behaviors/registry/ui/atoms"),
        )
        .unwrap();
        let a = root.join("packages/almadar-std/behaviors/registry/ui/atoms/dupe-name.orb");
        let b = root.join("packages/almadar-behaviors/behaviors/registry/ui/atoms/dupe-name.orb");
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, "{}").unwrap();
        let err = resolve_target(PathBuf::from("dupe-name"), &root).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous behavior name"));
        // Deterministic order: candidates sorted lexicographically (almadar-behaviors < almadar-std).
        let behaviors_idx = msg.find("almadar-behaviors").unwrap();
        let std_idx = msg.find("almadar-std").unwrap();
        assert!(behaviors_idx < std_idx);
    }

    #[test]
    fn tier_gate_smoke_skips_l2_and_server() {
        // Direct decision-table check (no schema execution needed): smoke never
        // runs L2, regardless of --server; deep always forces it; standard mirrors
        // the caller's flag.
        let run_l2 = |tier: Tier| tier != Tier::Smoke;
        assert!(!run_l2(Tier::Smoke));
        assert!(run_l2(Tier::Standard));
        assert!(run_l2(Tier::Deep));

        let server_effective = |tier: Tier, server: bool| match tier {
            Tier::Smoke => false,
            Tier::Standard => server,
            Tier::Deep => true,
        };
        assert!(!server_effective(Tier::Smoke, true));
        assert!(!server_effective(Tier::Standard, false));
        assert!(server_effective(Tier::Standard, true));
        assert!(server_effective(Tier::Deep, false));
    }

    #[test]
    fn tier_gate_deep_defaults_shell_to_typescript_unless_explicit() {
        let shell_lang_effective =
            |tier: Tier, shell: Option<orbital_verify::shell::ShellLang>| match tier {
                Tier::Deep => shell.or(Some(orbital_verify::shell::ShellLang::TypeScript)),
                _ => shell,
            };
        assert_eq!(
            shell_lang_effective(Tier::Deep, None),
            Some(orbital_verify::shell::ShellLang::TypeScript)
        );
        assert_eq!(
            shell_lang_effective(Tier::Deep, Some(orbital_verify::shell::ShellLang::Python)),
            Some(orbital_verify::shell::ShellLang::Python)
        );
        assert_eq!(shell_lang_effective(Tier::Standard, None), None);
        assert_eq!(shell_lang_effective(Tier::Smoke, None), None);
    }
}

/// Checks an engine reported that were neither passed nor failed.
///
/// `runtime-verify` buckets every result as `pass` / `error` / **`warning`**
/// (`buildJsonReport`), and `total` counts all three — so a warning-only run
/// arrives here as `passed 0, failed 0, checks 1`.
fn unaccounted_checks(er: &engines::EngineReport) -> usize {
    er.checks.saturating_sub(er.passed + er.failed)
}

/// The verdict word for one engine's row.
///
/// `failed == 0` alone used to print `PASS`, so a warning-only run said
/// `PASS 0/1 checks` — nothing passed, nothing failed, and the word said green
/// (`V-RUNTIME-ZERO-CHECK-VERDICT-INCONSISTENT`). A warning is still not a
/// failure: `engines_green` and the exit code are unchanged, and only the
/// verdict stops claiming a pass it does not have.
fn engine_status(er: &engines::EngineReport) -> &'static str {
    if !er.ran {
        "ERROR"
    } else if er.failed > 0 {
        "FAIL"
    } else if unaccounted_checks(er) > 0 {
        "WARN"
    } else {
        "PASS"
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn report(ran: bool, passed: usize, failed: usize, checks: usize) -> engines::EngineReport {
        engines::EngineReport {
            engine: "runtime".to_string(),
            ran,
            passed,
            failed,
            checks,
            coverage: None,
            circuit_findings: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn every_check_passed_is_a_pass() {
        assert_eq!(engine_status(&report(true, 3, 0, 3)), "PASS");
        assert_eq!(unaccounted_checks(&report(true, 3, 0, 3)), 0);
    }

    #[test]
    fn any_failure_is_a_fail_even_beside_unaccounted_checks() {
        assert_eq!(engine_status(&report(true, 0, 1, 3)), "FAIL");
    }

    /// The regression: nothing passed, nothing failed, one check reported.
    #[test]
    fn a_warning_only_run_is_warn_not_a_silent_pass() {
        let er = report(true, 0, 0, 1);
        assert_eq!(engine_status(&er), "WARN");
        assert_eq!(unaccounted_checks(&er), 1);
    }

    #[test]
    fn an_engine_that_never_ran_outranks_everything() {
        assert_eq!(engine_status(&report(false, 0, 0, 0)), "ERROR");
    }

    /// A zero-check run that RAN is vacuously green — no result was withheld.
    #[test]
    fn zero_checks_is_not_a_warning() {
        assert_eq!(engine_status(&report(true, 0, 0, 0)), "PASS");
    }
}
