//! orb runs its shells as programs found beside it, so a test that compiles
//! or emits first builds the current shell programs (and `orb-desktop`) into
//! orb's directory.

use std::path::PathBuf;
use std::sync::Once;

const SHELLS: &[&str] = &["typescript", "python", "rust", "android", "ios", "chrome"];

static BUILD: Once = Once::new();

fn build_shells() {
    BUILD.call_once(|| {
        let profile_dir = PathBuf::from(env!("CARGO_BIN_EXE_orb"));
        let profile = profile_dir.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).expect("orb's profile dir").to_string();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut command = std::process::Command::new(cargo);
        command.args(["build", "--profile", if profile == "debug" { "dev" } else { &profile }]);
        for shell in SHELLS {
            command.args(["-p", &format!("orbital-shell-{shell}")]);
        }
        // orb-desktop (the Rust shell's `desktop` feature) runs `orb verify`'s Layer 2.
        command.args(["--features", "orbital-shell-rust/desktop"]);
        let status = command.current_dir(env!("CARGO_MANIFEST_DIR")).status().expect("spawn cargo build for the shell programs");
        assert!(status.success(), "building the shell programs failed");
    });
}

/// `orb`, with every shell program it runs built beside it.
#[allow(dead_code)]
pub fn orb() -> PathBuf {
    build_shells();
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

/// `orbital` (the same binary under its other name), shells built beside it.
#[allow(dead_code)]
pub fn orbital() -> PathBuf {
    build_shells();
    PathBuf::from(env!("CARGO_BIN_EXE_orbital"))
}
