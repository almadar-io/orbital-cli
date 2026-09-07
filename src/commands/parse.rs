//! `orb parse`, `orb resolve`, and `orb format`.

use crate::utils::{detect_syntax, parse_schema_auto, InputSyntax};
use std::path::PathBuf;

/// Parse schema and display information
pub(crate) fn parse_schema(path: &PathBuf) -> anyhow::Result<()> {
    println!("Parsing schema: {}", path.display());

    let content_raw = std::fs::read_to_string(path)?;
    let schema = parse_schema_auto(&content_raw, &path.display().to_string())?;

    println!("\nSchema: {}", schema.name);
    if let Some(version) = &schema.version {
        println!("Version: {}", version);
    }
    if let Some(description) = &schema.description {
        println!("Description: {}", description);
    }

    println!("\nOrbitals ({}):", schema.orbitals.len());
    for orbital in &schema.orbitals {
        // Handle entity being either inline or reference
        let (entity_name, field_count) = match &orbital.entity {
            orbital_core::EntityRef::Inline(entity) => (entity.name.as_str(), entity.fields.len()),
            orbital_core::EntityRef::Reference(ref_str) => (ref_str.as_str(), 0),
            // Phase 3.5.G.A.5: `Extends` is rewritten to `Inline` before
            // this CLI command runs. Fall through with the upstream
            // `extends` reference and an unknown field count, mirroring
            // how the `Reference` arm reports a bare reference.
            orbital_core::EntityRef::Extends(extends) => (extends.reference.as_str(), 0),
        };
        println!("  - {} (entity: {})", orbital.name, entity_name);
        println!("    Fields: {}", field_count);
        println!("    Traits: {}", orbital.traits.len());
        if let Some(pages) = &orbital.pages {
            println!("    Pages: {}", pages.len());
        }
    }

    // Note: Traits are now only defined inline within orbitals, not at schema level.

    Ok(())
}

/// Resolve a schema: run Inline + Compose + Enrich, then serialize the
/// resulting `OrbitalSchema` to JSON. Emits to `output` (if given) or
/// stdout. Produces the same artifact `orbital compile` writes as
/// `resolved.orb` alongside generated code — useful for tooling that
/// wants the bundled schema view without running full codegen.
pub(crate) fn resolve_schema_cli(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
    verbose: bool,
) -> anyhow::Result<()> {
    use crate::utils::parse_schema_auto;
    use orbital_compiler::{resolve_schema_with_path, CompilerOptions};

    let options = CompilerOptions {
        verbose,
        ..CompilerOptions::from_env()
    };

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let schema = parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let resolved = resolve_schema_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    let resolved_value = serde_json::to_value(&resolved)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;
    // Byte-identical `orb resolve` (owner ruling, Stage C): `preserve_order`
    // means `to_string_pretty` alone reflects HashMap insertion order, which
    // varies run to run for the same schema. Canonicalize before printing.
    let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&resolved_value))
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;

    match output {
        Some(out) => {
            std::fs::write(out, &json)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?;
            if verbose {
                eprintln!("Wrote resolved schema to {}", out.display());
            }
        }
        None => {
            println!("{}", json);
        }
    }
    Ok(())
}

pub(crate) fn format_schema(path: &PathBuf, write: bool) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Could not read {}: {}", path.display(), e))?;

    let formatted = match detect_syntax(&content) {
        InputSyntax::Lolo => {
            let program = orbital_lolo::parse_str(&content)
                .map_err(|e| anyhow::anyhow!("Parse error in {}: {:?}", path.display(), e))?;
            orbital_lolo::format_program(&program)
        }
        InputSyntax::OrbJson => {
            let value: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Invalid JSON in {}: {}", path.display(), e))?;
            serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))?
        }
    };

    if write {
        std::fs::write(path, &formatted)?;
        println!("Formatted {} ({} bytes)", path.display(), formatted.len());
    } else {
        println!("{}", formatted);
    }

    Ok(())
}
