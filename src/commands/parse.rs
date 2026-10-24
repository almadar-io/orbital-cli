//! The `program/read` and `program/print` resolve and format texts.

use crate::utils::{detect_syntax, InputSyntax};

/// What `orb resolve` prints for `path`; with `output`, also written there with its catalogs and
/// lazy behaviors. Without `output`, a program with lazy behaviors or locales is refused.
pub(crate) fn resolved_text(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
) -> anyhow::Result<String> {
    use crate::utils::parse_schema_auto;
    use orbital_compiler::{resolve_schema_with_path, CompilerOptions};

    let options = CompilerOptions::from_env();

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let schema = parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let mut schema = schema;
    let mut lazy =
        orbital_compiler::phases::lazy::split_lazy_and_resolve(&mut schema, &options, Some(path))
            .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    let mut resolved = resolve_schema_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    orbital_compiler::phases::reflect::fold_program(&mut resolved, &mut lazy, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    let json = resolved_json(&resolved)?;

    match output {
        Some(out) => {
            std::fs::write(out, &json)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?;
            tracing::info!(target: orbital_core::logging::ns::CLI, path = %out.display(), "wrote resolved schema");
            let program_dir = path.parent().unwrap_or(std::path::Path::new("."));
            orbital_compiler::phases::messages::write_catalogs(out, &resolved, program_dir)
                .map_err(|e| {
                    anyhow::anyhow!("Failed to write catalogs beside {}: {}", out.display(), e)
                })?;
            write_lazy(
                out.parent().unwrap_or(std::path::Path::new(".")),
                &lazy,
                program_dir,
            )?;
        }
        None if !lazy.is_empty() => {
            anyhow::bail!(
                "{} imports {} behavior(s) with `uses lazy`; each resolves to its own .orb — pass --output so they can be written beside it",
                path.display(),
                lazy.len()
            );
        }
        None if resolved.locales.as_ref().is_some_and(|l| !l.is_empty()) => {
            anyhow::bail!(
                "{} declares locales; each locale's catalog is written beside the resolved .orb — pass --output",
                path.display()
            );
        }
        None => {}
    }
    Ok(json)
}

fn resolved_json(resolved: &orbital_core::schema::OrbitalSchema) -> anyhow::Result<String> {
    let mut resolved_value = serde_json::to_value(resolved)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;
    // Awaiting-server skeleton predictions — resolve output only, never persisted.
    orbital_compiler::phases::await_render::attach_await_renders(resolved, &mut resolved_value);
    // Byte-identical `orb resolve` (owner ruling, Stage C): `preserve_order`
    // means `to_string_pretty` alone reflects HashMap insertion order, which
    // varies run to run for the same schema. Canonicalize before printing.
    serde_json::to_string_pretty(&orbital_core::util::canonical_json(&resolved_value))
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))
}

pub(crate) fn write_lazy(
    dir: &std::path::Path,
    lazy: &[orbital_compiler::phases::lazy::ResolvedLazy],
    program_dir: &std::path::Path,
) -> anyhow::Result<()> {
    orbital_compiler::phases::lazy::write_lazy(dir, lazy, program_dir, &|schema| {
        resolved_json(schema).map_err(|e| std::io::Error::other(e.to_string()))
    })
    .map_err(|e| {
        anyhow::anyhow!(
            "Failed to write lazy behaviors under {}: {}",
            dir.display(),
            e
        )
    })?;
    for item in lazy {
        tracing::info!(
            target: orbital_core::logging::ns::CLI,
            orbital = %item.orbital,
            path = %dir.join(&item.orb_ref).display(),
            "wrote lazy behavior"
        );
    }
    Ok(())
}

/// `content` pretty-printed in the syntax it is written in: `.lolo` through the authored-AST
/// formatter (comments kept), `.orb` JSON with sorted keys. `label` names it in errors.
pub(crate) fn formatted_text(content: &str, label: &str) -> anyhow::Result<String> {
    Ok(match detect_syntax(content) {
        InputSyntax::Lolo => {
            let program = orbital_lolo::parse_str(content)
                .map_err(|e| anyhow::anyhow!("Parse error in {}: {:?}", label, e))?;
            orbital_lolo::format_program(&program)
        }
        InputSyntax::OrbJson => {
            let value: serde_json::Value = serde_json::from_str(content)
                .map_err(|e| anyhow::anyhow!("Invalid JSON in {}: {}", label, e))?;
            serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))?
        }
    })
}
