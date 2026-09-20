//! `orb behaviors install|remove|update|list|which` — behavior packages are
//! ordinary npm packages resolved node-style (Almadar_Behavior_Packages.md).
//! bun (already shipped beside `orb`, `utils::find_bun`) is the installer;
//! `orb` adds no manifest, no store format, no environment variable.

use orbital_compiler::loader::{index_registry, ExternalOrbitalLoader, LoaderConfig};
use orbital_core::behavior_packages::{
    compiler_satisfies, core_satisfies, std_peer_satisfies, user_store_dir, BehaviorPackage,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::baked_engine_pins::BAKED_CORE;

use super::npm_project::{discover_from, project_dir, run_bun};

/// `dependencies` + `devDependencies` from the project's own `package.json`,
/// name -> declared range (`dependencies` wins on a name collision, read
/// after `devDependencies` so it overwrites).
fn manifest_ranges(dir: &Path) -> anyhow::Result<HashMap<String, String>> {
    let path = dir.join("package.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("invalid package.json at {}: {e}", path.display()))?;
    let mut ranges = HashMap::new();
    for field in ["devDependencies", "dependencies"] {
        if let Some(obj) = value.get(field).and_then(|v| v.as_object()) {
            for (name, range) in obj {
                if let Some(range) = range.as_str() {
                    ranges.insert(name.clone(), range.to_string());
                }
            }
        }
    }
    Ok(ranges)
}

/// `discover()`'d packages keyed by name, carrying just the (version, root)
/// pair a before/after snapshot needs to detect a change.
fn discovered_versions(packages: &[BehaviorPackage]) -> HashMap<String, (String, PathBuf)> {
    packages
        .iter()
        .map(|p| (p.name.clone(), (p.version.clone(), p.root.clone())))
        .collect()
}

/// The `version` field read straight from `<dir>/node_modules/<name>/package.json`
/// — for a manifest-declared package `discover()` skipped because its
/// `package.json` carries no `orb` block, so there is no `BehaviorPackage` to
/// read a version from.
fn installed_version(dir: &Path, name: &str) -> Option<String> {
    let raw =
        std::fs::read_to_string(dir.join("node_modules").join(name).join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("version")?.as_str().map(|s| s.to_string())
}

/// Packages a bun run actually touched: every name whose declared manifest
/// range is new/changed, unioned with every name whose discovered
/// version/root is new/changed, comparing an "after" snapshot against a
/// "before" one straddling the bun invocation. Deterministic and name-based —
/// no parsing of the CLI's install/update spec argument, which may be a local
/// path or `link:` spec that never matches the resolved package name.
fn compute_affected(
    before_manifest: &HashMap<String, String>,
    after_manifest: &HashMap<String, String>,
    before_discovered: &HashMap<String, (String, PathBuf)>,
    after_discovered: &HashMap<String, (String, PathBuf)>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut affected = Vec::new();
    for (name, range) in after_manifest {
        if before_manifest.get(name) != Some(range) && seen.insert(name.clone()) {
            affected.push(name.clone());
        }
    }
    for (name, info) in after_discovered {
        if before_discovered.get(name) != Some(info) && seen.insert(name.clone()) {
            affected.push(name.clone());
        }
    }
    affected.sort();
    affected
}

/// The std peer version to check an io package's `peerDependencies` against:
/// the installed `almadar-std` package's version. `@almadar/std` is a real
/// npm-installed dependency now (Phase 2) — there is no baked fallback, so a
/// missing std is a hard, actionable error naming the install command.
fn std_version_for_peer_check(packages: &[BehaviorPackage]) -> anyhow::Result<String> {
    packages
        .iter()
        .find(|p| p.prefix == "almadar-std")
        .map(|p| p.version.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(orbital_core::behavior_packages::not_installed_message(
                orbital_core::behavior_packages::STD_PACKAGE
            ))
        })
}

/// Verify one package's `compiler` range (against the running `orb`), its
/// `core_range` (against the version of `@almadar/core` this `orb` was baked
/// against) when declared, and its `std_peer_range` (against the installed
/// std version) when declared. `Err` carries the human-readable reason naming
/// both versions in each case.
fn verify_package(pkg: &BehaviorPackage, packages: &[BehaviorPackage]) -> anyhow::Result<()> {
    let running = env!("CARGO_PKG_VERSION");
    let compiler_ok = compiler_satisfies(&pkg.compiler_range, running)
        .map_err(|e| anyhow::anyhow!("invalid compiler range '{}': {e}", pkg.compiler_range))?;
    if !compiler_ok {
        anyhow::bail!(
            "{}@{} requires orb {} but the running orb is {running}",
            pkg.name,
            pkg.version,
            pkg.compiler_range
        );
    }

    if let Some(range) = &pkg.core_range {
        let core_ok = core_satisfies(range, BAKED_CORE.1)
            .map_err(|e| anyhow::anyhow!("invalid core range '{range}': {e}"))?;
        if !core_ok {
            anyhow::bail!(
                "{}@{} requires @almadar/core {range} but the running orb bakes @almadar/core {}",
                pkg.name,
                pkg.version,
                BAKED_CORE.1
            );
        }
    }

    if let Some(range) = &pkg.std_peer_range {
        let std_version = std_version_for_peer_check(packages)?;
        let peer_ok = std_peer_satisfies(range, &std_version)
            .map_err(|e| anyhow::anyhow!("invalid std peer range '{range}': {e}"))?;
        if !peer_ok {
            anyhow::bail!(
                "{}@{} requires @almadar/std {range} but the resolved std is {std_version}",
                pkg.name,
                pkg.version
            );
        }
    }

    Ok(())
}

fn print_installed_line(pkg: &BehaviorPackage) {
    println!(
        "installed {}@{} (prefix {}, {} behaviors, {})",
        pkg.name,
        pkg.version,
        pkg.prefix,
        index_registry(&pkg.registry).len(),
        pkg.root.display()
    );
}

/// The clear, actionable error for a manifest-declared package `discover()`
/// skipped because its `package.json` carries no `orb` block — the
/// pre-Phase-2 shape of `@almadar/std`/`@almadar-io/behaviors`.
fn not_a_behavior_package_error(
    dir: &Path,
    after_manifest: &HashMap<String, String>,
    name: &str,
) -> anyhow::Error {
    let version = installed_version(dir, name)
        .or_else(|| after_manifest.get(name).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    anyhow::anyhow!(
        "{name}@{version} is not a behavior package (its package.json has no `orb` \
         block); a pre-Phase-2 release of @almadar/std or @almadar-io/behaviors needs \
         upgrading"
    )
}

pub(crate) fn run_install(pkg: Option<String>, global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;

    let before_manifest = manifest_ranges(&dir)?;
    let before_discovered = discovered_versions(&discover_from(&dir)?);

    match &pkg {
        Some(spec) => run_bun(&dir, &["add", spec])?,
        None => run_bun(&dir, &["install"])?,
    }

    let after_manifest = manifest_ranges(&dir)?;
    let packages = discover_from(&dir)?;
    let after_discovered = discovered_versions(&packages);
    let affected = compute_affected(
        &before_manifest,
        &after_manifest,
        &before_discovered,
        &after_discovered,
    );

    for name in &affected {
        match packages.iter().find(|p| &p.name == name) {
            Some(candidate) => {
                if let Err(e) = verify_package(candidate, &packages) {
                    run_bun(&dir, &["remove", &candidate.name]).ok();
                    return Err(e);
                }
            }
            None => {
                let err = not_a_behavior_package_error(&dir, &after_manifest, name);
                run_bun(&dir, &["remove", name]).ok();
                return Err(err);
            }
        }
    }

    for name in &affected {
        if let Some(candidate) = packages.iter().find(|p| &p.name == name) {
            print_installed_line(candidate);
        }
    }
    Ok(())
}

pub(crate) fn run_update(pkg: Option<String>, global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;

    let before_manifest = manifest_ranges(&dir)?;
    let before_discovered = discovered_versions(&discover_from(&dir)?);

    match &pkg {
        Some(spec) => run_bun(&dir, &["update", spec])?,
        None => run_bun(&dir, &["update"])?,
    }

    let after_manifest = manifest_ranges(&dir)?;
    let packages = discover_from(&dir)?;
    let after_discovered = discovered_versions(&packages);
    let affected = compute_affected(
        &before_manifest,
        &after_manifest,
        &before_discovered,
        &after_discovered,
    );

    let mut failures = Vec::new();
    for name in &affected {
        match packages.iter().find(|p| &p.name == name) {
            Some(candidate) => match verify_package(candidate, &packages) {
                Ok(()) => print_installed_line(candidate),
                Err(e) => failures.push(e.to_string()),
            },
            None => {
                failures.push(not_a_behavior_package_error(&dir, &after_manifest, name).to_string())
            }
        }
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "update regression, package left installed (bun has no per-package revert):\n{}",
            failures.join("\n")
        );
    }
    Ok(())
}

/// `<prefix>/` immediately preceded by a double quote, inside a `.lolo`/`.orb`
/// file under `dir` (skipping `node_modules`, `.git`, `dist`, `target`).
fn references_prefix(dir: &Path, prefix: &str) -> anyhow::Result<Vec<PathBuf>> {
    let needle = format!("\"{prefix}/");
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some("node_modules" | ".git" | "dist" | "target")
                ) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let is_lolo_or_orb = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "lolo" || e == "orb");
            if !is_lolo_or_orb {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if content.contains(&needle) {
                    hits.push(path);
                }
            }
        }
    }
    Ok(hits)
}

pub(crate) fn run_remove(pkg: String, global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let packages = discover_from(&dir)?;
    let target = packages
        .iter()
        .find(|p| p.name == pkg)
        .ok_or_else(|| anyhow::anyhow!("'{pkg}' is not an installed behavior package"))?;

    let referencing = references_prefix(&dir, &target.prefix)?;
    if !referencing.is_empty() {
        let files = referencing
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!(
            "refusing to remove '{pkg}': {} file(s) still import '{}/':\n  {files}",
            referencing.len(),
            target.prefix
        );
    }

    run_bun(&dir, &["remove", &pkg])
}

pub(crate) fn run_list(json: bool, global: bool) -> anyhow::Result<()> {
    let dir = if global {
        user_store_dir().ok_or_else(|| {
            anyhow::anyhow!("could not determine a home directory for the user store")
        })?
    } else {
        std::env::current_dir()?
    };
    let packages = discover_from(&dir)?;

    if json {
        #[derive(serde::Serialize)]
        struct Row {
            name: String,
            version: String,
            prefix: String,
            behaviors: usize,
            path: String,
            link: bool,
        }
        let rows: Vec<Row> = packages
            .iter()
            .map(|p| Row {
                name: p.name.clone(),
                version: p.version.clone(),
                prefix: p.prefix.clone(),
                behaviors: index_registry(&p.registry).len(),
                path: p.root.display().to_string(),
                link: p.is_link,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "{:<28} {:<12} {:<20} {:>9}  {:<6}  PATH",
        "NAME", "VERSION", "PREFIX", "BEHAVIORS", "LINK"
    );
    for pkg in &packages {
        println!(
            "{:<28} {:<12} {:<20} {:>9}  {:<6}  {}",
            pkg.name,
            pkg.version,
            pkg.prefix,
            index_registry(&pkg.registry).len(),
            pkg.is_link,
            pkg.root.display()
        );
    }
    Ok(())
}

pub(crate) fn run_which(spec: String) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let config = LoaderConfig {
        base_path: cwd,
        packages,
        ..Default::default()
    };
    let mut loader = ExternalOrbitalLoader::new(config);
    match loader.resolve_owned(&spec) {
        Ok((path, owner)) => {
            println!("{} ({owner})", path.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
