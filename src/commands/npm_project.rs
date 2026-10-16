//! Shared project/user-store resolution for `orb behaviors`, `orb engines`,
//! and `orb verify` — all three resolve npm packages node-style from the
//! working directory, with the user store (`~/.orb`) as the final rung.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use orbital_core::behavior_packages::{discover, user_store_dir, BehaviorPackage};

use crate::utils::find_bun;

/// The project directory a behavior-package/engine command operates on: the
/// nearest ancestor of the working directory with a `package.json`, or
/// (`--global`) the user store (`~/.orb`), bootstrapped with a private
/// `package.json` if it does not exist yet.
pub(crate) fn project_dir(global: bool) -> anyhow::Result<PathBuf> {
    if global {
        let dir = user_store_dir().ok_or_else(|| {
            anyhow::anyhow!("could not determine a home directory for the user store")
        })?;
        std::fs::create_dir_all(&dir)?;
        let manifest = dir.join("package.json");
        if !manifest.exists() {
            std::fs::write(
                &manifest,
                "{\n  \"name\": \"orb-user-store\",\n  \"private\": true\n}\n",
            )?;
        }
        return Ok(dir);
    }

    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|dir| dir.join("package.json").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no package.json above the working directory; run from your project or use --global"
            )
        })
}

/// `discover()` from `dir`, using the user store as its fallback the same way
/// `commands/stamp.rs` does.
pub(crate) fn discover_from(dir: &Path) -> anyhow::Result<Vec<BehaviorPackage>> {
    let user_store = user_store_dir().unwrap_or_else(|| dir.to_path_buf());
    discover(dir, &user_store)
        .map_err(|e| anyhow::anyhow!("failed to discover behavior packages: {e}"))
}

/// Set when stdout carries a protocol (`orb mcp`'s JSON-RPC): reports then go to stderr.
static STDOUT_IS_PROTOCOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn mark_stdout_as_protocol() {
    STDOUT_IS_PROTOCOL.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn report_on_stderr(stdout_is_protocol: bool) -> bool {
    stdout_is_protocol
}

/// Whether a command's report lines must stay off stdout.
pub(crate) fn reports_on_stderr() -> bool {
    report_on_stderr(STDOUT_IS_PROTOCOL.load(std::sync::atomic::Ordering::SeqCst))
}

pub(crate) fn run_bun(project_dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let bun = find_bun()?;
    let status = Command::new(&bun)
        .args(args)
        .current_dir(project_dir)
        .stdout(if reports_on_stderr() { Stdio::from(std::io::stderr()) } else { Stdio::inherit() })
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("bun {} failed", args.join(" "));
    }
    Ok(())
}

/// The `(start_dir, user_store)` pair every node-style walk-up resolves
/// against: the working directory, and the user store (falling back to the
/// working directory itself when no home directory can be found).
pub(crate) fn cwd_and_user_store() -> anyhow::Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let user_store = user_store_dir().unwrap_or_else(|| cwd.clone());
    Ok((cwd, user_store))
}

#[cfg(test)]
mod protocol_stdout_tests {
    /// Under `orb mcp` stdout carries JSON-RPC: a first-run `orb update` must report on stderr.
    #[test]
    fn reports_go_to_stderr_while_stdout_is_a_protocol_channel() {
        assert!(super::report_on_stderr(true));
        assert!(!super::report_on_stderr(false), "control: a plain command reports on stdout");
    }
}
