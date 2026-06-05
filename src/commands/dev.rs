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

    let path_clone = path.clone();

    // Start server in background thread (always uses mock data in dev mode)
    let server_handle =
        thread::spawn(move || orbital_server::run(&path_clone, "127.0.0.1", port, true));

    if display_available {
        // Give server a moment to start before opening GUI
        thread::sleep(std::time::Duration::from_millis(500));

        // Start GUI client pointing to local server
        let server_url = format!("http://127.0.0.1:{}", port);
        orbital_client::run_desktop(path, Some(&server_url))?;
    } else {
        // Headless: block on the server thread so the process stays alive
        server_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Server thread panicked"))??;
    }

    Ok(())
}
