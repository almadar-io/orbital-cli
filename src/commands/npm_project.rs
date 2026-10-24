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

pub(crate) fn run_bun(project_dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let bun = find_bun()?;
    let status = Command::new(&bun)
        .args(args)
        .current_dir(project_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("bun {} failed", args.join(" "));
    }
    Ok(())
}

