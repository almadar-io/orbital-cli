//! `orb dev` — HTTP dev server with optional desktop GUI.

use std::path::PathBuf;

/// Detect whether a graphical display is available
pub(crate) fn has_display() -> bool {
    // X11
    if std::env::var("DISPLAY").is_ok() {
        return true;
    }
    // Wayland
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("WAYLAND_SOCKET").is_ok() {
        return true;
    }
    // macOS / Windows always have a display when running interactively
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return true;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    false
}

/// Run development mode: HTTP server + optional GUI
///
/// In headless environments (no DISPLAY/WAYLAND_DISPLAY), the GUI is skipped
/// and only the API server is started. Access the schema at the printed URL.
pub(crate) fn run_dev_mode(path: &PathBuf, port: u16, force_headless: bool) -> anyhow::Result<()> {
    use std::thread;

    let display_available = !force_headless && has_display();

    println!("Starting Almadar dev server on http://127.0.0.1:{}", port);
    if !display_available {
        println!("No display detected — running in headless API-only mode.");
        println!("Access the schema API at http://127.0.0.1:{}/api", port);
        println!("(Set DISPLAY or WAYLAND_DISPLAY to enable the desktop GUI)");
    }

    // Resolve the schema (inline `uses:`, expand composers, collapse trait refs) BEFORE
    // serving — the runtime registers traits by their resolved names, so a raw `uses:`-based
    // organism otherwise 404s every event with "Trait not found: X.traits.Y". Server + desktop
    // both consume the resolved `.orb`. (The CLI owns the resolver; the runtime doesn't.)
    let resolved_path = resolve_to_temp(path)?;
    let serve_path = resolved_path.clone();

    // Start server in background thread (always uses mock data in dev mode)
    let server_handle =
        thread::spawn(move || orbital_server::run(&serve_path, "127.0.0.1", port, true));

    if display_available {
        // Wait for the server to actually bind the port before opening the GUI. The server
        // registers + seeds the (possibly large) schema BEFORE it binds, so a fixed sleep
        // races startup: the desktop's one-shot INIT fires early, 404s, and never retries.
        wait_for_port(port, std::time::Duration::from_secs(15))?;

        // Start GUI client pointing to local server (also on the resolved schema).
        let server_url = format!("http://127.0.0.1:{}", port);
        crate::utils::desktop::window(crate::utils::desktop::RENDERER, &resolved_path, Some(&server_url))?;
    } else {
        // Headless: block on the server thread so the process stays alive
        server_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Server thread panicked"))??;
    }

    Ok(())
}

/// Resolve a schema (Inline + Compose + Enrich) and write the resolved `.orb` to a temp
/// file, returning its path. The runtime entry points (`orbital_server::run`,
/// `orb-desktop window`) parse a path; resolution lives here in the CLI, which owns the compiler.
fn resolve_to_temp(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let schema = orbital_compiler::parse_file(path)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    let options = orbital_compiler::CompilerOptions::from_env();
    let (resolved, lazy) =
        orbital_compiler::phases::lazy::resolve_program_with_path(schema, &options, Some(path))
            .map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    let out = std::env::temp_dir().join(format!("orbital-dev-{}.orb", std::process::id()));
    std::fs::write(&out, serde_json::to_string(&resolved)?)?;
    let program_dir = path.parent().unwrap_or(std::path::Path::new("."));
    orbital_compiler::phases::messages::write_catalogs(&out, &resolved, program_dir)?;
    crate::commands::parse::write_lazy(
        out.parent().unwrap_or(std::path::Path::new(".")),
        &lazy,
        program_dir,
        false,
    )?;
    Ok(out)
}

/// Poll until the local server binds `port` (it binds only after register + seed, so a
/// successful connect means it is ready to serve), or the deadline passes.
fn wait_for_port(port: u16, timeout: std::time::Duration) -> anyhow::Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("dev server on {addr} did not become ready within {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
