//! `orb engines install|list|update|remove` — the two node-based verify
//! engines (the runtime walk and `--compiled`) are the two bins (`runtime-verify`,
//! `orbital-verify`) of ONE ordinary npm package, `@almadar-io/runtime-verify-tool`,
//! resolved node-style, same as `orb behaviors` (`commands/npm_project.rs`); bun
//! (`utils::find_bun`) is the installer.

use std::path::{Path, PathBuf};

use orbital_core::behavior_packages::{find_npm_package, user_store_dir};

use super::engines::{engine_pin, not_installed_message, Engine};
use super::npm_project::{project_dir, run_bun};

/// Neither `--runtime` nor `--compiled` given = both.
fn requested_engines(runtime: bool, compiled: bool) -> Vec<Engine> {
    if !runtime && !compiled {
        return vec![Engine::Runtime, Engine::Compiled];
    }
    let mut v = Vec::new();
    if runtime {
        v.push(Engine::Runtime);
    }
    if compiled {
        v.push(Engine::Compiled);
    }
    v
}

/// The engines to act on, one per distinct package: both engines share a package, which bun adds, updates and removes once.
fn distinct_packages(engines: Vec<Engine>) -> Vec<(&'static str, Vec<Engine>)> {
    let mut out: Vec<(&'static str, Vec<Engine>)> = Vec::new();
    for engine in engines {
        let (package, _bin_name, _pinned_version) = engine_pin(engine);
        match out.iter_mut().find(|(p, _)| *p == package) {
            Some((_, group)) => group.push(engine),
            None => out.push((package, vec![engine])),
        }
    }
    out
}

fn user_store_for(dir: &Path) -> PathBuf {
    user_store_dir().unwrap_or_else(|| dir.to_path_buf())
}

pub(crate) fn run_install(
    global: bool,
    runtime: bool,
    compiled: bool,
    no_browsers: bool,
) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let user_store = user_store_for(&dir);

    for (package, engines) in distinct_packages(requested_engines(runtime, compiled)) {
        let (_, _, pinned_version) = engine_pin(engines[0]);
        run_bun(&dir, &["add", "-d", &format!("{package}@{pinned_version}")])?;

        for engine in engines {
            let (_, bin_name, pinned_version) = engine_pin(engine);
            match find_npm_package(package, &dir, &user_store) {
                Some(pkg) if pkg.version == pinned_version && pkg.bin.contains_key(bin_name) => {
                    println!(
                        "installed {package}@{} (entry {bin_name} → {})",
                        pkg.version,
                        pkg.bin[bin_name].display()
                    );
                }
                Some(pkg) => {
                    let reason = if pkg.version != pinned_version {
                        format!("expected version {pinned_version}, got {}", pkg.version)
                    } else {
                        format!("installed package has no `{bin_name}` bin entry")
                    };
                    run_bun(&dir, &["remove", package]).ok();
                    anyhow::bail!("{package}: {reason}");
                }
                None => {
                    run_bun(&dir, &["remove", package]).ok();
                    anyhow::bail!("{package}: bun add reported success but the package was not found");
                }
            }
        }
    }

    if !no_browsers {
        run_bun(&dir, &["x", "playwright", "install", "chromium"])?;
    }
    Ok(())
}

pub(crate) fn run_update(global: bool, runtime: bool, compiled: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let user_store = user_store_for(&dir);

    for (package, engines) in distinct_packages(requested_engines(runtime, compiled)) {
        let (_, _, pinned_version) = engine_pin(engines[0]);
        let before = find_npm_package(package, &dir, &user_store).map(|p| p.version);

        run_bun(&dir, &["add", "-d", &format!("{package}@{pinned_version}")])?;

        for engine in engines {
            let (_, bin_name, pinned_version) = engine_pin(engine);
            match find_npm_package(package, &dir, &user_store) {
                Some(pkg) if pkg.version == pinned_version && pkg.bin.contains_key(bin_name) => {
                    match &before {
                        Some(prev) if prev != &pkg.version => {
                            println!("updated {package}: {prev} → {}", pkg.version)
                        }
                        Some(prev) => println!("{package}@{prev} already up to date"),
                        None => println!("installed {package}@{}", pkg.version),
                    }
                }
                Some(pkg) => {
                    let reason = if pkg.version != pinned_version {
                        format!("expected version {pinned_version}, got {}", pkg.version)
                    } else {
                        format!("installed package has no `{bin_name}` bin entry")
                    };
                    anyhow::bail!("{package}: {reason} (bun has no per-package revert)");
                }
                None => anyhow::bail!(
                    "{package}: bun add reported success but the package was not found (bun has no per-package revert)"
                ),
            }
        }
    }
    Ok(())
}

pub(crate) fn run_remove(global: bool, runtime: bool, compiled: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    for (package, _engines) in distinct_packages(requested_engines(runtime, compiled)) {
        run_bun(&dir, &["remove", package])?;
        println!("removed {package}");
    }
    Ok(())
}

pub(crate) fn run_list(json: bool, global: bool) -> anyhow::Result<()> {
    let dir = if global {
        user_store_dir().ok_or_else(|| {
            anyhow::anyhow!("could not determine a home directory for the user store")
        })?
    } else {
        std::env::current_dir()?
    };
    let user_store = user_store_for(&dir);

    #[derive(serde::Serialize)]
    struct Row {
        /// The engine's bin; the runtime walk and `--compiled` are two bins of one package.
        engine: String,
        package: String,
        version: Option<String>,
        entry: Option<String>,
        path: Option<String>,
        installed: bool,
        link: bool,
    }

    let rows: Vec<Row> = [Engine::Runtime, Engine::Compiled]
        .into_iter()
        .map(|engine| {
            let (package, bin_name, _pinned_version) = engine_pin(engine);
            match find_npm_package(package, &dir, &user_store) {
                Some(pkg) => Row {
                    engine: bin_name.to_string(),
                    package: package.to_string(),
                    version: Some(pkg.version.clone()),
                    entry: pkg.bin.contains_key(bin_name).then(|| bin_name.to_string()),
                    path: pkg.bin.get(bin_name).map(|p| p.display().to_string()),
                    installed: true,
                    link: pkg.is_link,
                },
                None => Row {
                    engine: bin_name.to_string(),
                    package: package.to_string(),
                    version: None,
                    entry: None,
                    path: None,
                    installed: false,
                    link: false,
                },
            }
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    for row in &rows {
        if row.installed {
            println!(
                "{:<32} {:<16} {:<12} {:<6}  {}",
                row.package,
                row.engine,
                row.version.as_deref().unwrap_or("?"),
                row.link,
                row.path.as_deref().unwrap_or("")
            );
        } else {
            println!(
                "{:<32} {:<16} not installed — {}",
                row.package,
                row.engine,
                not_installed_message(&row.package)
            );
        }
    }
    Ok(())
}
