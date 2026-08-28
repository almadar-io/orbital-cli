//! Agent spawning: locating the Bun runtime and agent bundle, and forwarding
//! CLI invocations to the bundled agent.

use crate::Cli;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

pub(crate) fn find_agent_entry() -> anyhow::Result<PathBuf> {
    // 1. ORB_AGENT_DIR env var
    if let Ok(dir) = std::env::var("ORB_AGENT_DIR") {
        let entry = PathBuf::from(&dir).join("cli.js");
        if entry.exists() {
            return Ok(entry);
        }
    }

    // 2. agent/ relative to current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let entry = dir.join("agent").join("cli.js");
            if entry.exists() {
                return Ok(entry);
            }
        }
    }

    anyhow::bail!(
        "Agent bundle not found. Set ORB_AGENT_DIR or place agent/cli.js next to the orb binary."
    )
}

pub(crate) fn spawn_agent(cli: &Cli) -> anyhow::Result<()> {
    // A truly bare `orb` (no prompt words) is an attended, interactive
    // invocation — not the unattended/scripted runaway pattern that OOM'd
    // the machine (user decision 2026-08-27). Any prompt words still require
    // ORB_AGENT_FALLBACK=1 as before.
    if !cli.prompt.is_empty() {
        ensure_agent_delegation_allowed("interactive agent")?;
    }
    let bun = find_bun()?;
    let entry = find_agent_entry()?;

    let mut cmd = Command::new(&bun);
    cmd.arg(&entry);

    // Forward prompt words
    if !cli.prompt.is_empty() {
        let prompt = cli.prompt.join(" ");
        cmd.arg(&prompt);
    }

    // Forward flags
    if let Some(ref provider) = cli.provider {
        cmd.arg("--provider").arg(provider);
    }
    if let Some(ref autonomy) = cli.autonomy {
        cmd.arg("--autonomy").arg(autonomy);
    }
    if let Some(ref resume) = cli.resume {
        cmd.arg("--resume");
        if let Some(ref id) = resume {
            cmd.arg(id);
        }
    }
    if cli.last {
        cmd.arg("--last");
    }
    if let Some(budget) = cli.budget {
        cmd.arg("--budget").arg(budget.to_string());
    }

    // Set ORB_BINARY_PATH so agent tools call back to us
    if let Ok(own_exe) = std::env::current_exe() {
        cmd.env("ORB_BINARY_PATH", &own_exe);
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to spawn agent: {}", e))?;

    std::process::exit(status.code().unwrap_or(1));
}

/// ALL agent delegation is opt-in (user decision 2026-07-17): runaway callers
/// accumulated hundreds of hung bun agent processes overnight and OOM-crashed
/// the machine twice. Nothing spawns the agent without ORB_AGENT_FALLBACK=1.
fn ensure_agent_delegation_allowed(what: &str) -> anyhow::Result<()> {
    if std::env::var("ORB_AGENT_FALLBACK").is_ok_and(|v| v == "1") {
        return Ok(());
    }
    anyhow::bail!(
        "agent delegation is disabled (attempted: {what}). Set ORB_AGENT_FALLBACK=1 to opt in."
    )
}

pub(crate) fn spawn_agent_with_args(args: &[&str]) -> anyhow::Result<()> {
    ensure_agent_delegation_allowed(&args.join(" "))?;
    let bun = find_bun()?;
    let entry = find_agent_entry()?;

    let mut cmd = Command::new(&bun);
    cmd.arg(&entry);
    for arg in args {
        cmd.arg(arg);
    }

    if let Ok(own_exe) = std::env::current_exe() {
        cmd.env("ORB_BINARY_PATH", &own_exe);
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to spawn agent: {}", e))?;

    std::process::exit(status.code().unwrap_or(1));
}
