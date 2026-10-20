//! Locating the Bun runtime that `orb add|update|remove|list` and `orb engines` install with.

use std::path::PathBuf;
use std::process::Command;

pub(crate) fn find_bun() -> anyhow::Result<PathBuf> {
    // 1. ORB_BUN_PATH env var
    if let Ok(p) = std::env::var("ORB_BUN_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
    }

    // 2. Sibling to current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bun_name = if cfg!(windows) { "bun.exe" } else { "bun" };
            let sibling = dir.join(bun_name);
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }

    // 3. PATH lookup
    if let Ok(output) = Command::new("which").arg("bun").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!("Bun runtime not found. Install Bun (https://bun.sh) or set ORB_BUN_PATH.")
}
