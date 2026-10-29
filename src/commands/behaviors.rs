//! `orb behaviors which` and the behavior-package compatibility check `orb add`
//! / `orb update` run (`verify_package`) — behavior packages are ordinary npm
//! packages resolved node-style (Almadar_Behavior_Packages.md).

use orbital_compiler::loader::{ExternalOrbitalLoader, LoaderConfig};
use orbital_core::behavior_packages::{core_satisfies, std_peer_satisfies, BehaviorPackage};

use crate::baked_engine_pins::BAKED_CORE;

use super::npm_project::discover_from;

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

/// Verify one package's `core_range` (against the version of `@almadar/core` this `orb` was baked
/// against) when declared, and its `std_peer_range` (against the installed
/// std version) when declared. `Err` carries the human-readable reason naming
/// both versions in each case.
pub(crate) fn verify_package(pkg: &BehaviorPackage, packages: &[BehaviorPackage]) -> anyhow::Result<()> {
    if let Some(range) = &pkg.core_range {
        // The installed @almadar/core is the language the program is checked
        // against; BAKED_CORE is only this binary's pin for `orb update`.
        let (core_version, core_source) = match orbital_core::language_data::language_data() {
            Ok(data) => (data.core_version.as_str(), "the installed"),
            Err(_) => (BAKED_CORE.1, "the running orb pins"),
        };
        let core_ok = core_satisfies(range, core_version)
            .map_err(|e| anyhow::anyhow!("invalid core range '{range}': {e}"))?;
        if !core_ok {
            anyhow::bail!(
                "{}@{} requires @almadar/core {range} but {core_source} @almadar/core is {core_version}",
                pkg.name,
                pkg.version,
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

/// The file `spec` resolves to from the working directory's installed packages and the package
/// that owns it, as `orb behaviors which` prints them; the inner error is the loader's refusal.
pub(crate) fn which(spec: &str) -> anyhow::Result<Result<(std::path::PathBuf, String), String>> {
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let config = LoaderConfig {
        base_path: cwd,
        packages,
        ..Default::default()
    };
    let mut loader = ExternalOrbitalLoader::new(config);
    Ok(loader
        .resolve_owned(spec)
        .map(|(path, owner)| (path, owner.to_string()))
        .map_err(|e| e.to_string()))
}
