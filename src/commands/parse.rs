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
) -> anyhow::Result<(
    orbital_core::schema::OrbitalSchema,
    Vec<orbital_compiler::phases::lazy::ResolvedLazy>,
)> {
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
) -> anyhow::Result<(
    orbital_core::schema::OrbitalSchema,
    Vec<orbital_compiler::phases::lazy::ResolvedLazy>,
)> {
    resolved_from(parsed_program(path)?, path)
}

/// The resolved program at `path` and each `uses lazy` behavior split off it, as `{ program, lazy: [{ orbital, program }] }`,
/// keys in declaration order; nothing is written, so a program with lazy behaviors or locales is not refused.
pub(crate) fn resolved_value(path: &std::path::Path) -> anyhow::Result<orbital_core::Value> {
    use orbital_core::value_serialize::to_value;
    use orbital_core::{Value, ValueMap};
    let (resolved, lazy) = resolved_program(path)?;
    let lazy = lazy
        .iter()
        .map(|child| {
            let mut fields = ValueMap::default();
            fields.insert("orbital".to_string(), to_value(&child.orbital)?);
            fields.insert("program".to_string(), resolved_canonical(&child.schema)?);
            Ok(Value::object(fields))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut fields = ValueMap::default();
    fields.insert("program".to_string(), resolved_canonical(&resolved)?);
    fields.insert("lazy".to_string(), Value::array(lazy));
    Ok(Value::object(fields))
}

/// The resolved program as a key-sorted core value, built straight from the schema with no JSON tree between;
/// it holds what `resolved_tree` holds.
fn resolved_canonical(
    resolved: &orbital_core::schema::OrbitalSchema,
) -> anyhow::Result<orbital_core::Value> {
    let mut value = orbital_core::value_serialize::to_canonical_value(resolved)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;
    orbital_compiler::phases::await_render::attach_await_renders(resolved, &mut value);
    Ok(value)
}

/// The program at `path` as it reads before resolution, and every trait and orbital reference the resolution
/// of it resolves, each paired with the upstream declaration it resolved to (`program/read { references: true }`).
pub(crate) fn referenced_program(
    path: &std::path::Path,
) -> anyhow::Result<(
    serde_json::Value,
    Vec<orbital_compiler::phases::inline::ReferencePair>,
)> {
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
    Ok((
        orbital_core::util::canonical_json(&unresolved),
        session.take_references(),
    ))
}

pub(crate) fn resolved_text(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
) -> anyhow::Result<String> {
    resolved_read(path, output).map(|read| read.text)
}

/// What reading the program at `path` as resolved answers (`program/read { resolve }`).
pub(crate) struct ResolvedRead {
    pub text: String,
    /// The source carried the `resolved` stamp when it was loaded (EXEC-LOAD-1).
    pub loaded_resolved: bool,
    /// Where the resolved program lives on disk: the source when it was already resolved, `output` when it was
    /// resolved and written; absent when it was resolved in memory only.
    pub program_path: Option<std::path::PathBuf>,
}

/// `resolved_text`, with where the program lives and whether the program at `path` carried the `resolved` stamp
/// when it was loaded. A resolved source is answered as it was read and never written out again (EXEC-LOAD-1).
pub(crate) fn resolved_read(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
) -> anyhow::Result<ResolvedRead> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    if let Some(mark) = stamp_of(&content) {
        orbital_compiler::phases::check_resolved_mark(&mark, path.display().to_string())
            .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
        return Ok(ResolvedRead {
            text: content,
            loaded_resolved: true,
            program_path: Some(path.to_path_buf()),
        });
    }
    let schema = crate::utils::parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    if output.is_none() && schema.locales.as_ref().is_some_and(|l| !l.is_empty()) {
        anyhow::bail!("{}", locales_refusal(path));
    }
    drop(content);
    let (resolved, lazy) = resolved_from(schema, path)?;
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
            anyhow::bail!("{}", locales_refusal(path));
        }
        None => {}
    }
    Ok(ResolvedRead {
        text: json,
        loaded_resolved: false,
        program_path: output.map(std::path::Path::to_path_buf),
    })
}

/// The `resolved` stamp of an `.orb` text, read without building the program (a 98 MB resolved program is not
/// parsed twice); `None` for `.lolo` source and for an `.orb` that carries none.
fn stamp_of(content: &str) -> Option<orbital_core::schema::ResolvedMark> {
    #[derive(serde::Deserialize)]
    struct Stamp {
        resolved: Option<orbital_core::schema::ResolvedMark>,
    }
    match detect_syntax(content) {
        InputSyntax::OrbJson => serde_json::from_str::<Stamp>(content)
            .ok()
            .and_then(|s| s.resolved),
        InputSyntax::Lolo => None,
    }
}

fn locales_refusal(path: &std::path::Path) -> String {
    format!(
        "{} declares locales; each locale's catalog is written beside the resolved .orb — pass --output",
        path.display()
    )
}

fn resolved_json(resolved: &orbital_core::schema::OrbitalSchema) -> anyhow::Result<String> {
    serde_json::to_string_pretty(&resolved_tree(resolved)?)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))
}

/// The resolved program as a key-sorted JSON tree, built once and sorted in place.
fn resolved_tree(
    resolved: &orbital_core::schema::OrbitalSchema,
) -> anyhow::Result<serde_json::Value> {
    let mut tree = serde_json::to_value(resolved)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;
    // Awaiting-server skeleton predictions — resolve output only, never persisted.
    orbital_compiler::phases::await_render::attach_await_renders(resolved, &mut tree);
    // Byte-identical `orb resolve` (owner ruling, Stage C): `preserve_order` reflects insertion order, which
    // varies run to run for the same schema, so the keys are sorted before the tree is printed or handed on.
    Ok(orbital_core::util::into_canonical_json(tree))
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
