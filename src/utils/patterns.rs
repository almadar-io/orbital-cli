//! Loaders for project-supplied patterns, services registry, and component
//! mappings consumed by the validate/compile commands.


/// Load extra patterns from a patterns.json file.
///
/// If `explicit_path` is provided, load from that path.
/// Otherwise, auto-discover patterns.json adjacent to the schema file or in parent directories.
pub(crate) fn load_extra_patterns(
    schema_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> Option<orbital_compiler::ExtraPatterns> {
    use std::collections::{HashMap, HashSet};

    let patterns_path = if let Some(p) = explicit_path {
        if p.exists() {
            p.to_path_buf()
        } else {
            eprintln!("Warning: patterns file not found: {}", p.display());
            return None;
        }
    } else {
        // Auto-discover: look for patterns.json adjacent to schema, then parent dirs
        let schema_dir = schema_path.parent()?;
        let candidates = [
            schema_dir.join("patterns.json"),
            schema_dir.join("design-system/patterns.json"),
        ];
        match candidates.into_iter().find(|p| p.exists()) {
            Some(p) => p,
            None => return None,
        }
    };

    // Read and parse patterns.json
    let content = match std::fs::read_to_string(&patterns_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: could not read {}: {}", patterns_path.display(), e);
            return None;
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "Warning: invalid JSON in {}: {}",
                patterns_path.display(),
                e
            );
            return None;
        }
    };

    // Parse into ExtraPatterns
    let mut types = HashSet::new();
    let mut props_schema: HashMap<String, HashSet<String>> = HashMap::new();

    if let Some(patterns) = json.get("patterns").and_then(|v| v.as_object()) {
        for (pattern_name, pattern_def) in patterns {
            types.insert(pattern_name.clone());
            // Support both formats:
            // 1. "props": [{ "name": "propName" }]  (legacy array format)
            // 2. "propsSchema": { "propName": { "types": [...] } }  (sync tool format)
            if let Some(props) = pattern_def.get("props").and_then(|v| v.as_array()) {
                let prop_names: HashSet<String> = props
                    .iter()
                    .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect();
                props_schema.insert(pattern_name.clone(), prop_names);
            } else if let Some(schema) = pattern_def.get("propsSchema").and_then(|v| v.as_object())
            {
                let prop_names: HashSet<String> = schema.keys().cloned().collect();
                props_schema.insert(pattern_name.clone(), prop_names);
            }
        }
    }

    if types.is_empty() {
        return None;
    }

    eprintln!(
        "Loaded {} extra patterns from {}",
        types.len(),
        patterns_path.display()
    );

    Some(orbital_compiler::ExtraPatterns {
        types,
        props_schema,
    })
}

/// The one language-data preflight every compiler-backed command runs first:
/// the pattern vocabulary (installed `@almadar/core`) and the operator table
/// (installed `@almadar/std`), so a missing or incompatible install surfaces
/// as a structured error before any phase reads it.
pub(crate) fn require_language_data() -> anyhow::Result<()> {
    ensure_language_ready().map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))
}

/// Turns first-run setup off (CI, or a caller that wants the raw error).
pub(crate) const NO_AUTO_SETUP_ENV: &str = "ORB_NO_AUTO_SETUP";

/// The language preflight, setting the user store up on first run: when the
/// core and std this directory resolves are the user store's own (or nothing),
/// and the store has not been set up for this orb (no core, or not the core it
/// pins), run `orb update --global` once and check again. A project's own
/// `node_modules` is never touched — its failure is returned as is.
pub(crate) fn ensure_language_ready() -> Result<(), orbital_core::language_data::LanguageDataError>
{
    let first = match orbital_core::language_data::preflight() {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if std::env::var_os(NO_AUTO_SETUP_ENV).is_some() || !user_store_needs_setup() {
        return Err(first);
    }
    eprintln!(
        "orb: the language is not set up for this orb ({}: {first}) — running `orb update --global` (set {NO_AUTO_SETUP_ENV}=1 to skip)",
        first.code()
    );
    if let Err(e) = crate::commands::packages::run_update(true) {
        eprintln!("orb: update did not complete: {e}");
        return Err(first);
    }
    orbital_core::language_data::preflight()
}

/// True when core and std resolve from the user store (or not at all) and the
/// store's core is missing or not the one this orb pins.
fn user_store_needs_setup() -> bool {
    use orbital_core::behavior_packages::{find_npm_package, user_store_dir, STD_PACKAGE};
    use orbital_core::language_data::CORE_PACKAGE;
    let (Ok(cwd), Some(store)) = (std::env::current_dir(), user_store_dir()) else {
        return false;
    };
    let store = std::fs::canonicalize(&store).unwrap_or(store);
    let owned_by_store = |name: &str| {
        find_npm_package(name, &cwd, &store).is_none_or(|pkg| pkg.root.starts_with(&store))
    };
    if !owned_by_store(CORE_PACKAGE) || !owned_by_store(STD_PACKAGE) {
        return false;
    }
    let (_, pinned) = crate::baked_engine_pins::BAKED_CORE;
    find_npm_package(CORE_PACKAGE, &cwd, &store).is_none_or(|core| core.version != pinned)
}

/// Load the services registry. Returns both validation projections (action
/// names + per-action param contracts) parsed from ONE source, so they can
/// never drift: `--services <path>` when given, else the installed
/// `@almadar/core`'s registry.
pub(crate) fn load_services_registry(
    explicit_path: Option<&std::path::Path>,
) -> anyhow::Result<orbital_compiler::embedded::ServiceRegistryData> {
    require_language_data()?;
    let Some(path) = explicit_path else {
        let installed = orbital_compiler::embedded::baked_service_registry().clone();
        eprintln!(
            "Loaded {} services from the installed @almadar/core",
            installed.actions.len()
        );
        return Ok(installed);
    };
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("could not read services registry {}: {e}", path.display()))?;
    let data = orbital_compiler::embedded::parse_service_registry_data(&content)
        .map_err(|e| anyhow::anyhow!("invalid JSON in {}: {e}", path.display()))?;
    eprintln!(
        "Loaded {} services from {}",
        data.actions.len(),
        path.display()
    );
    Ok(data)
}

/// The project's `patterns.json` (the explicit path, else beside the schema or
/// in its `design-system/`), sent to the TypeScript shell as `projectPatterns`.
pub(crate) fn load_project_patterns(
    schema_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> Option<serde_json::Value> {
    let patterns_path = match explicit_path {
        Some(p) if p.exists() => p.to_path_buf(),
        Some(_) => return None,
        None => {
            let schema_dir = schema_path.parent()?;
            [schema_dir.join("patterns.json"), schema_dir.join("design-system/patterns.json")]
                .into_iter()
                .find(|p| p.exists())?
        }
    };
    let content = std::fs::read_to_string(&patterns_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let count = json.get("patterns").and_then(|v| v.as_object()).map_or(0, |p| p.len());
    if count == 0 {
        return None;
    }
    eprintln!("Loaded {} project component mappings from {}", count, patterns_path.display());
    Some(json)
}
