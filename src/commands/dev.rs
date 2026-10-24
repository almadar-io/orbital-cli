//! Native resolution helpers shared by CLI programs and execution.

use std::path::PathBuf;

/// Resolve a schema (Inline + Compose + Enrich) and write the resolved `.orb` to a temp
/// file, returning its path. Native hosts parse a path; the CLI owns resolution.
pub(crate) fn resolve_to_temp(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let schema = orbital_compiler::parse_file(path)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    resolve_schema_to_temp(schema, path)
}

/// [`resolve_to_temp`] for a program already parsed: `path` is where it was read from, so its relative imports resolve.
pub(crate) fn resolve_schema_to_temp(schema: orbital_core::OrbitalSchema, path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let options = orbital_compiler::CompilerOptions::from_env();
    let (resolved, lazy) =
        orbital_compiler::phases::lazy::resolve_program_with_path(schema, &options, Some(path))
            .map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    // One directory per call: programs resolved concurrently in one process (the runner, its tests) never share files.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orbital-dev-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let out = dir.join("program.orb");
    std::fs::write(&out, serde_json::to_string(&resolved)?)?;
    let program_dir = path.parent().unwrap_or(std::path::Path::new("."));
    orbital_compiler::phases::messages::write_catalogs(&out, &resolved, program_dir)?;
    crate::commands::parse::write_lazy(
        out.parent().unwrap_or(std::path::Path::new(".")),
        &lazy,
        program_dir,
    )?;
    Ok(out)
}
