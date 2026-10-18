//! `orb stamp` — assign V4 identity (ids + ledger) to a name-only `.orb`.
//!
//! The factory path emits name-only schemas; `orb stamp` runs the single
//! `stamp_identity` minting authority (the same one the `.lolo` lowerer uses)
//! over that already-lowered `.orb` so ids + ledger are assigned in one place.
//! Unlike `orb resolve`, the schema shape is preserved — no Inline/Compose/
//! Enrich runs. `--prior` (the existing on-disk orbital) anchors id reuse so
//! re-stamps are byte-stable. `--imports <DIR>` additionally resolves
//! cross-file `uses` id edges (`Alias.traits.X` / `Alias.entity` /
//! `Alias.pages.X`) against the behavior packages installed from `<DIR>` —
//! see `orbital_compiler::resolve_cross_file_id_edges`.

use orbital_compiler::loader::{ExternalOrbitalLoader, LoaderConfig};
use orbital_core::behavior_packages::{discover, user_store_dir};
use std::collections::HashSet;
use std::path::Path;

pub(crate) fn stamp_schema_cli(
    path: &Path,
    prior_path: Option<&Path>,
    output: Option<&Path>,
    imports_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let content = crate::utils::translate_schema_content(&content);
    let mut schema = orbital_core::schema::parse_schema(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;

    let prior_schema =
        match prior_path {
            Some(pp) => {
                let pc = std::fs::read_to_string(pp)
                    .map_err(|e| anyhow::anyhow!("Failed to read prior {}: {}", pp.display(), e))?;
                let pc = crate::utils::translate_schema_content(&pc);
                Some(orbital_core::schema::parse_schema(&pc).map_err(|e| {
                    anyhow::anyhow!("Failed to parse prior {}: {}", pp.display(), e)
                })?)
            }
            None => None,
        };

    orbital_lolo::stamp_schema_identity(&mut schema, prior_schema.as_ref(), &HashSet::new());

    if let Some(dir) = imports_dir {
        let user_store = user_store_dir().unwrap_or_else(|| dir.to_path_buf());
        let packages = discover(dir, &user_store)
            .map_err(|e| anyhow::anyhow!("Failed to discover behavior packages: {}", e))?;
        let config = LoaderConfig {
            base_path: dir.to_path_buf(),
            packages,
            ..Default::default()
        };
        let mut loader = ExternalOrbitalLoader::new(config);
        orbital_compiler::resolve_cross_file_id_edges(&mut schema, &mut loader)
            .map_err(|e| anyhow::anyhow!("Failed to resolve cross-file id edges: {}", e))?;
    }

    // Serialize via `to_value` → strip nulls → canonical_json → pretty,
    // IDENTICAL to `orb emit orb` (emit/mod.rs). `preserve_order` is ON
    // workspace-wide (`orbital-compiler/Cargo.toml`), so `serde_json::Map` is
    // insertion-ordered, not a sorted BTreeMap — `canonical_json` is what
    // makes the output the canonical sorted-key form the registry `.orb`
    // files are stored in. Serializing the typed struct directly would use
    // declaration/HashMap order (churn + non-deterministic), breaking
    // byte-stability of a re-stamped registry file.
    let value = serde_json::to_value(&schema)
        .map_err(|e| anyhow::anyhow!("Failed to serialize stamped schema: {}", e))?;
    let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))
        .map_err(|e| anyhow::anyhow!("Failed to serialize stamped schema: {}", e))?;
    match output {
        Some(out) => std::fs::write(out, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?,
        None => println!("{}", json),
    }
    Ok(())
}
