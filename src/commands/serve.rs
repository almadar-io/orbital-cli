//! `orb serve` — a program served by the native server with the TypeScript shell's prebuilt web client: no build,
//! no Node process for the app.
//!
//! 1. The program's runtime files (`client.json`, `program.client.json`, `themes.css`, `language.json`, `locales/`)
//!    come from a client-mode compile into a scratch directory, the same files `orb compile` ships.
//! 2. Unless `--no-emulators`, the shared dev Firebase emulators start through the installed `orb emulators` program
//!    (std `cli/` `std-orb-emulators`, run in process): the dev personas are the app's `[identity]` rows signed in as Auth-emulator users, and the page reads the emulator from
//!    the served `/__/firebase/init.json`.
//! 3. `orbital_server::run_standalone` serves the API, the runtime files and the prebuilt client on one port, the store
//!    in memory with sample rows unless `--no-mock` (then the environment picks it: `DATA_BACKEND`, ...).

use crate::commands::compile::compile_schema;
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

/// What a web client of `path` needs: the prebuilt client, the program's runtime files beside it, and the resolved program.
pub(crate) struct PreparedWeb {
    pub client_dir: PathBuf,
    pub assets: PathBuf,
    pub program: PathBuf,
}

/// Compile `path` client-mode into a scratch directory and lay out its runtime files beside the prebuilt web client
/// (`client_dir`, else the installed typescript shell's).
pub(crate) fn prepare_web(path: &Path, client_dir: Option<&Path>) -> anyhow::Result<PreparedWeb> {
    let client_dir = match client_dir {
        Some(dir) => dir.to_path_buf(),
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
    static PREPARED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = PREPARED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let scratch = std::env::temp_dir().join(format!("orb-serve-{}-{n}", std::process::id()));
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
    let program = crate::commands::dev::resolve_to_temp(path)?;
    Ok(PreparedWeb { client_dir, assets, program })
}

pub(crate) fn run_serve(path: &Path, options: &ServeOptions) -> anyhow::Result<()> {
    let PreparedWeb { client_dir, assets, program } = prepare_web(path, options.client_dir.as_deref())?;

    let mut env = vec![("NODE_ENV".to_string(), "development".to_string())];
    if options.emulators {
        let emulator_env = start_dev_emulators()?;
        write_hosted_config(&assets, &emulator_env)?;
        env.extend(emulator_env);
    } else {
        println!(
            "Serving without the Firebase emulators: no dev personas, every request is anonymous."
        );
    }

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

/// Start (or reuse) the shared dev emulators through the installed `orb emulators` program and read back its
/// `KEY=value` answer (`FIREBASE_PROJECT_ID`, `*_EMULATOR_HOST`, ...).
fn start_dev_emulators() -> anyhow::Result<Vec<(String, String)>> {
    println!("Starting the Firebase emulators (Firestore + Auth)...");
    let cwd = std::env::current_dir()?;
    let store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    let program = crate::commands::program_command::find_command_program("emulators", &cwd, &store)?.ok_or_else(|| {
        anyhow::anyhow!("no installed program provides `orb emulators` (std `cli/` std-orb-emulators): run `orb update`, or pass --no-emulators")
    })?;
    let answer =
        crate::commands::program_command::run_command_program(&program, "emulators", &[], &cwd)?;
    if answer.code != 0 {
        let stderr = answer.stderr.as_deref().unwrap_or("");
        anyhow::bail!(
            "the Firebase emulators did not start: {}{stderr}\nPass --no-emulators to serve without dev personas.",
            answer.body
        );
    }
    env_lines(&answer.body)
}

/// `KEY=value` lines, one per variable; a line without `=` is an error that names it.
fn env_lines(text: &str) -> anyhow::Result<Vec<(String, String)>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!("`orb emulators` answered a line that is not KEY=value: {line}")
                })
        })
        .collect()
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

    const APP: &str = r#"app served-notes [
  version: "1.0.0",
  description: "A one-orbital app served beside std-http-server."
]

orbital NotesOrbital {
  entity Note [runtime] {
    id : string!
  }

  trait Notes -> Note [interaction] {
    initial: idle

    state idle {
      INIT -> idle
    }
  }

  page "/notes" -> Notes
}
"#;

    fn resolved_program(path: &Path) -> orbital_core::Value {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let resolved = crate::commands::dev::resolve_to_temp(path).unwrap_or_else(|e| panic!("resolve {}: {e:#}", path.display()));
        serde_json::from_str(&std::fs::read_to_string(&resolved).unwrap()).unwrap()
    }

    fn app_program(tag: &str) -> orbital_core::Value {
        let dir = std::env::temp_dir().join(format!("orb-serve-composed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("served-notes.lolo");
        std::fs::write(&path, APP).unwrap();
        resolved_program(&path)
    }

    fn std_http_server() -> orbital_core::Value {
        resolved_program(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/almadar-std/behaviors/lolo/net/organisms/std-http-server.lolo"),
        )
    }

    fn program_host() -> orbital_shell_rust::host::SubstratePort {
        orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost))
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn a_composed_app_and_std_http_server_boot_on_the_shell_host_and_answer_health() {
        use std::io::{Read, Write};
        let port = free_port();
        let overrides = serde_json::json!({
            "Http1.port": port,
            "grants": { "net": [{ "target": format!("127.0.0.1:{port}"), "kinds": ["listen"] }] }
        });
        let host = orbital_shell_rust::host::boot::boot_composed(
            vec![app_program("health"), std_http_server()],
            "served-notes",
            Some(overrides),
            program_host(),
            orbital_shell_rust::host::boot::Catalogs::new(),
            true,
            &[("NODE_ENV".to_string(), "development".to_string())].into_iter().collect(),
            true,
        )
        .unwrap_or_else(|e| panic!("boot: {e:#}"));
        let names: Vec<String> = host.list_orbitals().into_iter().map(|o| o.name).collect();
        assert!(names.iter().any(|n| n == "NotesOrbital"), "the app is composed in: {names:?}");
        orbital_shell_rust::host::run::mount_init(&host).unwrap_or_else(|e| panic!("INIT: {e}"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut conn = loop {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(conn) => break conn,
                Err(e) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the composed server never listened on {port}: {e}; open calls {:?}; trace {}",
                        host.open_calls(),
                        serde_json::to_string(&host.trace()).unwrap()
                    );
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        };
        conn.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        conn.write_all(b"GET /api/health HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").unwrap();
        let mut reply = String::new();
        let _ = conn.read_to_string(&mut reply);
        host.cancel_open_calls();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply:?}");
        assert!(reply.contains("\"status\":\"healthy\""), "{reply:?}");
    }

    #[test]
    fn control_a_compose_that_refuses_boots_nothing() {
        let error = orbital_shell_rust::host::boot::boot_composed(
            vec![app_program("refused"), orbital_core::Value::String("not a program".to_string())],
            "served-notes",
            None,
            program_host(),
            orbital_shell_rust::host::boot::Catalogs::new(),
            true,
            &Default::default(),
            false,
        )
        .err()
        .expect("a list holding a non-program is refused");
        assert!(error.to_string().contains("program/compose: not a program"), "{error:#}");
    }

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
    fn the_emulators_answer_is_one_variable_per_line() {
        assert_eq!(
            env_lines("FIRESTORE_EMULATOR_HOST=127.0.0.1:8080\nFIREBASE_PROJECT_ID=demo-almadar\n")
                .unwrap(),
            pairs(&[
                ("FIRESTORE_EMULATOR_HOST", "127.0.0.1:8080"),
                ("FIREBASE_PROJECT_ID", "demo-almadar")
            ])
        );
        assert_eq!(env_lines("").unwrap(), pairs(&[]));
    }

    #[test]
    fn control_a_line_without_equals_is_refused_by_name() {
        let err = env_lines("FIRESTORE_EMULATOR_HOST=127.0.0.1:8080\nemulators_timeout: no")
            .unwrap_err()
            .to_string();
        assert!(err.contains("emulators_timeout: no"), "{err}");
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
