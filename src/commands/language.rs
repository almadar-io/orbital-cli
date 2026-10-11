//! `orb language which` and the language checks `orb add` / `orb update` run — the language data
//! (`@almadar/core`: pattern vocabulary, theme presets, fixtures), the
//! operator table (`@almadar/std`) and the language packs (every language but
//! English, `orbital_core::i18n::packs`) are installed npm packages resolved
//! node-style (`orbital_core::language_data`, Almadar_Behavior_Packages.md §14),
//! same mechanism as `orb behaviors` / `orb engines`; bun is the installer.
//! `orb update` installs everything a program needs; `orb add <pack>` extends
//! the language with another one.

use std::path::{Path, PathBuf};

use orbital_core::behavior_packages::{find_npm_package, user_store_dir, STD_PACKAGE};
use orbital_core::i18n::packs::discover_vocabulary_packs;
use orbital_core::language_data::{load_language_data, load_operator_data, CORE_PACKAGE};


fn user_store_for(dir: &Path) -> PathBuf {
    user_store_dir().unwrap_or_else(|| dir.to_path_buf())
}

/// `Ok(summary)` when the installed core's language data loads for this orb.
pub(crate) fn check_core(dir: &Path) -> Result<String, String> {
    load_language_data(dir, &user_store_for(dir))
        .map(|d| {
            format!(
                "{CORE_PACKAGE}@{} ({} theme presets)",
                d.core_version,
                d.themes.len()
            )
        })
        .map_err(|e| format!("{}: {e}", e.code()))
}

/// The installed language packs, or why they cannot load.
pub(crate) fn check_packs(dir: &Path) -> Result<Vec<String>, String> {
    discover_vocabulary_packs(dir, &user_store_for(dir))
        .map(|packs| {
            packs
                .iter()
                .map(|p| format!("{}@{} ({})", p.package, p.version, p.definition.meta.code))
                .collect()
        })
        .map_err(|e| format!("{}: {e}", e.code()))
}

/// `@scope/name@1.0.0` → `@scope/name`; `name@^1` → `name`.
pub(crate) fn package_name(spec: &str) -> &str {
    let at = if let Some(rest) = spec.strip_prefix('@') {
        rest.find('@').map(|i| i + 1)
    } else {
        spec.find('@')
    };
    at.map_or(spec, |i| &spec[..i])
}

#[derive(serde::Serialize)]
struct Row {
    package: String,
    provides: String,
    version: Option<String>,
    path: Option<String>,
    link: bool,
    /// `None` when the data loads for this orb; else the structured error.
    problem: Option<String>,
}

fn rows(dir: &Path) -> Vec<Row> {
    let store = user_store_for(dir);
    let problem = |r: Result<(), orbital_core::language_data::LanguageDataError>| {
        r.err().map(|e| format!("{}: {e}", e.code()))
    };
    [
        (
            CORE_PACKAGE,
            "patterns, theme presets, fixtures",
            problem(load_language_data(dir, &store).map(|_| ())),
        ),
        (
            STD_PACKAGE,
            "operator table",
            problem(load_operator_data(dir, &store).map(|_| ())),
        ),
    ]
    .into_iter()
    .map(|(package, provides, problem)| {
        let pkg = find_npm_package(package, dir, &store);
        Row {
            package: package.to_string(),
            provides: provides.to_string(),
            version: pkg.as_ref().map(|p| p.version.clone()),
            path: pkg.as_ref().map(|p| p.root.display().to_string()),
            link: pkg.as_ref().is_some_and(|p| p.is_link),
            problem,
        }
    })
    .chain(pack_rows(dir, &store))
    .collect()
}

/// One row per installed language pack, or one row naming why they do not load.
fn pack_rows(dir: &Path, store: &Path) -> Vec<Row> {
    match discover_vocabulary_packs(dir, store) {
        Ok(packs) => packs
            .into_iter()
            .map(|p| {
                let pkg = find_npm_package(&p.package, dir, store);
                Row {
                    provides: format!(
                        "language: {} ({})",
                        p.definition.meta.name, p.definition.meta.code
                    ),
                    version: Some(p.version),
                    path: pkg.as_ref().map(|n| n.root.display().to_string()),
                    link: pkg.as_ref().is_some_and(|n| n.is_link),
                    package: p.package,
                    problem: None,
                }
            })
            .collect(),
        Err(e) => vec![Row {
            package: "language packs".to_string(),
            provides: "every language but English".to_string(),
            version: None,
            path: None,
            link: false,
            problem: Some(format!("{}: {e}", e.code())),
        }],
    }
}

/// Which copies compile: each language package's resolved path, or why not.
pub(crate) fn run_which() -> anyhow::Result<()> {
    for row in rows(&std::env::current_dir()?) {
        match (&row.path, &row.problem) {
            (Some(path), None) => println!(
                "{}@{} → {path}",
                row.package,
                row.version.as_deref().unwrap_or("?")
            ),
            (_, Some(problem)) => println!("{} → {problem}", row.package),
            (None, None) => println!("{} → not installed", row.package),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::package_name;

    #[test]
    fn a_spec_names_its_package_with_or_without_a_version() {
        assert_eq!(package_name("@someone/lolo-tr"), "@someone/lolo-tr");
        assert_eq!(package_name("@someone/lolo-tr@1.2.0"), "@someone/lolo-tr");
        assert_eq!(package_name("lolo-tr@^1"), "lolo-tr");
        assert_eq!(package_name("lolo-tr"), "lolo-tr");
    }
}
