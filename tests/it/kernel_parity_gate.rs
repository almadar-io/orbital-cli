//! P-044: the `.lolo` kernel (std-kernel-dispatch) answers as the native kernel. `orb goldens --parity` plays each
//! case of a std replay-goldens file on both kernels through std's KernelParity. These files are the pinned subset a
//! kernel change is checked against here; the whole std corpus runs in std's T7 release gate (`scripts/gates/t7.sh`).

use super::language_data_preflight::{orb, project};
use std::path::PathBuf;

const PINNED: &[&str] = &[
    "agent/abilities.replay.goldens.json",
    "flow/job-runner-scheduler.replay.goldens.json",
    "net/http-keyed.replay.goldens.json",
    "ui/rasm/stack.replay.goldens.json",
];

fn std_spec(rel: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packages/almadar-std/behaviors/lolo/spec").join(rel);
    path.canonicalize().unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn played(rel: &str, parity: bool) -> (bool, String) {
    let dir = project(false);
    let file = std_spec(rel);
    let file = file.to_str().expect("a utf-8 path");
    let out = if parity { orb(dir.path(), &["goldens", "--parity", file]) } else { orb(dir.path(), &["goldens", file]) };
    let report = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), report)
}

#[test]
fn the_lolo_kernel_answers_as_the_native_kernel_on_the_pinned_std_goldens() {
    assert!(!PINNED.is_empty(), "no pinned goldens file");
    for rel in PINNED {
        let (ok, report) = played(rel, true);
        assert!(report.lines().any(|l| l.starts_with("pass  ")), "{rel}: no case was played on both kernels\n{report}");
        assert!(ok && !report.lines().any(|l| l.starts_with("fail  ")), "{rel}: the kernels differ\n{report}");
    }
}

#[test]
fn the_kernel_atoms_own_parity_fixtures_and_controls_hold() {
    let (ok, report) = played("kernel/kernel-dispatch.replay.goldens.json", false);
    assert!(ok && !report.lines().any(|l| l.starts_with("fail  ")), "std-kernel-dispatch's goldens fail\n{report}");
    assert!(report.lines().any(|l| l.starts_with("pass  control-")), "the controls were not played\n{report}");
}
