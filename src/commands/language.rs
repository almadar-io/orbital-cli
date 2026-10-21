//! The language checks `orb add` / `orb update` run (`orb language which` is `std-orb-language`) — the language data
//! (`@almadar/core`: pattern vocabulary, theme presets, fixtures), the
//! operator table (`@almadar/std`) and the language packs (every language but
//! English, `orbital_core::i18n::packs`) are installed npm packages resolved
//! node-style (`orbital_core::language_data`, Almadar_Behavior_Packages.md §14),
//! same mechanism as `orb behaviors` / `orb engines`; bun is the installer.
//! `orb update` installs everything a program needs; `orb add <pack>` extends
//! the language with another one.

use std::path::{Path, PathBuf};

use orbital_core::behavior_packages::user_store_dir;
use orbital_core::i18n::packs::discover_vocabulary_packs;
use orbital_core::language_data::{load_language_data, CORE_PACKAGE};

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
