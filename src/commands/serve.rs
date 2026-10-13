//! `orb serve` — compile + build + serve a full-stack app with bundled Bun.

use crate::commands::compile::compile_schema;
use crate::utils::find_bun;
use crate::{HostArg, ServerFrameworkArg, ShellMode, ShellType};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Compile and serve a full-stack app from a .orb file using bundled Bun.
///
/// Steps:
/// 1. Compile .orb -> Hono TypeScript app (into .orbital/ directory)
/// 2. bun install
/// 3. Build shared package (tsup)
/// 4. Start the dev Firebase emulators (unless `--no-emulators`): dev personas are the app's
///    `[identity]` rows signed in as Auth-emulator users, so `@user` is a verified identity
/// 5. Build client (vite via bun), pointed at the Auth emulator
/// 6. Serve with Bun (Hono API + static client on single port)
pub(crate) fn run_serve(
    path: &PathBuf,
    port: u16,
    open: bool,
    no_build: bool,
    patterns_path: Option<&std::path::Path>,
    emulators: bool,
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
            HostArg::Generated,
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
    }

    let emulator_env = if emulators {
        start_dev_emulators(&bun, &output)?
    } else {
        println!(
            "Serving without the Firebase emulators: no dev personas, every request is anonymous."
        );
        Vec::new()
    };

    if !no_build {
        // Step 5: Build client
        let client_dir = output.join("packages/client");
        println!("Building client...");
        let client_status = Command::new(&bun)
            .args(["run", "vite", "build"])
            .current_dir(&client_dir)
            .envs(client_emulator_env(&emulator_env))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        if !client_status.success() {
            anyhow::bail!("client build failed");
        }
    }

    // Step 6: Serve
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
        .envs(emulator_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !serve_status.success() {
        anyhow::bail!("Server exited with error");
    }

    Ok(())
}

/// Run the compiled app's `packages/server/scripts/emulators.mjs`: starts (or reuses) the shared dev
/// emulators and answers their env (`FIREBASE_PROJECT_ID`, `*_EMULATOR_HOST`, …) as one JSON line.
fn start_dev_emulators(
    bun: &std::path::Path,
    output: &std::path::Path,
) -> anyhow::Result<Vec<(String, String)>> {
    println!("Starting the Firebase emulators (Firestore + Auth)...");
    let script = output.join("packages/server/scripts/emulators.mjs");
    if !script.exists() {
        anyhow::bail!(
            "{} not found. The Hono shell template may be outdated.",
            script.display()
        );
    }
    let out = Command::new(bun)
        .arg(&script)
        .current_dir(output.join("packages/server"))
        .stderr(Stdio::inherit())
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "the Firebase emulators did not start (they need firebase-tools and Java 21; see the output above). \
             Pass --no-emulators to serve without dev personas."
        );
    }
    let line = String::from_utf8(out.stdout)?;
    let env: std::collections::BTreeMap<String, String> = serde_json::from_str(line.trim())?;
    Ok(env.into_iter().collect())
}

/// The client build's view of the emulators: sign-in goes to the Auth emulator under its project.
fn client_emulator_env(emulator_env: &[(String, String)]) -> Vec<(String, String)> {
    let get = |key: &str| {
        emulator_env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let (Some(project), Some(auth_host)) = (
        get("FIREBASE_PROJECT_ID"),
        get("FIREBASE_AUTH_EMULATOR_HOST"),
    ) else {
        return Vec::new();
    };
    vec![
        (
            "VITE_APP_FIREBASE_API_KEY".to_string(),
            "emulator".to_string(),
        ),
        ("VITE_APP_FIREBASE_PROJECT_ID".to_string(), project),
        ("VITE_FIREBASE_AUTH_EMULATOR_HOST".to_string(), auth_host),
    ]
}

#[cfg(test)]
mod tests {
    use super::client_emulator_env;

    #[test]
    fn the_client_signs_in_on_the_auth_emulator_under_its_project() {
        let env = vec![
            (
                "FIREBASE_PROJECT_ID".to_string(),
                "demo-almadar".to_string(),
            ),
            (
                "FIREBASE_AUTH_EMULATOR_HOST".to_string(),
                "127.0.0.1:9099".to_string(),
            ),
            (
                "FIRESTORE_EMULATOR_HOST".to_string(),
                "127.0.0.1:8080".to_string(),
            ),
        ];
        assert_eq!(
            client_emulator_env(&env),
            vec![
                (
                    "VITE_APP_FIREBASE_API_KEY".to_string(),
                    "emulator".to_string()
                ),
                (
                    "VITE_APP_FIREBASE_PROJECT_ID".to_string(),
                    "demo-almadar".to_string()
                ),
                (
                    "VITE_FIREBASE_AUTH_EMULATOR_HOST".to_string(),
                    "127.0.0.1:9099".to_string()
                ),
            ]
        );
    }

    #[test]
    fn control_no_auth_emulator_means_no_client_sign_in_config() {
        assert!(client_emulator_env(&[(
            "FIREBASE_PROJECT_ID".to_string(),
            "demo-almadar".to_string()
        )])
        .is_empty());
        assert!(client_emulator_env(&[]).is_empty());
    }
}
