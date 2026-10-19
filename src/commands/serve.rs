//! `orb serve` — a program served by the native server with the TypeScript shell's prebuilt web client: no build,
//! no Node process for the app.
//!
//! 1. The program's runtime files (`client.json`, `program.client.json`, `themes.css`, `language.json`, `locales/`)
//!    come from a client-mode compile into a scratch directory, the same files `orb compile` ships.
//! 2. Unless `--no-emulators`, the shared dev Firebase emulators start (`@almadar/db/emulator`, run with Bun): the dev
//!    personas are the app's `[identity]` rows signed in as Auth-emulator users, and the page reads the emulator from
//!    the served `/__/firebase/init.json`.
//! 3. `orbital_server::run_standalone` serves the API, the runtime files and the prebuilt client on one port, the store
//!    in memory with sample rows unless `--no-mock` (then the environment picks it: `DATA_BACKEND`, ...).

use crate::commands::compile::compile_schema;
use crate::utils::find_bun;
use crate::{HostArg, ServerFrameworkArg, ShellMode, ShellType};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub open: bool,
    pub mock: bool,
    pub emulators: bool,
    pub client_dir: Option<PathBuf>,
}

pub(crate) fn run_serve(path: &Path, options: &ServeOptions) -> anyhow::Result<()> {
    let client_dir = match &options.client_dir {
        Some(dir) => dir.clone(),
        None => crate::utils::shell_programs::locate("typescript")?.description.web_client.ok_or_else(|| {
            anyhow::anyhow!(
                "the installed typescript shell ships no prebuilt web client: run `orb update` (in a shell checkout, build it with `node scripts/build-web-client.mjs`)"
            )
        })?,
    };
    if !client_dir.join("index.html").is_file() {
        anyhow::bail!(
            "{} holds no index.html: it is not a built web client",
            client_dir.display()
        );
    }

    let scratch = std::env::temp_dir().join(format!("orb-serve-{}", std::process::id()));
    let app = scratch.join("app");
    let assets = scratch.join("assets");
    std::fs::create_dir_all(&scratch)?;
    println!("Compiling {}...", path.display());
    compile_schema(
        &path.to_path_buf(),
        &app,
        ShellType::Typescript,
        ShellMode::Client,
        None,
        None,
        None,
        None,
        false,
        false,
        ServerFrameworkArg::Express,
        HostArg::Kernel,
        None,
    )?;
    copy_runtime_files(&app, &assets)?;

    let mut env = vec![("NODE_ENV".to_string(), "development".to_string())];
    if options.emulators {
        let emulator_env = start_dev_emulators(&scratch)?;
        write_hosted_config(&assets, &emulator_env)?;
        env.extend(emulator_env);
    } else {
        println!(
            "Serving without the Firebase emulators: no dev personas, every request is anonymous."
        );
    }

    let program = crate::commands::dev::resolve_to_temp(path)?;
    let url = format!("http://{}:{}", options.host, options.port);
    println!("\nServing {} on {url}\n", path.display());
    if options.open {
        open_when_listening(url, options.port);
    }
    orbital_server::run_standalone(
        &program,
        &orbital_server::StandaloneConfig {
            host: options.host.clone(),
            port: options.port,
            client_dir: Some(client_dir),
            assets_dir: Some(assets),
            mock: options.mock,
            env,
        },
    )
}

/// The program's files the web client reads at startup: the client-mode compile's `packages/client/public/` and its
/// `locales/` (served at `/locales`).
fn copy_runtime_files(app: &Path, assets: &Path) -> anyhow::Result<()> {
    copy_dir(&app.join("packages/client/public"), assets)?;
    copy_dir(&app.join("locales"), &assets.join("locales"))?;
    if !assets.join("client.json").is_file() {
        anyhow::bail!("the client-mode compile wrote no client.json: the installed typescript shell predates the prebuilt web client; run `orb update`");
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The page's Firebase config, as Firebase Hosting serves it, naming the Auth emulator.
fn hosted_config(emulator_env: &[(String, String)]) -> Option<serde_json::Value> {
    let get = |key: &str| {
        emulator_env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    Some(serde_json::json!({
        "apiKey": "emulator",
        "projectId": get("FIREBASE_PROJECT_ID")?,
        "authEmulatorHost": get("FIREBASE_AUTH_EMULATOR_HOST")?,
    }))
}

fn write_hosted_config(assets: &Path, emulator_env: &[(String, String)]) -> anyhow::Result<()> {
    let Some(config) = hosted_config(emulator_env) else {
        anyhow::bail!("the dev emulators answered no Auth emulator host and project");
    };
    let dir = assets.join("__/firebase");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("init.json"),
        serde_json::to_string_pretty(&config)?,
    )?;
    Ok(())
}

/// The installed `@almadar/db`'s `./emulator` entry, from its `exports` (resolved from the working directory, then the
/// user store).
fn emulator_module() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let user_store =
        orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    let package = orbital_core::behavior_packages::find_npm_package("@almadar/db", &cwd, &user_store).ok_or_else(|| {
        anyhow::anyhow!(
            "the dev emulators come from @almadar/db, which is not installed here or in the user store: add it to the project (`bun add -d @almadar/db`), or pass --no-emulators"
        )
    })?;
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(package.root.join("package.json"))?)?;
    let entry = manifest
        .pointer("/exports/.~1emulator/import")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "@almadar/db {} exports no ./emulator entry",
                package.version
            )
        })?;
    Ok(package.root.join(entry))
}

/// Start (or reuse) the shared dev emulators and read back their env (`FIREBASE_PROJECT_ID`, `*_EMULATOR_HOST`, ...).
fn start_dev_emulators(scratch: &Path) -> anyhow::Result<Vec<(String, String)>> {
    println!("Starting the Firebase emulators (Firestore + Auth)...");
    let module = emulator_module()?;
    let script = scratch.join("emulators.mjs");
    let answer = scratch.join("emulators.json");
    std::fs::write(
        &script,
        format!(
            "import {{ writeFileSync }} from 'node:fs';\nimport {{ homedir }} from 'node:os';\nimport {{ join }} from 'node:path';\n\
             import {{ startDevEmulators }} from {};\n\
             const dataDir = process.env.ALMADAR_EMULATOR_DATA_DIR ?? join(homedir(), '.almadar', 'emulator-data');\n\
             const {{ env }} = await startDevEmulators(dataDir, {{ ...process.env, NODE_ENV: 'development' }});\n\
             writeFileSync({}, JSON.stringify(env));\nprocess.exit(0);\n",
            serde_json::to_string(&format!("file://{}", module.display()))?,
            serde_json::to_string(&answer.display().to_string())?
        ),
    )?;
    let status = Command::new(find_bun()?).arg(&script).status()?;
    if !status.success() {
        anyhow::bail!(
            "the Firebase emulators did not start (they need firebase-tools and Java 21; see the output above). Pass --no-emulators to serve without dev personas."
        );
    }
    let env: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&answer)?)?;
    Ok(env.into_iter().collect())
}

fn open_when_listening(url: String, port: u16) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = Command::new(opener)
            .arg(&url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_page_signs_in_on_the_auth_emulator_under_its_project() {
        let env = pairs(&[
            ("FIREBASE_PROJECT_ID", "demo-almadar"),
            ("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9099"),
            ("FIRESTORE_EMULATOR_HOST", "127.0.0.1:8080"),
        ]);
        assert_eq!(
            hosted_config(&env),
            Some(
                serde_json::json!({ "apiKey": "emulator", "projectId": "demo-almadar", "authEmulatorHost": "127.0.0.1:9099" })
            )
        );
    }

    #[test]
    fn control_no_auth_emulator_means_no_page_sign_in_config() {
        assert_eq!(
            hosted_config(&pairs(&[("FIREBASE_PROJECT_ID", "demo-almadar")])),
            None
        );
        assert_eq!(hosted_config(&[]), None);
    }

    #[test]
    fn the_runtime_files_are_the_public_folder_with_the_locales_beside_it() {
        let root = std::env::temp_dir().join(format!("orb-serve-files-{}", std::process::id()));
        let app = root.join("app");
        std::fs::create_dir_all(app.join("packages/client/public/nested")).unwrap();
        std::fs::create_dir_all(app.join("locales")).unwrap();
        std::fs::write(app.join("packages/client/public/client.json"), "{}").unwrap();
        std::fs::write(app.join("packages/client/public/nested/a.txt"), "a").unwrap();
        std::fs::write(app.join("locales/en.json"), "{}").unwrap();
        std::fs::write(app.join("package.json"), "{}").unwrap();
        copy_runtime_files(&app, &root.join("assets")).unwrap();
        assert!(root.join("assets/client.json").is_file());
        assert!(root.join("assets/nested/a.txt").is_file());
        assert!(root.join("assets/locales/en.json").is_file());
        assert!(
            !root.join("assets/package.json").exists(),
            "only the client's files are served"
        );
        std::fs::remove_file(app.join("packages/client/public/client.json")).unwrap();
        assert!(
            copy_runtime_files(&app, &root.join("assets2")).is_err(),
            "control: a compile without client.json is refused"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
