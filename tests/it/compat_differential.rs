//! Phase 4.0 zero-delta differential harness for the type-compat re-baseline
//! (docs/Almadar_Compiler_Spec.md, Phase 4 protocol). Walks the real behavior
//! corpus (almadar-std + almadar-behaviors registries), runs each `.orb`
//! through the SAME pipeline `orb validate --json` runs (by invoking the real
//! `orb` binary), and snapshots diagnostics as sorted `(file, code, path,
//! message)` tuples in two sections: `warnings` and `errors`. The corpus is
//! NOT error-free today; the spec's 0-error expectation is recorded as the
//! measured error set and any movement in it fails the differential exactly
//! like a warning delta.
//!
//! Modes:
//!   default                 compare against the snapshot (byte-identical);
//!                           on mismatch write `warnings.actual.json` and fail
//!   COMPAT_BASELINE_WRITE=1 (re-)write the snapshot
//!   COMPAT_PROJECT_DIR=<dir> run `orb validate` with `<dir>` as its cwd instead
//!                           of the repo root, to prove behavior-package node_modules
//!                           resolution byte-matches the workspace-linked corpus
//!
//! Ignored by default; run explicitly:
//!   cargo test -p orbital-cli --test compat_differential -- --ignored --release

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo-root>/orbital-rust/crates/orbital-cli
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// cwd for the spawned `orb validate` — the repo root by default, or
/// `COMPAT_PROJECT_DIR` when set (see module doc comment).
fn project_dir(repo: &Path) -> PathBuf {
    std::env::var_os("COMPAT_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.to_path_buf())
}

const REGISTRY_ROOTS: &[&str] = &[
    "packages/almadar-std/behaviors/registry",
    "packages/almadar-behaviors/behaviors/registry",
];

const BASELINE_DIR: &str = "orbital-rust/target/compat-baseline";
const BASELINE_NAME: &str = "warnings.json";
const ACTUAL_NAME: &str = "warnings.actual.json";

#[derive(Deserialize)]
struct ValidationJson {
    valid: bool,
    errors: Option<Vec<DiagnosticRow>>,
    warnings: Option<Vec<DiagnosticRow>>,
}

#[derive(Deserialize)]
struct DiagnosticRow {
    code: String,
    path: String,
    message: String,
}

struct FileResult {
    rel_path: String,
    errors: Vec<(String, String, String)>,
    warnings: Vec<(String, String, String)>,
}

fn walk_orb_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_orb_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("orb") {
            out.push(path);
        }
    }
}

/// Run `orb validate --json` on one file — the exact pipeline `orb validate` uses.
fn validate_one(repo: &Path, file: &Path) -> FileResult {
    let rel = file
        .strip_prefix(repo)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    // the spawn cwd may not be `repo` (COMPAT_PROJECT_DIR), so the path must be absolute
    let abs_file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        repo.join(file)
    };
    let output = Command::new(orb_bin())
        .args(["validate", "--json"])
        .arg(&abs_file)
        .current_dir(project_dir(repo))
        .output()
        .expect("failed to spawn orb binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Result<ValidationJson, _> = serde_json::from_str(&stdout);
    match parsed {
        Err(e) => FileResult {
            rel_path: rel,
            errors: vec![(
                "HARNESS_PARSE_ERROR".into(),
                String::new(),
                format!(
                    "orb validate --json did not emit JSON (status {}): {e}",
                    output.status
                ),
            )],
            warnings: Vec::new(),
        },
        Ok(json) => {
            let row = |r: &DiagnosticRow| (r.code.clone(), r.path.clone(), r.message.clone());
            let mut errors: Vec<_> = json
                .errors
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(row)
                .collect();
            if !output.status.success() && !json.valid && errors.is_empty() {
                errors.push((
                    "HARNESS_NONZERO_EXIT".into(),
                    String::new(),
                    stdout.into_owned(),
                ));
            }
            FileResult {
                rel_path: rel,
                errors,
                warnings: json
                    .warnings
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(row)
                    .collect(),
            }
        }
    }
}

#[test]
#[ignore = "tool: Phase 4.0 differential harness over the compat corpus; run explicitly with --ignored --release"]
fn compat_differential_corpus() {
    let repo = repo_root();
    let mut files = Vec::new();
    for rel in REGISTRY_ROOTS {
        let root = repo.join(rel);
        assert!(root.is_dir(), "corpus root missing: {}", root.display());
        walk_orb_files(&root, &mut files);
    }
    files.sort();
    files.dedup();
    assert!(
        files.len() > 1000,
        "corpus sanity: expected >1000 .orb files, found {}",
        files.len()
    );

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<FileResult>>> = files.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(files.len());
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                let r = validate_one(&repo, &files[i]);
                *results[i].lock().unwrap() = Some(r);
            });
        }
    });
    let elapsed = started.elapsed();

    let mut all_tuples: Vec<(String, String, String, String)> = Vec::new();
    let mut total_warnings = 0usize;
    let mut file_errors: Vec<(String, String, String, String)> = Vec::new();
    for r in &results {
        let r = r.lock().unwrap().take().unwrap();
        total_warnings += r.warnings.len();
        for (code, path, message) in r.errors {
            file_errors.push((r.rel_path.clone(), code, path, message));
        }
        for (code, path, message) in r.warnings {
            all_tuples.push((r.rel_path.clone(), code, path, message));
        }
    }
    all_tuples.sort();
    all_tuples.dedup();
    file_errors.sort();
    file_errors.dedup();

    #[derive(Serialize)]
    struct Snapshot {
        files: usize,
        warnings_total: usize,
        warnings: Vec<(String, String, String, String)>,
        errors: Vec<(String, String, String, String)>,
    }

    let unique_tuples = all_tuples.len();
    println!(
        "compat-differential summary: files={} errors={} warnings(total)={} warnings(unique tuples)={} elapsed={:?}",
        files.len(),
        file_errors.len(),
        total_warnings,
        unique_tuples,
        elapsed
    );

    let baseline_dir = repo.join(BASELINE_DIR);
    std::fs::create_dir_all(&baseline_dir).unwrap();
    let baseline_path = baseline_dir.join(BASELINE_NAME);
    let snapshot = Snapshot {
        files: files.len(),
        warnings_total: total_warnings,
        warnings: all_tuples,
        errors: file_errors,
    };
    let actual = format!("{}\n", serde_json::to_string_pretty(&snapshot).unwrap());

    if std::env::var("COMPAT_BASELINE_WRITE").ok().as_deref() == Some("1") {
        std::fs::write(&baseline_path, &actual).unwrap();
        println!(
            "compat-differential: wrote {} tuples to {}",
            unique_tuples,
            baseline_path.display()
        );
        return;
    }

    let expected = std::fs::read_to_string(&baseline_path).unwrap_or_else(|e| {
        panic!(
            "cannot read baseline {} (run with COMPAT_BASELINE_WRITE=1 to create it): {e}",
            baseline_path.display()
        )
    });
    if expected != actual {
        let actual_path = baseline_dir.join(ACTUAL_NAME);
        std::fs::write(&actual_path, &actual).unwrap();
        let expected_lines: Vec<&str> = expected.lines().collect();
        let actual_lines: Vec<&str> = actual.lines().collect();
        let max = expected_lines.len().max(actual_lines.len());
        let mut diff = String::new();
        let mut diffs = 0usize;
        for i in 0..max {
            let e = expected_lines.get(i).copied().unwrap_or("<missing>");
            let a = actual_lines.get(i).copied().unwrap_or("<missing>");
            if e != a {
                diffs += 1;
                if diffs <= 20 {
                    diff.push_str(&format!(
                        "line {}:\n  expected: {e}\n  actual:   {a}\n",
                        i + 1
                    ));
                }
            }
        }
        panic!(
            "compat-differential baseline mismatch: {} delta line(s). actual written to {}. first diffs:\n{}",
            diffs,
            actual_path.display(),
            diff
        );
    }
    println!(
        "compat-differential: zero delta against {}",
        baseline_path.display()
    );
}
