//! `orb verify --server` — server↔client render parity over real HTTP.
//!
//! Boots `orbital-server` in-process, drives the client via `SyncServerBridge`, and runs the
//! same L2 render contract over the wire. Asserts the parity pass is actually executed (the
//! `[server]` line) and the run succeeds — so the emitted server's routes + the client render
//! agree with the in-process kernel path.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std-modal.orb")
}

fn owner_scoped_identity_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/owner_scoped_identity.orb")
}

#[test]
fn verify_server_runs_http_parity_pass() {
    let output = Command::new(orb_bin())
        .args(["verify", fixture().to_str().unwrap(), "--server"])
        .output()
        .expect("spawn orb verify --server");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "orb verify --server failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The parity pass must actually run (and pass) — not silently skip.
    assert!(
        stdout.contains("[server]"),
        "no server-parity pass in output (the [server] L2 line is missing):\n{stdout}"
    );
    assert!(
        !stdout.contains("L2/server FAIL"),
        "server-parity pass reported failures:\n{stdout}"
    );
    assert!(
        stdout.contains("passed"),
        "verify did not report a pass:\n{stdout}"
    );
}

/// `Almadar_Compiler_Gaps.md` §103 `C-L2-SERVER-BRIDGE-ANONYMOUS`: an app declaring an
/// `[identity]` roster plus an entity whose row is only visible to its owning viewer
/// (`ChannelMember`, filtered by `member == @user.id`) must parity-pass over HTTP exactly
/// like the in-process client — the HTTP bridge now binds the same first-roster persona
/// `default_probe_persona` gives the in-process probe, and seeds an owned row for it.
/// Before the fix the HTTP bridge bound no persona at all, `@user.id` was unset, the
/// owner-scoped fetch matched nothing, and `entity_data_bound` failed with the literal
/// `entity-list[entity=<empty-array>]` offender (reproduced by temporarily reverting
/// `boot_http_bridge` to its pre-fix `seed_sample`-only body during this fix's development).
#[test]
fn verify_server_binds_default_persona_for_owner_scoped_identity() {
    let output = Command::new(orb_bin())
        .args(["verify", owner_scoped_identity_fixture().to_str().unwrap(), "--server"])
        .output()
        .expect("spawn orb verify --server");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "orb verify --server failed on an owner-scoped [identity] fixture.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("<empty-array>"),
        "server-side probe evaluated the owner-scoped fetch anonymously (no default persona bound):\n{stdout}"
    );
    assert!(
        !stdout.contains("L2/server FAIL"),
        "server-parity pass reported failures:\n{stdout}"
    );
}
