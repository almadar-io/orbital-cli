//! Loaders for project-supplied patterns, services registry, and component
//! mappings consumed by the validate/compile commands.

use crate::utils::strings::kebab_to_pascal;

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

/// Load services registry from services-registry.json.
/// Returns both validation projections (action names + per-action param
/// contracts) parsed from ONE source, so they can never drift.
pub(crate) fn load_services_registry(
    explicit_path: Option<&std::path::Path>,
) -> orbital_compiler::embedded::ServiceRegistryData {
    // Precedence: --services > live CWD registry (in-repo dev picks up a fresh
    // `almadar-sync services` with no rebuild) > baked copy in the binary
    // (itself overridable via ORBITAL_SERVICES_JSON / ORBITAL_BAKED_DIR).
    let registry_path = if let Some(p) = explicit_path {
        if p.exists() {
            Some(p.to_path_buf())
        } else {
            eprintln!(
                "Warning: services registry not found: {} — falling back to the baked registry",
                p.display()
            );
            None
        }
    } else {
        // Auto-discover: check well-known locations in the repo. The
        // registry moved when almadar-patterns was merged into
        // @almadar/core; the old path is kept as a fallback for older
        // trees that haven't picked up that merge yet.
        let candidates = [
            std::path::PathBuf::from("packages/almadar-core/src/patterns/services-registry.json"),
            std::path::PathBuf::from("packages/almadar-patterns/src/services-registry.json"),
        ];
        candidates.into_iter().find(|p| p.exists())
    };

    let Some(registry_path) = registry_path else {
        let baked = orbital_compiler::embedded::baked_service_registry().clone();
        eprintln!(
            "Loaded {} services from the baked registry",
            baked.actions.len()
        );
        return baked;
    };

    let content = match std::fs::read_to_string(&registry_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Warning: could not read {}: {} — falling back to the baked registry",
                registry_path.display(),
                e
            );
            return orbital_compiler::embedded::baked_service_registry().clone();
        }
    };

    let data = match orbital_compiler::embedded::parse_service_registry_data(&content) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "Warning: invalid JSON in {}: {} — falling back to the baked registry",
                registry_path.display(),
                e
            );
            return orbital_compiler::embedded::baked_service_registry().clone();
        }
    };

    if data.actions.is_empty() {
        eprintln!(
            "Warning: services registry at {} loaded but contained no services — falling back to the baked registry",
            registry_path.display()
        );
        return orbital_compiler::embedded::baked_service_registry().clone();
    }

    eprintln!(
        "Loaded {} services from {}",
        data.actions.len(),
        registry_path.display()
    );

    data
}

/// Load project-specific component mappings from patterns.json for code generation.
/// This returns a HashMap suitable for passing to TypeScriptBackend.extra_mappings.
pub(crate) fn load_project_component_mappings(
    schema_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> Option<std::collections::HashMap<String, orbital_shell_typescript::ExtraComponentMapping>> {
    use std::collections::HashMap;

    let patterns_path = if let Some(p) = explicit_path {
        if p.exists() {
            p.to_path_buf()
        } else {
            return None;
        }
    } else {
        // Auto-discover: same logic as load_extra_patterns
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

    let content = std::fs::read_to_string(&patterns_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let mut mappings = HashMap::new();

    if let Some(patterns) = json.get("patterns").and_then(|v| v.as_object()) {
        for (pattern_name, pattern_def) in patterns {
            // Get component name — either from explicit "component" field or derive from pattern type
            let component = pattern_def
                .get("component")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| kebab_to_pascal(pattern_name));

            let category = pattern_def
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("display")
                .to_string();

            mappings.insert(
                pattern_name.clone(),
                orbital_shell_typescript::ExtraComponentMapping {
                    component,
                    category,
                },
            );
        }
    }

    if mappings.is_empty() {
        return None;
    }

    eprintln!(
        "Loaded {} project component mappings from {}",
        mappings.len(),
        patterns_path.display()
    );

    Some(mappings)
}
