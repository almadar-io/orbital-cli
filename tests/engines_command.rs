//! `orb engines` integration tests — drives the real `orb`/`orbital` binary
//! (`CARGO_BIN_EXE_orb`) against a temp project with a stub `bun`
//! (`ORB_BUN_PATH`, no network) that materializes a fake engine npm package
//! (`bin` entry printing a canned JSON report) on `add` and deletes it on
//! `remove`, mirroring `behaviors_command.rs`'s pattern. The two engine
//! package names are the real, stable identifiers from
//! `baked_engine_pins.rs` (`@almadar-io/runtime-verify-tool` /
//! `@almadar-io/verify-tool`); the PINNED VERSION is read from the `bun add`
//! spec the stub receives, never hardcoded, so this stays correct across
//! pin bumps.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RUNTIME_PKG: &str = "@almadar-io/runtime-verify-tool";
const COMPILED_PKG: &str = "@almadar-io/verify-tool";

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

/// A temp project dir with a `package.json`, so `orb engines`/`project_dir`
/// resolves it as the project rather than falling back to `~/.orb`.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"engines-command-test\",\n  \"private\": true\n}\n",
        )
        .unwrap();
        orbital_test_support::link_installed_language(
            dir.path(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        );
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn pkg_dir(&self, pkg: &str) -> PathBuf {
        self.path().join("node_modules").join(pkg)
    }
}

/// `bun add -d <pkg>@<ver>` materializes `node_modules/<pkg>` with a
/// `package.json` (`bin` entry unless `ORB_ENGINE_STUB_NO_BIN` is set) and a
/// `dist/index.js` that prints a canned report matching whichever engine's
/// fold (`fold_runtime_json`/`fold_compiled_json`) expects; `remove` deletes
/// it; `bun x playwright install chromium` drops a marker file.
const BUN_ENGINE_STUB: &str = r#"#!/bin/sh
set -e
case "$1" in
  add)
    spec="$3"
    pkg="${spec%@*}"
    ver="${spec##*@}"
    case "$pkg" in
      *runtime-verify-tool*)
        bin_key="runtime-verify"
        body='{"summary":{"total":1,"pass":1,"error":0,"warning":0},"results":[],"coverage":{"covered":1,"total":1,"ratio":1}}'
        ;;
      *)
        bin_key="orbital-verify"
        body='{"totalChecks":1,"totalPassed":1,"totalFailed":0,"projects":[]}'
        ;;
    esac
    mkdir -p "node_modules/$pkg/dist"
    if [ -n "$ORB_ENGINE_STUB_NO_BIN" ]; then
      cat > "node_modules/$pkg/package.json" <<EOF
{
  "name": "$pkg",
  "version": "$ver"
}
EOF
    else
      cat > "node_modules/$pkg/package.json" <<EOF
{
  "name": "$pkg",
  "version": "$ver",
  "bin": { "$bin_key": "./dist/index.js" }
}
EOF
    fi
    cat > "node_modules/$pkg/dist/index.js" <<EOF
console.log('$body');
EOF
    ;;
  remove)
    pkg="$2"
    rm -rf "node_modules/$pkg"
    ;;
  x)
    if [ "$2" = "playwright" ]; then
      : > "playwright-install-marker"
    fi
    ;;
  install|update)
    ;;
esac
"#;

fn write_stub_script(project: &Project, filename: &str, script: &str) -> PathBuf {
    let path = project.path().join(filename);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(script.as_bytes()).unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn write_engine_bun_stub(project: &Project) -> PathBuf {
    write_stub_script(project, "fake-engine-bun.sh", BUN_ENGINE_STUB)
}

fn orb(project: &Project, bun_stub: &Path, args: &[&str]) -> Output {
    Command::new(orb_bin())
        .args(args)
        .current_dir(project.path())
        .env("ORB_BUN_PATH", bun_stub)
        .output()
        .expect("spawn orb")
}

fn orb_with_env(
    project: &Project,
    bun_stub: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(orb_bin());
    cmd.args(args)
        .current_dir(project.path())
        .env("ORB_BUN_PATH", bun_stub);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn orb")
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

#[test]
fn install_with_no_flags_installs_both_engines() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--no-browsers"],
    );
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        project.pkg_dir(RUNTIME_PKG).is_dir(),
        "runtime engine not installed"
    );
    assert!(
        project.pkg_dir(COMPILED_PKG).is_dir(),
        "compiled engine not installed"
    );
}

#[test]
fn install_runtime_only_skips_compiled() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(RUNTIME_PKG), "stdout: {stdout}");
    assert!(project.pkg_dir(RUNTIME_PKG).is_dir());
    assert!(!project.pkg_dir(COMPILED_PKG).exists());
}

#[test]
fn install_no_browsers_skips_the_playwright_step() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(output.status.success());
    assert!(
        !project.path().join("playwright-install-marker").exists(),
        "playwright install should have been skipped"
    );
}

#[test]
fn install_without_no_browsers_runs_the_playwright_step() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb(&project, &bun_stub, &["engines", "install", "--runtime"]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        project.path().join("playwright-install-marker").exists(),
        "playwright install marker missing"
    );
}

#[test]
fn install_post_check_failure_removes_the_package_and_reports_the_reason() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb_with_env(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
        &[("ORB_ENGINE_STUB_NO_BIN", "1")],
    );
    assert!(!output.status.success(), "install should have failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no `runtime-verify` bin entry"),
        "stderr: {stderr}"
    );
    assert!(
        !project.pkg_dir(RUNTIME_PKG).exists(),
        "failed install should have been removed by the stub `bun remove`"
    );
}

#[test]
fn install_global_uses_home_orb_store() {
    let tmp_home = tempfile::tempdir().expect("tmp home");
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let output = orb_with_env(
        &project,
        &bun_stub,
        &[
            "engines",
            "install",
            "--runtime",
            "--no-browsers",
            "--global",
        ],
        &[("HOME", tmp_home.path().to_str().unwrap())],
    );
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        tmp_home
            .path()
            .join(".orb/node_modules")
            .join(RUNTIME_PKG)
            .is_dir(),
        "engine should have installed under $HOME/.orb, not the project dir"
    );
    assert!(!project.pkg_dir(RUNTIME_PKG).exists());
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[test]
fn list_text_shows_installed_and_not_installed_rows() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let install = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(install.status.success());

    let output = orb(&project, &bun_stub, &["engines", "list"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(RUNTIME_PKG), "stdout: {stdout}");
    assert!(
        stdout.contains(COMPILED_PKG) && stdout.contains("not installed"),
        "stdout: {stdout}"
    );
}

#[test]
fn list_json_reports_installed_and_not_installed_flags() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let install = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(install.status.success());

    let output = orb(&project, &bun_stub, &["engines", "list", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows: serde_json::Value = serde_json::from_str(&stdout).expect("valid json array");
    let rows = rows.as_array().expect("array");

    let runtime_row = rows
        .iter()
        .find(|r| r["package"] == RUNTIME_PKG)
        .expect("runtime row present");
    assert_eq!(runtime_row["installed"], true);
    assert_eq!(runtime_row["entry"], "runtime-verify");

    let compiled_row = rows
        .iter()
        .find(|r| r["package"] == COMPILED_PKG)
        .expect("compiled row present");
    assert_eq!(compiled_row["installed"], false);
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[test]
fn update_reinstalls_at_the_pinned_version() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let install = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(install.status.success());

    let output = orb(&project, &bun_stub, &["engines", "update", "--runtime"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.pkg_dir(RUNTIME_PKG).is_dir());
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

#[test]
fn remove_deletes_the_installed_engine() {
    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    let install = orb(
        &project,
        &bun_stub,
        &["engines", "install", "--runtime", "--no-browsers"],
    );
    assert!(install.status.success());
    assert!(project.pkg_dir(RUNTIME_PKG).is_dir());

    let output = orb(&project, &bun_stub, &["engines", "remove", "--runtime"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!project.pkg_dir(RUNTIME_PKG).exists());
}

// ---------------------------------------------------------------------------
// `orb verify` <-> engine resolution end-to-end
// ---------------------------------------------------------------------------

fn fixture_orb_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/owner_scoped_identity.orb")
}

/// The not-installed path: an empty project + empty `$HOME` means
/// `resolve_node_engine` can't find the runtime engine anywhere on the
/// walk-up, so `orb verify --runtime` reports `ORB_ENGINE_NOT_INSTALLED`
/// and exits 2 (the "engine could not run at all" class, never conflated
/// with a defect).
#[test]
fn verify_runtime_engine_not_installed_reports_hint_and_exits_2() {
    let project = Project::new();
    let tmp_home = tempfile::tempdir().expect("tmp home");
    let fixture = fixture_orb_path();

    let output = Command::new(orb_bin())
        .args([
            "verify",
            "--runtime",
            "--smoke",
            "--json",
            fixture.to_str().unwrap(),
        ])
        .current_dir(project.path())
        .env("HOME", tmp_home.path())
        .output()
        .expect("spawn orb");

    assert_eq!(output.status.code(), Some(2), "{:?}", output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ORB_ENGINE_NOT_INSTALLED"),
        "stderr: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json report");
    let engines = report["engines"].as_array().expect("engines array");
    let runtime = engines
        .iter()
        .find(|e| e["engine"] == "runtime")
        .expect("runtime engine entry");
    assert_eq!(runtime["status"], "not_installed");
    assert_eq!(runtime["ran"], false);
}

/// End-to-end: a fixture engine installed for real (materialized directly
/// via the stub's `add` op, no `ORB_BUN_PATH` override at verify time) and
/// the REAL bun on PATH runs it — `orb verify --runtime --smoke` folds its
/// JSON report and exits 0.
#[test]
fn verify_runtime_runs_the_installed_engine_on_real_bun() {
    let has_bun = Command::new("bun")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_bun {
        eprintln!("skipping: no real `bun` on PATH in this environment");
        return;
    }

    let project = Project::new();
    let bun_stub = write_engine_bun_stub(&project);
    // Seed the fixture package directly through the stub's `add` op — this
    // spawns the STUB (a plain `sh` script) to materialize files, never a
    // network install.
    let status = Command::new("sh")
        .arg(&bun_stub)
        .args(["add", "-d", &format!("{RUNTIME_PKG}@1.0.0")])
        .current_dir(project.path())
        .status()
        .expect("seed via stub bun");
    assert!(status.success(), "stub bun add failed");
    assert!(project.pkg_dir(RUNTIME_PKG).is_dir());

    let tmp_home = tempfile::tempdir().expect("tmp home");
    let fixture = fixture_orb_path();

    let output = Command::new(orb_bin())
        .args([
            "verify",
            "--runtime",
            "--smoke",
            "--json",
            fixture.to_str().unwrap(),
        ])
        .current_dir(project.path())
        .env("HOME", tmp_home.path())
        .output()
        .expect("spawn orb");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json report");
    let engines = report["engines"].as_array().expect("engines array");
    let runtime = engines
        .iter()
        .find(|e| e["engine"] == "runtime")
        .expect("runtime engine entry");
    assert_eq!(runtime["status"], "ran");
    assert_eq!(runtime["ran"], true);
    assert_eq!(runtime["checks"], 1);
    assert_eq!(runtime["passed"], 1);
}
