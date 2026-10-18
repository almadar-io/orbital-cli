//! `orb verify` unified engine orchestration.
//!
//! The runtime and compiled engines are separate node CLIs (`tools/runtime-verify`,
//! `tools/runtime-verify --compiled`) that play the SAME circuit the in-process rust
//! engine plays, just in a different place (`docs/Almadar_Verification.md`). This
//! module resolves + spawns them under a tier-scaled watchdog, parses their JSON
//! reports, and folds every engine's findings into one `UnifiedVerifyReport`
//! speaking the shared circuit vocabulary.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::utils::find_bun;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Engine {
    Rust,
    Runtime,
    Compiled,
}

impl Engine {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Engine::Rust => "rust",
            Engine::Runtime => "runtime",
            Engine::Compiled => "compiled",
        }
    }
}

/// A native device the compiled engine can walk instead of the web client
/// (`tools/runtime-verify --compiled --target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeTarget {
    Ios,
    Android,
}

impl NativeTarget {
    /// The `--target` value `tools/runtime-verify --compiled` takes.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            NativeTarget::Ios => "ios",
            NativeTarget::Android => "android",
        }
    }

    /// Engine label in the unified report: `compiled:ios` / `compiled:android`.
    pub(crate) fn engine_label(self) -> String {
        format!("{}:{}", Engine::Compiled.as_str(), self.as_str())
    }

    /// The device a `--shell` compile gate language runs on, if it has one.
    pub(crate) fn from_shell_lang(lang: orbital_verify::shell::ShellLang) -> Option<Self> {
        use orbital_verify::shell::ShellLang;
        match lang {
            ShellLang::Swift => Some(NativeTarget::Ios),
            ShellLang::Kotlin => Some(NativeTarget::Android),
            _ => None,
        }
    }
}

/// The engine label for a compiled-engine run: plain `compiled`, or `compiled:<target>`.
fn compiled_label(native: Option<NativeTarget>) -> String {
    native.map_or_else(
        || Engine::Compiled.as_str().to_string(),
        NativeTarget::engine_label,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Smoke,
    Standard,
    Deep,
}

impl Tier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tier::Smoke => "smoke",
            Tier::Standard => "standard",
            Tier::Deep => "deep",
        }
    }

    /// Watchdog budget for one engine invocation at this tier.
    fn timeout(self) -> Duration {
        match self {
            Tier::Smoke => Duration::from_secs(120),
            Tier::Standard => Duration::from_secs(600),
            Tier::Deep => Duration::from_secs(1800),
        }
    }
}

/// The shared circuit vocabulary (`docs/Almadar_Verification.md` → "Vocabulary
/// unification"). Anything not listed here passes through with its own engine
/// term preserved — the table only grows toward convergence, it never hides a term.
fn translate_circuit_term(key: &str) -> String {
    match key {
        "clickNoListener" => "unsubscribed emit (missing listens)",
        "replayDiverged" | "replay-diverged" => "circuit divergence",
        "portalSweep" | "portalPerStep" | "portal" => "render-slot check",
        "guardParity" => "guard divergence",
        "dataMutation" => "entity mutation",
        "preconditionSkipped" => "transition skipped (precondition unreachable)",
        other => other,
    }
    .to_string()
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CoverageInfo {
    pub covered: usize,
    pub total: usize,
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CircuitFinding {
    pub check: String,
    pub engine_term: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EngineReport {
    pub engine: String,
    pub ran: bool,
    /// `"ran"` | `"not_installed"` | `"error"` — finer-grained than `ran` alone,
    /// so a missing npm package (fixable with `orb engines install`) reads
    /// differently from a genuine spawn/timeout/parse failure.
    pub status: &'static str,
    pub passed: usize,
    pub failed: usize,
    pub checks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageInfo>,
    pub circuit_findings: Vec<CircuitFinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl EngineReport {
    /// An exit-2-class outcome: the engine itself never produced a verdict
    /// (spawn/timeout/parse/not-found) — never conflated with a defect (exit 1).
    pub(crate) fn not_available(engine: Engine, error: String) -> Self {
        Self::not_available_as(engine.as_str().to_string(), error)
    }

    /// `not_available` under an explicit engine label (`compiled:ios`).
    pub(crate) fn not_available_as(engine: String, error: String) -> Self {
        EngineReport {
            engine,
            ran: false,
            status: "error",
            passed: 0,
            failed: 0,
            checks: 0,
            coverage: None,
            circuit_findings: Vec::new(),
            error: Some(error),
        }
    }

    /// The engine's npm package is not installed — the actionable rung below
    /// `not_available`, pointing at `orb engines install` instead of a bare error.
    pub(crate) fn not_installed(engine: Engine, package: &str) -> Self {
        EngineReport {
            engine: engine.as_str().to_string(),
            ran: false,
            status: "not_installed",
            passed: 0,
            failed: 0,
            checks: 0,
            coverage: None,
            circuit_findings: Vec::new(),
            error: Some(not_installed_message(package)),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnifiedSummary {
    pub engines_requested: usize,
    pub engines_green: usize,
    pub total_checks: usize,
    pub passed: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnifiedVerifyReport {
    pub item: String,
    pub tier: String,
    pub engines: Vec<EngineReport>,
    pub summary: UnifiedSummary,
    pub verdict: String,
}

impl UnifiedVerifyReport {
    pub(crate) fn new(item: String, tier: Tier, engines: Vec<EngineReport>) -> Self {
        let engines_requested = engines.len();
        let engines_green = engines.iter().filter(|e| e.ran && e.failed == 0).count();
        let total_checks = engines.iter().map(|e| e.checks).sum();
        let passed = engines.iter().map(|e| e.passed).sum();
        let failed = engines.iter().map(|e| e.failed).sum();
        let verdict = if engines.iter().any(|e| !e.ran) {
            "engine-error"
        } else if failed > 0 {
            "failed"
        } else {
            "green"
        };
        UnifiedVerifyReport {
            item,
            tier: tier.as_str().to_string(),
            engines,
            summary: UnifiedSummary {
                engines_requested,
                engines_green,
                total_checks,
                passed,
                failed,
            },
            verdict: verdict.to_string(),
        }
    }

    /// 0 = every requested engine ran green · 1 = engines ran, ≥1 check failed ·
    /// 2 = an engine could not run at all (infra — takes precedence over 1).
    pub(crate) fn exit_code(&self) -> i32 {
        if self.engines.iter().any(|e| !e.ran) {
            2
        } else if self.summary.failed > 0 {
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Rust engine fold (in-process Report/RenderReport → EngineReport)
// ---------------------------------------------------------------------------

/// Fold the in-process rust verify outcome (`commands::verify::VerifyOneOutcome`)
/// into the shared `EngineReport` shape. Verdict names translate through the same
/// vocabulary table where applicable (today none coincide — the rust observer names
/// are its own, e.g. `state_transition` — so most pass through unchanged until the
/// vocabularies converge further); the frame count is reported as "played transitions".
pub(crate) fn fold_rust_report(
    outcome: &crate::commands::verify::VerifyOneOutcome,
) -> EngineReport {
    let Some(l1) = outcome.l1.as_ref() else {
        // L0 (static validate) failed before L1 ever ran — a real defect (exit-1
        // class), not an engine-availability problem, so still `ran: true`.
        let findings: Vec<CircuitFinding> = outcome
            .l0_errors
            .iter()
            .map(|e| CircuitFinding {
                check: "validate".to_string(),
                engine_term: "validate".to_string(),
                detail: e.clone(),
            })
            .collect();
        let failed = findings.len().max(1);
        return EngineReport {
            engine: Engine::Rust.as_str().to_string(),
            ran: true,
            status: "ran",
            passed: 0,
            failed,
            checks: failed,
            coverage: None,
            circuit_findings: findings,
            error: None,
        };
    };

    let mut checks = l1.verdicts.len();
    let mut passed = l1.verdicts.iter().filter(|v| v.passed).count();
    let mut failed = checks - passed;
    let mut findings: Vec<CircuitFinding> = l1
        .verdicts
        .iter()
        .filter(|v| !v.passed)
        .map(|v| CircuitFinding {
            check: v.name.clone(),
            engine_term: translate_circuit_term(&v.name),
            detail: v.detail.clone(),
        })
        .collect();
    findings.push(CircuitFinding {
        check: "frames".to_string(),
        engine_term: "played transitions".to_string(),
        detail: format!("{} frame(s) played", l1.summary.frames),
    });

    if let Some(r) = &outcome.l2 {
        checks += r.verdicts.len();
        passed += r.verdicts.iter().filter(|v| v.passed).count();
        failed += r.verdicts.iter().filter(|v| !v.passed).count();
        findings.extend(
            r.verdicts
                .iter()
                .filter(|v| !v.passed)
                .map(|v| CircuitFinding {
                    check: v.name.clone(),
                    engine_term: translate_circuit_term(&v.name),
                    detail: v.detail.clone(),
                }),
        );
    }
    if let Some(r) = &outcome.l2_server {
        checks += r.verdicts.len();
        passed += r.verdicts.iter().filter(|v| v.passed).count();
        failed += r.verdicts.iter().filter(|v| !v.passed).count();
        findings.extend(
            r.verdicts
                .iter()
                .filter(|v| !v.passed)
                .map(|v| CircuitFinding {
                    check: format!("{}:server", v.name),
                    engine_term: translate_circuit_term(&v.name),
                    detail: v.detail.clone(),
                }),
        );
    }
    if let Some(v) = &outcome.shell {
        checks += 1;
        if v.passed {
            passed += 1;
        } else {
            failed += 1;
            findings.push(CircuitFinding {
                check: v.name.clone(),
                engine_term: translate_circuit_term(&v.name),
                detail: v.detail.clone(),
            });
        }
    }

    EngineReport {
        engine: Engine::Rust.as_str().to_string(),
        ran: true,
        status: "ran",
        passed,
        failed,
        checks,
        coverage: None,
        circuit_findings: findings,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Engine resolution + spawn watchdog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedCommand {
    /// Run on bun (`<bun> <entry> <args…>`) — the normal case, whether the
    /// entry came from an env override or an installed npm package's `bin`.
    Bun(PathBuf),
    Exe(PathBuf),
}

/// Why an engine could not be resolved to a runnable command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EngineError {
    /// The engine's npm package is not installed anywhere on the node-style
    /// walk-up from the working directory (or the user store).
    NotInstalled { package: String },
    /// The package is installed but unusable (e.g. no matching `bin` entry).
    Invalid { package: String, reason: String },
    /// An explicit env override (or the bun runtime itself) could not be resolved.
    Override(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::NotInstalled { package } => {
                write!(f, "{}", not_installed_message(package))
            }
            EngineError::Invalid { package, reason } => {
                write!(f, "ORB_ENGINE_INVALID: `{package}` {reason}")
            }
            EngineError::Override(msg) => write!(f, "{msg}"),
        }
    }
}

/// The shared "engine not installed" hint — CLI text, the JSON `error` field,
/// and `orb engines list` rows all read through this one function.
pub(crate) fn not_installed_message(package: &str) -> String {
    format!(
        "ORB_ENGINE_NOT_INSTALLED: `{package}` is not installed — run `orb engines install` (or `--global`)"
    )
}

/// Resolve an engine command: (1) an explicit env override — if set, it ALWAYS
/// wins (valid or not; an invalid override is a hard `Err`, never silently
/// skipped), mirroring the existing `ORBITAL_BIN → PATH → …` precedent
/// (`docs/Almadar_Verification.md`'s orbital-verify.sh resolution); (2) the
/// engine's pinned npm package, resolved node-style from `start_dir` (walk-up)
/// with `user_store` as the final rung — its declared `bin` entry is run on
/// bun. Never panics — an unresolvable engine is a plain `Err(EngineError)`
/// the caller folds into `EngineReport::not_available`/`not_installed`.
/// `(package, bin entry name, pinned version)` for a spawnable engine —
/// shared by `resolve_node_engine` and `orb engines` (install/list/update).
pub(crate) fn engine_pin(engine: Engine) -> (&'static str, &'static str, &'static str) {
    match engine {
        Engine::Runtime => {
            let (pkg, ver) = crate::baked_engine_pins::RUNTIME_VERIFY_ENGINE;
            (pkg, "runtime-verify", ver)
        }
        Engine::Compiled => {
            let (pkg, ver) = crate::baked_engine_pins::COMPILED_VERIFY_ENGINE;
            (pkg, "orbital-verify", ver)
        }
        Engine::Rust => unreachable!("rust engine runs in-process, never spawned"),
    }
}

fn env_override_var(engine: Engine) -> &'static str {
    match engine {
        Engine::Runtime => "ORB_RUNTIME_VERIFY_BIN",
        Engine::Compiled => "ORB_COMPILED_VERIFY_BIN",
        Engine::Rust => unreachable!("rust engine runs in-process, never spawned"),
    }
}

fn resolve_node_engine(
    engine: Engine,
    start_dir: &Path,
    user_store: &Path,
) -> std::result::Result<ResolvedCommand, EngineError> {
    let env_var = env_override_var(engine);
    let (package, bin_name, _pinned_version) = engine_pin(engine);

    if let Ok(over) = std::env::var(env_var) {
        let p = PathBuf::from(&over);
        if !p.exists() {
            return Err(EngineError::Override(format!(
                "{env_var}={over} does not exist"
            )));
        }
        if p.extension().and_then(|e| e.to_str()) == Some("js") {
            return Ok(ResolvedCommand::Bun(p));
        }
        return Ok(ResolvedCommand::Exe(p));
    }

    let npm_pkg = orbital_core::behavior_packages::find_npm_package(package, start_dir, user_store)
        .ok_or_else(|| EngineError::NotInstalled {
            package: package.to_string(),
        })?;
    match npm_pkg.bin.get(bin_name) {
        Some(entry) => Ok(ResolvedCommand::Bun(entry.clone())),
        None => Err(EngineError::Invalid {
            package: package.to_string(),
            reason: format!("installed package has no `{bin_name}` bin entry"),
        }),
    }
}

fn build_command(
    resolved: ResolvedCommand,
    args: &[String],
) -> std::result::Result<Command, EngineError> {
    let mut cmd = match resolved {
        ResolvedCommand::Bun(entry) => {
            let bun = find_bun()
                .map_err(|e| EngineError::Override(format!("bun runtime not found: {e}")))?;
            let mut c = Command::new(bun);
            c.arg(entry);
            c
        }
        ResolvedCommand::Exe(path) => Command::new(path),
    };
    cmd.args(args);
    Ok(cmd)
}

/// `ORBITAL_BIN` points the spawned engines back at THIS binary (so they never shell
/// out to a stale installed `orb`); `ALMADAR_DEV`/`ALMADAR_ROOT` are inherited so
/// the dev-workspace registry resolution the two engines rely on stays in effect.
///
/// `NODE_OPTIONS` gets a heap floor because these engines parse the behavior
/// corpus, which sits close enough to node's default old-space that a run dies
/// with `Reached heap limit` before it verifies anything — and the death is
/// invisible from here, surfacing only as an unparseable empty report
/// (RV-CATALOG-LOAD-OOM). We spawn `node <engine>/dist/index.js` directly, so a
/// heap default set in the engine's own package scripts never reaches it. An
/// explicit caller value always wins.
fn apply_shared_env(cmd: &mut Command) {
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("ORBITAL_BIN", exe);
    }
    for var in ["ALMADAR_DEV", "ALMADAR_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    let node_options = match std::env::var("NODE_OPTIONS") {
        Ok(v) if v.contains("--max-old-space-size") => v,
        Ok(v) if !v.trim().is_empty() => format!("{v} {NODE_HEAP_FLOOR}"),
        _ => NODE_HEAP_FLOOR.to_string(),
    };
    cmd.env("NODE_OPTIONS", node_options);
}

/// Heap floor handed to spawned node engines. See `apply_shared_env`.
const NODE_HEAP_FLOOR: &str = "--max-old-space-size=8192";

struct SpawnOutcome {
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Spawn + poll `Child::try_wait` under a budget, killing on timeout. Pipes are
/// drained on background threads so a chatty child can never deadlock on a full
/// pipe buffer while we're busy polling.
fn run_with_timeout(mut cmd: Command, budget: Duration) -> Result<SpawnOutcome> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child: Child = cmd.spawn().context("failed to spawn engine process")?;
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");

    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let timed_out = loop {
        if child
            .try_wait()
            .context("polling engine process")?
            .is_some()
        {
            break false;
        }
        if start.elapsed() > budget {
            let _ = child.kill();
            let _ = child.wait();
            break true;
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    Ok(SpawnOutcome {
        timed_out,
        stdout,
        stderr,
    })
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

// ---------------------------------------------------------------------------
// Runtime engine (tools/runtime-verify)
// ---------------------------------------------------------------------------

/// Tier → runtime-verify walk flags. Smoke/standard ride the tool's
/// frontier default (authored topology walked in full, imported topology
/// verified at its source atom); deep requests the exhaustive DFS.
fn runtime_tier_flags(tier: Tier) -> &'static [&'static str] {
    match tier {
        Tier::Smoke => &["--first-frame", "--fast"],
        Tier::Standard => &["--fast"],
        Tier::Deep => &["--full-walk"],
    }
}

pub(crate) fn run_runtime_engine(
    target: &Path,
    tier: Tier,
    start_dir: &Path,
    user_store: &Path,
    port: u16,
    min_coverage: Option<f64>,
    engine_timeout: Option<Duration>,
) -> EngineReport {
    let resolved = match resolve_node_engine(Engine::Runtime, start_dir, user_store) {
        Ok(r) => r,
        Err(EngineError::NotInstalled { package }) => {
            return EngineReport::not_installed(Engine::Runtime, &package)
        }
        Err(e) => return EngineReport::not_available(Engine::Runtime, e.to_string()),
    };

    let item = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("item");
    let out_dir = std::env::temp_dir().join(format!(
        "orbital-verify-runtime-{item}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return EngineReport::not_available(
            Engine::Runtime,
            format!("cannot create output dir {}: {e}", out_dir.display()),
        );
    }

    let mut args = vec![
        target.display().to_string(),
        "--headless".to_string(),
        "--json".to_string(),
        "--output".to_string(),
        out_dir.display().to_string(),
        "--server-port".to_string(),
        port.to_string(),
    ];
    for flag in runtime_tier_flags(tier) {
        args.push((*flag).to_string());
    }
    if let Some(mc) = min_coverage {
        args.push("--min-coverage".to_string());
        args.push(mc.to_string());
    }

    let mut cmd = match build_command(resolved, &args) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&out_dir);
            return EngineReport::not_available(Engine::Runtime, e.to_string());
        }
    };
    apply_shared_env(&mut cmd);

    let budget = engine_timeout.unwrap_or_else(|| tier.timeout());
    let outcome = match run_with_timeout(cmd, budget) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&out_dir);
            return EngineReport::not_available(Engine::Runtime, format!("{e}"));
        }
    };
    let _ = std::fs::remove_dir_all(&out_dir);

    if outcome.timed_out {
        return EngineReport::not_available(
            Engine::Runtime,
            format!("timed out after {budget:?} (override with --engine-timeout)"),
        );
    }

    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let doc: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            let stderr = String::from_utf8_lossy(&outcome.stderr);
            return EngineReport::not_available(
                Engine::Runtime,
                format!(
                    "could not parse JSON report: {e} (stderr: {})",
                    first_line(&stderr)
                ),
            );
        }
    };

    fold_runtime_json(&doc)
}

/// runtime-verify formats a check failure as `"<verdictKey>: <detail>"`
/// (`verdictToChecks` → `errors.push(...)`); frame-level errors are
/// `"frame N: <message>"` with no vocabulary key to translate.
fn is_vocab_key(key: &str) -> bool {
    matches!(
        key,
        "clickNoListener"
            | "replayDiverged"
            | "portalSweep"
            | "portalPerStep"
            | "portal"
            | "guardParity"
            | "dataMutation"
            | "preconditionSkipped"
    )
}

fn parse_runtime_finding(text: &str) -> CircuitFinding {
    match text.split_once(": ") {
        Some((key, detail)) if is_vocab_key(key) => CircuitFinding {
            check: key.to_string(),
            engine_term: translate_circuit_term(key),
            detail: detail.to_string(),
        },
        _ => CircuitFinding {
            check: "runtime".to_string(),
            engine_term: text.to_string(),
            detail: text.to_string(),
        },
    }
}

fn fold_runtime_json(doc: &serde_json::Value) -> EngineReport {
    let summary = &doc["summary"];
    let checks = summary["total"].as_u64().unwrap_or(0) as usize;
    let passed = summary["pass"].as_u64().unwrap_or(0) as usize;
    let failed = summary["error"].as_u64().unwrap_or(0) as usize;

    let coverage = doc.get("coverage").and_then(|c| {
        Some(CoverageInfo {
            covered: c.get("covered")?.as_u64()? as usize,
            total: c.get("total")?.as_u64()? as usize,
            ratio: c.get("ratio")?.as_f64()?,
        })
    });

    let mut findings = Vec::new();
    if let Some(results) = doc["results"].as_array() {
        for result in results {
            if let Some(errors) = result["errors"].as_array() {
                for e in errors {
                    if let Some(text) = e.as_str() {
                        findings.push(parse_runtime_finding(text));
                    }
                }
            }
        }
    }

    EngineReport {
        engine: Engine::Runtime.as_str().to_string(),
        ran: true,
        status: "ran",
        passed,
        failed,
        checks,
        coverage,
        circuit_findings: findings,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Compiled engine (tools/runtime-verify --compiled)
// ---------------------------------------------------------------------------

/// The argv `tools/runtime-verify --compiled` is spawned with for one compiled-engine run.
fn compiled_args(
    target: &Path,
    tier: Tier,
    port: u16,
    client_port: u16,
    native: Option<NativeTarget>,
    traits: Option<&[String]>,
) -> Vec<String> {
    let mut args = vec![
        target.display().to_string(),
        "--headless".to_string(),
        "--json".to_string(),
        "--no-gaps".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--client-port".to_string(),
        client_port.to_string(),
    ];
    if let Some(native) = native {
        args.push("--target".to_string());
        args.push(native.as_str().to_string());
    }
    for name in traits.unwrap_or_default() {
        args.push("--trait".to_string());
        args.push(name.clone());
    }
    match tier {
        Tier::Smoke => args.push("--no-browser".to_string()),
        Tier::Standard => args.push("--fast".to_string()),
        Tier::Deep => {}
    }
    args
}

/// The argv for capturing every `poster`-declaring trait's WebP poster into `dir` (no walk).
fn poster_args(target: &Path, port: u16, client_port: u16, dir: &Path) -> Vec<String> {
    let mut args = compiled_args(target, Tier::Deep, port, client_port, None, None);
    args.push("--posters".to_string());
    args.push(dir.display().to_string());
    args
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_compiled_engine(
    target: &Path,
    tier: Tier,
    start_dir: &Path,
    user_store: &Path,
    port: u16,
    client_port: u16,
    engine_timeout: Option<Duration>,
    native: Option<NativeTarget>,
    traits: Option<&[String]>,
    posters: Option<&Path>,
) -> EngineReport {
    let label = compiled_label(native);
    let resolved = match resolve_node_engine(Engine::Compiled, start_dir, user_store) {
        Ok(r) => r,
        Err(EngineError::NotInstalled { package }) => {
            let mut report = EngineReport::not_installed(Engine::Compiled, &package);
            report.engine = label;
            return report;
        }
        Err(e) => return EngineReport::not_available_as(label, e.to_string()),
    };

    let args = match posters {
        Some(dir) => poster_args(target, port, client_port, dir),
        None => compiled_args(target, tier, port, client_port, native, traits),
    };

    let mut cmd = match build_command(resolved, &args) {
        Ok(c) => c,
        Err(e) => return EngineReport::not_available_as(label, e.to_string()),
    };
    apply_shared_env(&mut cmd);

    let budget = engine_timeout.unwrap_or_else(|| tier.timeout());
    let outcome = match run_with_timeout(cmd, budget) {
        Ok(o) => o,
        Err(e) => return EngineReport::not_available_as(label, format!("{e}")),
    };

    if outcome.timed_out {
        return EngineReport::not_available_as(
            label,
            format!("timed out after {budget:?} (override with --engine-timeout)"),
        );
    }

    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let doc: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            let stderr = String::from_utf8_lossy(&outcome.stderr);
            return EngineReport::not_available_as(
                label,
                format!(
                    "could not parse JSON report: {e} (stderr: {})",
                    first_line(&stderr)
                ),
            );
        }
    };

    fold_compiled_json(&doc, &label)
}

fn fold_compiled_json(doc: &serde_json::Value, label: &str) -> EngineReport {
    // A project that could not run (missing toolchain, device, library build) has no verdict:
    // the engine did not run, which is exit-2 class and never a pass.
    if let Some(entries) = doc["inconclusive"].as_array().filter(|a| !a.is_empty()) {
        let reasons: Vec<String> = entries
            .iter()
            .map(|e| {
                format!(
                    "{}: {}",
                    e["project"].as_str().unwrap_or("project"),
                    e["reason"].as_str().unwrap_or("no reason given")
                )
            })
            .collect();
        return EngineReport::not_available_as(
            label.to_string(),
            format!("inconclusive: {}", reasons.join("; ")),
        );
    }
    let checks = doc["totalChecks"].as_u64().unwrap_or(0) as usize;
    let passed = doc["totalPassed"].as_u64().unwrap_or(0) as usize;
    let failed = doc["totalFailed"].as_u64().unwrap_or(0) as usize;

    let mut coverage = None;
    let mut findings = Vec::new();
    if let Some(projects) = doc["projects"].as_array() {
        for project in projects {
            if coverage.is_none() {
                if let Some(cov) = project.get("coverage") {
                    if let (Some(covered), Some(total)) = (
                        cov.get("coveredItems").and_then(|v| v.as_u64()),
                        cov.get("totalItems").and_then(|v| v.as_u64()),
                    ) {
                        coverage = Some(CoverageInfo {
                            covered: covered as usize,
                            total: total as usize,
                            ratio: cov.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        });
                    }
                }
            }
            if let Some(phases) = project["phases"].as_array() {
                for phase in phases {
                    if let Some(checks_arr) = phase["checks"].as_array() {
                        for check in checks_arr {
                            if check
                                .get("passed")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                continue;
                            }
                            let label = check
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("check")
                                .to_string();
                            let detail = check
                                .get("detail")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            findings.push(CircuitFinding {
                                engine_term: translate_circuit_term(&label),
                                check: label,
                                detail,
                            });
                        }
                    }
                }
            }
        }
    }

    EngineReport {
        engine: label.to_string(),
        ran: true,
        status: "ran",
        passed,
        failed,
        checks,
        coverage,
        circuit_findings: findings,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_known_terms() {
        assert_eq!(
            translate_circuit_term("clickNoListener"),
            "unsubscribed emit (missing listens)"
        );
        assert_eq!(
            translate_circuit_term("replayDiverged"),
            "circuit divergence"
        );
        assert_eq!(translate_circuit_term("portalSweep"), "render-slot check");
        assert_eq!(translate_circuit_term("portalPerStep"), "render-slot check");
        assert_eq!(translate_circuit_term("guardParity"), "guard divergence");
        assert_eq!(translate_circuit_term("dataMutation"), "entity mutation");
        assert_eq!(
            translate_circuit_term("preconditionSkipped"),
            "transition skipped (precondition unreachable)"
        );
    }

    #[test]
    fn translate_unknown_term_passes_through() {
        assert_eq!(
            translate_circuit_term("state_transition"),
            "state_transition"
        );
        assert_eq!(
            translate_circuit_term("Coverage 87.5% (7/8)"),
            "Coverage 87.5% (7/8)"
        );
    }

    #[test]
    fn runtime_tier_flags_deep_requests_full_walk() {
        assert_eq!(runtime_tier_flags(Tier::Deep), ["--full-walk"]);
        assert_eq!(runtime_tier_flags(Tier::Standard), ["--fast"]);
        assert_eq!(runtime_tier_flags(Tier::Smoke), ["--first-frame", "--fast"]);
    }

    #[test]
    fn runtime_finding_translates_known_key() {
        let f =
            parse_runtime_finding("clickNoListener: ResetButton emits RESET with no subscriber");
        assert_eq!(f.check, "clickNoListener");
        assert_eq!(f.engine_term, "unsubscribed emit (missing listens)");
        assert_eq!(f.detail, "ResetButton emits RESET with no subscriber");
    }

    #[test]
    fn runtime_finding_passes_through_frame_error() {
        let f = parse_runtime_finding("frame 3: unexpected state after INIT");
        assert_eq!(f.check, "runtime");
        assert_eq!(f.engine_term, "frame 3: unexpected state after INIT");
    }

    #[test]
    fn fold_runtime_json_shape() {
        let doc = serde_json::json!({
            "timestamp": "2026-07-21T00:00:00Z",
            "url": "http://localhost",
            "mode": "playground",
            "summary": { "total": 5, "pass": 4, "error": 1, "warning": 0 },
            "results": [
                { "name": "x", "status": "error", "errors": ["clickNoListener: dead button"], "warnings": [], "screenshotPath": null, "durationMs": 10 }
            ],
            "coverage": { "covered": 8, "total": 10, "ratio": 0.8 }
        });
        let report = fold_runtime_json(&doc);
        assert!(report.ran);
        assert_eq!(report.checks, 5);
        assert_eq!(report.passed, 4);
        assert_eq!(report.failed, 1);
        let cov = report.coverage.expect("coverage present");
        assert_eq!((cov.covered, cov.total), (8, 10));
        assert_eq!(report.circuit_findings.len(), 1);
        assert_eq!(
            report.circuit_findings[0].engine_term,
            "unsubscribed emit (missing listens)"
        );
    }

    #[test]
    fn fold_compiled_json_shape() {
        let doc = serde_json::json!({
            "projects": [{
                "project": "ui-timer-display",
                "phases": [{
                    "phase": 4,
                    "name": "Browser",
                    "checks": [
                        { "label": "App loads", "passed": true },
                        { "label": "guardParity", "passed": false, "detail": "predicted pass, got fail" }
                    ],
                    "passed": 1,
                    "failed": 1
                }],
                "totalPassed": 1,
                "totalFailed": 1,
                "totalChecks": 2,
                "coverage": { "coveredItems": 3, "totalItems": 4, "ratio": 0.75 }
            }],
            "totalPassed": 1,
            "totalFailed": 1,
            "totalChecks": 2,
            "gapsDocumented": 0
        });
        let report = fold_compiled_json(&doc, "compiled");
        assert_eq!(report.engine, "compiled");
        assert!(report.ran);
        assert_eq!(report.checks, 2);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        let cov = report.coverage.expect("coverage present");
        assert_eq!((cov.covered, cov.total), (3, 4));
        assert_eq!(report.circuit_findings.len(), 1);
        assert_eq!(report.circuit_findings[0].check, "guardParity");
        assert_eq!(report.circuit_findings[0].engine_term, "guard divergence");
    }

    #[test]
    fn compiled_args_without_a_native_target_have_no_target_flag() {
        let args = compiled_args(Path::new("a.orb"), Tier::Standard, 3040, 3050, None, None);
        assert!(!args.iter().any(|a| a == "--target"));
        assert_eq!(args[0], "a.orb");
        assert!(args.contains(&"--fast".to_string()));
    }

    #[test]
    fn compiled_args_route_a_native_target_to_the_tool() {
        for (native, value) in [
            (NativeTarget::Ios, "ios"),
            (NativeTarget::Android, "android"),
        ] {
            let args = compiled_args(
                Path::new("a.orb"),
                Tier::Deep,
                3040,
                3050,
                Some(native),
                None,
            );
            let at = args
                .iter()
                .position(|a| a == "--target")
                .expect("--target present");
            assert_eq!(args[at + 1], value);
            assert!(
                !args.contains(&"--fast".to_string()),
                "deep has no tier flag"
            );
        }
    }

    #[test]
    fn compiled_args_forward_each_trait_to_the_tool() {
        let traits = vec!["NoteBrowseList".to_string(), "NoteTagBrowse".to_string()];
        let args = compiled_args(
            Path::new("a.orb"),
            Tier::Deep,
            3040,
            3050,
            Some(NativeTarget::Ios),
            Some(&traits),
        );
        let flags: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--trait")
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(flags, vec!["NoteBrowseList", "NoteTagBrowse"]);
    }

    #[test]
    fn poster_args_ask_the_tool_to_capture_into_the_dir() {
        let args = poster_args(Path::new("a.orb"), 3040, 3050, Path::new("/site/static"));
        let at = args
            .iter()
            .position(|a| a == "--posters")
            .expect("--posters present");
        assert_eq!(args[at + 1], "/site/static");
        assert_eq!(args[0], "a.orb");
        assert!(
            !args.contains(&"--fast".to_string()) && !args.contains(&"--no-browser".to_string()),
            "a poster capture needs the browser and no walk budget"
        );
    }

    #[test]
    fn control_a_verify_run_carries_no_posters_flag() {
        let args = compiled_args(Path::new("a.orb"), Tier::Deep, 3040, 3050, None, None);
        assert!(!args.iter().any(|a| a == "--posters"));
    }

    #[test]
    fn compiled_args_without_traits_carry_no_trait_flag() {
        let args = compiled_args(Path::new("a.orb"), Tier::Deep, 3040, 3050, None, None);
        assert!(!args.iter().any(|a| a == "--trait"));
    }

    #[test]
    fn smoke_still_skips_the_browser_phase_for_a_native_target() {
        let args = compiled_args(
            Path::new("a.orb"),
            Tier::Smoke,
            1,
            2,
            Some(NativeTarget::Ios),
            None,
        );
        assert!(args.contains(&"--no-browser".to_string()));
    }

    #[test]
    fn native_target_follows_the_shell_language() {
        use orbital_verify::shell::ShellLang;
        assert_eq!(
            NativeTarget::from_shell_lang(ShellLang::Swift),
            Some(NativeTarget::Ios)
        );
        assert_eq!(
            NativeTarget::from_shell_lang(ShellLang::Kotlin),
            Some(NativeTarget::Android)
        );
        for lang in [ShellLang::Python, ShellLang::Rust, ShellLang::TypeScript] {
            assert_eq!(NativeTarget::from_shell_lang(lang), None);
        }
    }

    #[test]
    fn engine_labels_name_the_platform() {
        assert_eq!(compiled_label(None), "compiled");
        assert_eq!(compiled_label(Some(NativeTarget::Ios)), "compiled:ios");
        assert_eq!(
            compiled_label(Some(NativeTarget::Android)),
            "compiled:android"
        );
    }

    #[test]
    fn fold_compiled_json_carries_the_native_label_and_counts() {
        let doc = serde_json::json!({
            "projects": [],
            "totalPassed": 5,
            "totalFailed": 1,
            "totalChecks": 6,
            "gapsDocumented": 0,
            "inconclusive": []
        });
        let report = fold_compiled_json(&doc, "compiled:android");
        assert_eq!(report.engine, "compiled:android");
        assert!(report.ran);
        assert_eq!((report.passed, report.failed, report.checks), (5, 1, 6));
    }

    #[test]
    fn an_inconclusive_native_run_is_not_a_pass_and_not_a_defect() {
        let doc = serde_json::json!({
            "projects": [],
            "totalPassed": 0,
            "totalFailed": 1,
            "totalChecks": 1,
            "gapsDocumented": 0,
            "inconclusive": [{ "project": "demo", "reason": "xcodebuild is not available" }]
        });
        let report = fold_compiled_json(&doc, "compiled:ios");
        assert_eq!(report.engine, "compiled:ios");
        assert!(!report.ran, "no verdict was produced");
        assert_eq!(report.status, "error");
        let message = report.error.clone().expect("reason carried");
        assert!(
            message.contains("inconclusive: demo: xcodebuild is not available"),
            "{message}"
        );
        let unified = UnifiedVerifyReport::new("demo".into(), Tier::Standard, vec![report]);
        assert_eq!(unified.verdict, "engine-error");
        assert_eq!(unified.exit_code(), 2);
    }

    #[test]
    fn an_empty_inconclusive_list_does_not_mask_a_normal_report() {
        let doc = serde_json::json!({ "projects": [], "totalPassed": 1, "totalFailed": 0, "totalChecks": 1, "inconclusive": [] });
        assert!(fold_compiled_json(&doc, "compiled:ios").ran);
    }

    #[test]
    fn unified_report_exit_codes() {
        let green = UnifiedVerifyReport::new(
            "x".into(),
            Tier::Standard,
            vec![EngineReport {
                engine: "rust".into(),
                ran: true,
                status: "ran",
                passed: 3,
                failed: 0,
                checks: 3,
                coverage: None,
                circuit_findings: vec![],
                error: None,
            }],
        );
        assert_eq!(green.exit_code(), 0);
        assert_eq!(green.verdict, "green");

        let failed = UnifiedVerifyReport::new(
            "x".into(),
            Tier::Standard,
            vec![EngineReport {
                engine: "rust".into(),
                ran: true,
                status: "ran",
                passed: 2,
                failed: 1,
                checks: 3,
                coverage: None,
                circuit_findings: vec![],
                error: None,
            }],
        );
        assert_eq!(failed.exit_code(), 1);
        assert_eq!(failed.verdict, "failed");

        let engine_error = UnifiedVerifyReport::new(
            "x".into(),
            Tier::Standard,
            vec![EngineReport::not_available(
                Engine::Runtime,
                "node missing".into(),
            )],
        );
        assert_eq!(engine_error.exit_code(), 2);
        assert_eq!(engine_error.verdict, "engine-error");
    }

    #[test]
    fn engine_error_takes_precedence_over_failed() {
        let mixed = UnifiedVerifyReport::new(
            "x".into(),
            Tier::Standard,
            vec![
                EngineReport {
                    engine: "rust".into(),
                    ran: true,
                    status: "ran",
                    passed: 2,
                    failed: 1,
                    checks: 3,
                    coverage: None,
                    circuit_findings: vec![],
                    error: None,
                },
                EngineReport::not_available(Engine::Runtime, "timed out".into()),
            ],
        );
        assert_eq!(mixed.exit_code(), 2);
    }

    // Process env is shared across parallel test threads; every test that
    // mutates ORB_RUNTIME_VERIFY_BIN must hold this lock for its full body.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spawned_node_options(caller: Option<&str>) -> String {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "NODE_OPTIONS";
        let prev = std::env::var(key).ok();
        match caller {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let mut cmd = Command::new("node");
        apply_shared_env(&mut cmd);
        let got = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        got
    }

    /// RV-CATALOG-LOAD-OOM: an engine spawned with no caller NODE_OPTIONS still
    /// gets the heap floor, because the corpus parse overruns node's default.
    #[test]
    fn apply_shared_env_sets_heap_floor_when_unset() {
        assert_eq!(spawned_node_options(None), NODE_HEAP_FLOOR);
    }

    /// A caller's unrelated flags are preserved, not clobbered.
    #[test]
    fn apply_shared_env_appends_heap_floor_to_caller_options() {
        let got = spawned_node_options(Some("--enable-source-maps"));
        assert!(
            got.contains("--enable-source-maps"),
            "caller flags dropped: {got}"
        );
        assert!(got.contains(NODE_HEAP_FLOOR), "heap floor missing: {got}");
    }

    /// An explicit caller heap wins outright — including a SMALLER one, so this
    /// can never silently override a deliberate constraint.
    #[test]
    fn apply_shared_env_defers_to_explicit_caller_heap() {
        assert_eq!(
            spawned_node_options(Some("--max-old-space-size=2048")),
            "--max-old-space-size=2048"
        );
    }

    #[test]
    fn resolve_node_engine_env_override_nonexistent_is_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "ORB_RUNTIME_VERIFY_BIN";
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "/nonexistent/path/to/tool.js");
        let result = resolve_node_engine(
            Engine::Runtime,
            Path::new("/nonexistent/start"),
            Path::new("/nonexistent/store"),
        );
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        assert!(matches!(result, Err(EngineError::Override(_))));
    }

    /// An env override pointing at a `.js` entry resolves to `Bun` — the entry
    /// is run through bun, never node from PATH.
    #[test]
    fn resolve_node_engine_env_override_js_resolves_to_bun() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "ORB_RUNTIME_VERIFY_BIN";
        let tmp = std::env::temp_dir().join("orb-engine-env-override.js");
        std::fs::write(&tmp, "// stub").unwrap();
        let prev = std::env::var(key).ok();
        std::env::set_var(key, &tmp);
        let result = resolve_node_engine(
            Engine::Runtime,
            Path::new("/nonexistent/start"),
            Path::new("/nonexistent/store"),
        );
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let _ = std::fs::remove_file(&tmp);
        match result {
            Ok(ResolvedCommand::Bun(p)) => assert_eq!(p, tmp),
            other => panic!("expected Bun(..), got {other:?}"),
        }
    }

    /// An installed npm package whose `package.json` declares the expected
    /// `bin` entry resolves to `Bun(<entry>)`.
    #[test]
    fn resolve_node_engine_finds_installed_package_bin() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "ORB_RUNTIME_VERIFY_BIN";
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);

        let tmp = std::env::temp_dir().join("orb-engine-resolve-installed");
        let _ = std::fs::remove_dir_all(&tmp);
        let (pkg, ver) = crate::baked_engine_pins::RUNTIME_VERIFY_ENGINE;
        let pkg_dir = tmp.join("node_modules").join(pkg);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::create_dir_all(pkg_dir.join("dist")).unwrap();
        std::fs::write(pkg_dir.join("dist/index.js"), "// stub").unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            format!(r#"{{"name":"{pkg}","version":"{ver}","bin":{{"runtime-verify":"./dist/index.js"}}}}"#),
        )
        .unwrap();
        let user_store = tmp.join("empty-store");

        let result = resolve_node_engine(Engine::Runtime, &tmp, &user_store);
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let outcome = match result {
            Ok(ResolvedCommand::Bun(p)) => Ok(p.ends_with("dist/index.js")),
            other => Err(format!("expected Bun(..), got {other:?}")),
        };
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(outcome.expect("resolved to Bun(..)"));
    }

    /// No env override, and the package is missing from every node_modules on
    /// the walk-up: `EngineError::NotInstalled`.
    #[test]
    fn resolve_node_engine_missing_package_is_not_installed() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "ORB_RUNTIME_VERIFY_BIN";
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        let result = resolve_node_engine(
            Engine::Runtime,
            Path::new("/nonexistent/start-dir"),
            Path::new("/nonexistent/user-store"),
        );
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let (pkg, _) = crate::baked_engine_pins::RUNTIME_VERIFY_ENGINE;
        match result {
            Err(EngineError::NotInstalled { package }) => assert_eq!(package, pkg),
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    /// The package is installed but its `package.json` declares no matching
    /// `bin` entry: `EngineError::Invalid`.
    #[test]
    fn resolve_node_engine_package_without_bin_entry_is_invalid() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "ORB_RUNTIME_VERIFY_BIN";
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);

        let tmp = std::env::temp_dir().join("orb-engine-resolve-no-bin");
        let _ = std::fs::remove_dir_all(&tmp);
        let (pkg, ver) = crate::baked_engine_pins::RUNTIME_VERIFY_ENGINE;
        let pkg_dir = tmp.join("node_modules").join(pkg);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            format!(r#"{{"name":"{pkg}","version":"{ver}"}}"#),
        )
        .unwrap();
        let user_store = tmp.join("empty-store");

        let result = resolve_node_engine(Engine::Runtime, &tmp, &user_store);
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let _ = std::fs::remove_dir_all(&tmp);
        match result {
            Err(EngineError::Invalid { package, .. }) => assert_eq!(package, pkg),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
