//! The `program/read` and `program/print` resolve and format texts.

use crate::utils::{detect_syntax, InputSyntax};

/// The program at `path`, parsed: `.lolo` source, an unresolved `.orb`, or a resolved `.orb` (it carries `resolved`).
fn parsed_program(path: &std::path::Path) -> anyhow::Result<orbital_core::schema::OrbitalSchema> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    crate::utils::parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))
}

/// `schema` (parsed from `path`) resolved as `orb resolve` resolves it, with the lazy behaviors it splits off;
/// a program already carrying `resolved` is returned as it is.
fn resolved_from(
    schema: orbital_core::schema::OrbitalSchema,
    path: &std::path::Path,
) -> anyhow::Result<(orbital_core::schema::OrbitalSchema, Vec<orbital_compiler::phases::lazy::ResolvedLazy>)> {
    orbital_compiler::phases::lazy::resolve_program_with_path(
        schema,
        &orbital_compiler::CompilerOptions::from_env(),
        Some(path),
    )
    .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))
}

/// The program at `path` resolved as `orb resolve` resolves it, with the lazy behaviors it splits off.
pub(crate) fn resolved_program(
    path: &std::path::Path,
) -> anyhow::Result<(orbital_core::schema::OrbitalSchema, Vec<orbital_compiler::phases::lazy::ResolvedLazy>)> {
    resolved_from(parsed_program(path)?, path)
}

/// The resolved program at `path` and each `uses lazy` behavior split off it, as `{ program, lazy: [{ orbital, program }] }`,
/// keys in declaration order; nothing is written, so a program with lazy behaviors or locales is not refused.
pub(crate) fn resolved_value(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let (resolved, lazy) = resolved_program(path)?;
    let value = |schema: &orbital_core::schema::OrbitalSchema| -> anyhow::Result<serde_json::Value> {
        serde_json::from_str(&resolved_json(schema)?).map_err(|e| anyhow::anyhow!("{e}"))
    };
    let lazy = lazy
        .iter()
        .map(|child| Ok(serde_json::json!({ "orbital": child.orbital, "program": value(&child.schema)? })))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(serde_json::json!({ "program": value(&resolved)?, "lazy": lazy }))
}

/// The program at `path` as it reads before resolution, and every trait and orbital reference the resolution
/// of it resolves, each paired with the upstream declaration it resolved to (`program/read { references: true }`).
pub(crate) fn referenced_program(
    path: &std::path::Path,
) -> anyhow::Result<(serde_json::Value, Vec<orbital_compiler::phases::inline::ReferencePair>)> {
    use crate::utils::parse_schema_auto;
    use orbital_compiler::{resolve_schema_with_path, CompilerOptions};

    let mut options = CompilerOptions::from_env();
    let session = orbital_compiler::phases::inline::InlineSession::default();
    session.collect_references();
    options.inline.session = Some(session.clone());

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let mut schema = parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let unresolved = serde_json::to_value(&schema)
        .map_err(|e| anyhow::anyhow!("Failed to serialize {}: {}", path.display(), e))?;
    orbital_compiler::phases::lazy::split_lazy_and_resolve(&mut schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    session.take_references();
    resolve_schema_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    Ok((orbital_core::util::canonical_json(&unresolved), session.take_references()))
}

pub(crate) fn resolved_text(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
) -> anyhow::Result<String> {
    let schema = parsed_program(path)?;
    let input_resolved = schema.resolved.is_some();
    let (resolved, lazy) = resolved_from(schema, path)?;
    let json = resolved_json(&resolved)?;

    match output {
        // A resolved input's catalogs and lazy behaviors were written beside it when it was resolved; only the program text is written.
        Some(out) if input_resolved => {
            std::fs::write(out, &json)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?;
            tracing::info!(target: orbital_core::logging::ns::CLI, path = %out.display(), "wrote resolved schema (input already resolved; catalogs and lazy behaviors not rewritten)");
        }
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
