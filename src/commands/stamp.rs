//! `orb stamp` — assign V4 identity (ids + ledger) to a name-only `.orb`.
//!
//! The factory path emits name-only schemas; `orb stamp` runs the single
//! `stamp_identity` minting authority (the same one the `.lolo` lowerer uses)
//! over that already-lowered `.orb` so ids + ledger are assigned in one place.
//! Unlike `orb resolve`, the schema shape is preserved — no Inline/Compose/
//! Enrich runs. `--prior` (the existing on-disk orbital) anchors id reuse so
//! re-stamps are byte-stable.

use std::collections::HashSet;
use std::path::Path;

pub(crate) fn stamp_schema_cli(
    path: &Path,
    prior_path: Option<&Path>,
    output: Option<&Path>,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let mut schema = orbital_core::schema::parse_schema(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;

    let prior_schema = match prior_path {
        Some(pp) => {
            let pc = std::fs::read_to_string(pp)
                .map_err(|e| anyhow::anyhow!("Failed to read prior {}: {}", pp.display(), e))?;
            Some(
                orbital_core::schema::parse_schema(&pc)
                    .map_err(|e| anyhow::anyhow!("Failed to parse prior {}: {}", pp.display(), e))?,
            )
        }
        None => None,
    };

    orbital_lolo::stamp_schema_identity(&mut schema, prior_schema.as_ref(), &HashSet::new());

    let json = serde_json::to_string_pretty(&schema)
        .map_err(|e| anyhow::anyhow!("Failed to serialize stamped schema: {}", e))?;
    match output {
        Some(out) => std::fs::write(out, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?,
        None => println!("{}", json),
    }
    Ok(())
}
