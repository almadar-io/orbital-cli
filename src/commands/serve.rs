//! `orb serve` — compile + build + serve a full-stack app with bundled Bun.

use crate::commands::compile::compile_schema;
use crate::utils::find_bun;
use crate::{ServerFrameworkArg, ShellMode, ShellType};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Compile and serve a full-stack app from a .orb file using bundled Bun.
///
/// Steps:
/// 1. Compile .orb -> Hono TypeScript app (into .orbital/ directory)
/// 2. bun install
/// 3. Build shared package (tsup)
/// 4. Build client (vite via bun)
/// 5. Serve with Bun (Hono API + static client on single port)
pub(crate) fn run_serve(
    path: &PathBuf,
    port: u16,
    open: bool,
    no_build: bool,
    patterns_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let bun = find_bun()?;

    // Output directory: .orbital/ next to the schema file
    let schema_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let output = schema_dir.join(".orbital");

    // Step 1: Compile with Hono backend
    if !no_build || !output.exists() {
        println!("Compiling {}...", path.display());
        compile_schema(
            path,
            &output,
            ShellType::Typescript,
            ShellMode::Full,
            None, // frontend
            None, // backend
            None, // backend_url
            patterns_path,
            false, // verbose
            false, // dev_logs
            ServerFrameworkArg::Hono,
            None, // emit_orb
        )?;
    }

    // Step 2: Install dependencies
    println!("Installing dependencies...");
    let install_status = Command::new(&bun)
        .args(["install"])
        .current_dir(&output)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !install_status.success() {
        anyhow::bail!("bun install failed");
    }

    if !no_build {
        // Step 3: Build shared package
        let shared_dir = output.join("packages/shared");
        if shared_dir.join("tsup.config.ts").exists() {
            println!("Building shared package...");
            let shared_status = Command::new(&bun)
                .args(["run", "build"])
                .current_dir(&shared_dir)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            if !shared_status.success() {
                anyhow::bail!("shared build failed");
            }
        }

        // Step 4: Build client
        let client_dir = output.join("packages/client");
        println!("Building client...");
        let client_status = Command::new(&bun)
            .args(["run", "vite", "build"])
            .current_dir(&client_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        if !client_status.success() {
            anyhow::bail!("client build failed");
        }
    }

    // Step 5: Serve
    let server_entry = output.join("packages/server/src/serve.ts");
    if !server_entry.exists() {
        anyhow::bail!(
            "serve.ts not found at {}. The Hono shell template may be outdated.",
            server_entry.display()
        );
    }

    println!("\nStarting server on http://localhost:{}...\n", port);

    if open {
        // Open browser in background
        let url = format!("http://localhost:{}", port);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = Command::new("xdg-open")
                .arg(&url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }

    let serve_status = Command::new(&bun)
        .args(["run", "packages/server/src/serve.ts"])
        .current_dir(&output)
        .env("PORT", port.to_string())
        .env("NODE_ENV", "development")
        .env("USE_MOCK_DATA", "true")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !serve_status.success() {
        anyhow::bail!("Server exited with error");
    }

    Ok(())
}
