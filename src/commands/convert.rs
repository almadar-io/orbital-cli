//! `orb convert` — spawn npx almadar-converter and stream its JSON output.

use std::path::PathBuf;

pub(crate) fn run_convert(
    path: Option<PathBuf>,
    output: &str,
    crawl: Option<String>,
    crawl_facts: Option<PathBuf>,
    ai: bool,
    verify: bool,
    facts_only: bool,
    headed: bool,
    depth: u32,
    verbose: bool,
) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut args = vec!["almadar-converter".to_string()];

    // Source path (positional arg)
    if let Some(p) = &path {
        args.push(p.display().to_string());
    }

    // Output
    args.push("-o".to_string());
    args.push(output.to_string());

    // Flags
    if let Some(url) = &crawl {
        args.push("--crawl".to_string());
        args.push(url.clone());
    }
    if let Some(cf) = &crawl_facts {
        args.push("--crawl-facts".to_string());
        args.push(cf.display().to_string());
    }
    if ai {
        args.push("--ai".to_string());
    }
    if verify {
        args.push("--verify".to_string());
    }
    if facts_only {
        args.push("--facts-only".to_string());
    }
    if headed {
        args.push("--headed".to_string());
    }
    args.push("--depth".to_string());
    args.push(depth.to_string());
    args.push("--json".to_string());
    if verbose {
        args.push("--verbose".to_string());
    }

    if verbose {
        eprintln!("Running: npx {}", args.join(" "));
    }

    // Spawn npx almadar-converter --json and stream output
    let mut child = Command::new("npx")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn npx: {}. Is Node.js installed?", e))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
        let line = line?;
        // Parse JSON progress events and render them as CLI output
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(gate) = event.get("gate").and_then(|g| g.as_str()) {
                let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let message = event.get("message").and_then(|m| m.as_str()).unwrap_or("");
                match status {
                    "start" => eprint!("[{}] {}...", gate, message),
                    "complete" => eprintln!(" {}", message),
                    "progress" => eprintln!("\n[{}] {}", gate, message),
                    "error" => eprintln!(" ERROR: {}", message),
                    _ => {}
                }
            } else if let Some(success) = event.get("success").and_then(|s| s.as_bool()) {
                // Final result line
                if success {
                    if let Some(out) = event.get("output").and_then(|o| o.as_str()) {
                        eprintln!("\nOutput: {}", out);
                    }
                } else if let Some(err) = event.get("error").and_then(|e| e.as_str()) {
                    anyhow::bail!("Conversion failed: {}", err);
                }
            }
        } else if verbose {
            eprintln!("{}", line);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!(
            "almadar-converter exited with code {}",
            status.code().unwrap_or(-1)
        );
    }

    Ok(())
}
