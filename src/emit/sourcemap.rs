//! Source Map v3 emission for `orb emit --map`.
//!
//! File-level granularity: each generated `.ts`/`.py` file gets a sidecar
//! `.map` whose mappings point the whole generated file at the byte range
//! of its originating node (entity / trait / page / service) in the
//! source `.orb`.
//!
//! Compared to finer-grained (per-line or per-expression) maps this is a
//! coarse mapping — but it's enough for IDEs and browsers to jump from
//! generated code back to the schema with one click, which is the main
//! use case Part II §13 calls out.
//!
//! Finer granularity (Phase 5 of the design doc) requires threading
//! `Span` through every generator call site in `orbital-shell-*` — a
//! separate cross-crate refactor that isn't this PR's scope.

use std::path::{Path, PathBuf};

use orbital_compiler::generate::GeneratedFile;
use orbital_core::str_utils::kebab_to_pascal;

use super::spans::{byte_to_line_col, NodeKind};
use super::EmitTarget;

/// Classify a generated file path into a (kind, name) pair so we can look
/// up its span in `OrbSpans`. Returns `None` when the file is shared
/// scaffolding (barrel index, generic types) that doesn't map to a single
/// originating node.
pub fn classify_file(file: &GeneratedFile, target: EmitTarget) -> Option<(NodeKind, String)> {
    let path = file.path.as_str();
    match target {
        EmitTarget::Ts => classify_ts(path, &file.content),
        EmitTarget::Py => classify_py(path, &file.content),
        EmitTarget::Orb | EmitTarget::Lolo => None, // these targets emit no code files.
    }
}

/// TS-side path → (kind, name) mapping. Follows the TypeScript backend's
/// emitted path shape documented in `Almadar_Compiler_Emit_Code.md` §21.
fn classify_ts(path: &str, content: &str) -> Option<(NodeKind, String)> {
    // Trait hooks: .../hooks/traits/useFooLogic.tsx  or  useFoo.tsx
    if let Some(name) = strip_between(path, "/hooks/traits/use", "Logic.tsx")
        .or_else(|| strip_between(path, "/hooks/traits/use", "Logic.ts"))
    {
        return Some((NodeKind::Trait, name));
    }
    if let Some(name) = strip_between(path, "/hooks/traits/use", ".tsx")
        .or_else(|| strip_between(path, "/hooks/traits/use", ".ts"))
    {
        // Exclude `useOrbitalBridge` and barrel index — those aren't per-trait.
        if !name.starts_with("Orbital") && name != "index" {
            return Some((NodeKind::Trait, name));
        }
    }
    // Trait components: .../components/traits/FooView.tsx
    if let Some(name) = strip_between(path, "/components/traits/", "View.tsx")
        .or_else(|| strip_between(path, "/components/traits/", "View.ts"))
    {
        return Some((NodeKind::Trait, name));
    }
    // Trait server routes: packages/server/src/routes/foo.ts
    if let Some(stem) = path
        .strip_prefix("packages/server/src/routes/")
        .and_then(|p| p.strip_suffix(".ts"))
    {
        if stem != "index" && !stem.is_empty() {
            return Some((NodeKind::Trait, kebab_to_pascal(stem)));
        }
    }
    // Page features: packages/client/src/features/<name>/...
    if let Some(rest) = path.strip_prefix("packages/client/src/features/") {
        if let Some((name, _)) = rest.split_once('/') {
            return Some((NodeKind::Page, format!("/{}", feature_to_path(name))));
        }
    }
    // Entity files: packages/shared/src/types/entities.ts holds multiple
    // entities. For file-level maps we pick the FIRST entity name found in
    // the content, which is better than nothing — fine-grained per-entity
    // mapping lives in Phase 5.
    if path.starts_with("packages/shared/") {
        if let Some(name) = first_ts_entity_name(content) {
            return Some((NodeKind::Entity, name));
        }
    }
    None
}

/// Python-side path → (kind, name). Simpler layout than TS.
fn classify_py(path: &str, content: &str) -> Option<(NodeKind, String)> {
    if let Some(stem) = path
        .strip_prefix("traits/")
        .and_then(|p| p.strip_suffix(".py"))
    {
        if stem != "__init__" && !stem.is_empty() {
            return Some((NodeKind::Trait, snake_to_pascal(stem)));
        }
    }
    if let Some(stem) = path
        .strip_prefix("routes/")
        .and_then(|p| p.strip_suffix(".py"))
    {
        if stem != "__init__" && !stem.is_empty() {
            return Some((NodeKind::Trait, snake_to_pascal(stem)));
        }
    }
    if path == "entities/__init__.py" {
        if let Some(name) = first_py_entity_name(content) {
            return Some((NodeKind::Entity, name));
        }
    }
    None
}

/// Build a Source Map v3 for a single generated file, pointing the entire
/// generated content at `orb_range` of the `.orb` source.
pub fn build_map_for_file(
    generated_content: &str,
    orb_source_content: &str,
    orb_source_path: &Path,
    orb_range: std::ops::Range<usize>,
) -> String {
    let (src_line, src_col) = byte_to_line_col(orb_source_content, orb_range.start);

    // We emit one mapping per line of generated code, all pointing at the
    // same `.orb` origin byte. That gives VSCode / browsers a hit on every
    // line (otherwise the IDE says "no source" for lines without mappings).
    let gen_line_count = generated_content.lines().count().max(1);
    let mut sm = sourcemap::SourceMapBuilder::new(None);
    let src_id = sm.add_source(orb_source_path.to_str().unwrap_or("schema.orb"));
    sm.set_source_contents(src_id, Some(orb_source_content));
    for gen_line in 0..gen_line_count as u32 {
        sm.add_raw(gen_line, 0, src_line, src_col, Some(src_id), None, false);
    }
    let mut buf: Vec<u8> = Vec::new();
    sm.into_sourcemap()
        .to_writer(&mut buf)
        .expect("sourcemap serialization failed");
    String::from_utf8(buf).expect("sourcemap should be UTF-8")
}

/// Resolve where the `.map` for a given output file should live.
/// For `/tmp/out.ts` → `/tmp/out.ts.map`. Called by `emit::write_output`
/// when `--map` is provided.
pub fn map_path_for_output(output: &Path) -> PathBuf {
    let mut buf = output.as_os_str().to_owned();
    buf.push(".map");
    PathBuf::from(buf)
}

// --- Utilities -------------------------------------------------------------

fn strip_between(s: &str, prefix: &str, suffix: &str) -> Option<String> {
    let rest = s.split(prefix).nth(1)?;
    let name = rest.strip_suffix(suffix)?;
    // Reject empty or identifier-unfriendly matches.
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name.to_string())
}

fn snake_to_pascal(s: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn feature_to_path(name: &str) -> String {
    // Features are generated with kebab or snake case; pages in .orb are
    // keyed by URL path. The inverse is ambiguous — e.g. `foo_bar_baz` could
    // have been `/foo/bar/baz` or `/foo-bar-baz`. For now we just swap
    // underscores for slashes as the common case, and callers should fall
    // back to searching all page paths if this guess misses.
    name.replace('_', "/")
}

fn first_ts_entity_name(content: &str) -> Option<String> {
    // Find the first `export interface <Ident>` in the file.
    let needle = "export interface ";
    let idx = content.find(needle)?;
    let rest = &content[idx + needle.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

fn first_py_entity_name(content: &str) -> Option<String> {
    // `class <Ident>(BaseModel):` is the Pydantic idiom the Python backend uses.
    let needle = "class ";
    let mut pos = 0usize;
    while let Some(idx) = content[pos..].find(needle) {
        let abs = pos + idx;
        let rest = &content[abs + needle.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if end > 0 {
            let candidate = &rest[..end];
            // Skip Enum/State types — those aren't the entity class.
            if !candidate.ends_with("State")
                && !candidate.ends_with("Event")
                && content[abs..].contains("BaseModel")
            {
                return Some(candidate.to_string());
            }
        }
        pos = abs + needle.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_classifier_catches_trait_hook() {
        let f = GeneratedFile::new("packages/client/src/hooks/traits/useFooLogic.tsx", "");
        assert_eq!(
            classify_ts(&f.path, &f.content),
            Some((NodeKind::Trait, "Foo".into()))
        );
    }

    #[test]
    fn ts_classifier_catches_server_route() {
        let f = GeneratedFile::new("packages/server/src/routes/modal-record-modal.ts", "");
        assert_eq!(
            classify_ts(&f.path, &f.content),
            Some((NodeKind::Trait, "ModalRecordModal".into()))
        );
    }

    #[test]
    fn py_classifier_catches_trait() {
        let f = GeneratedFile::new("traits/modal_record_modal.py", "");
        assert_eq!(
            classify_py(&f.path, &f.content),
            Some((NodeKind::Trait, "ModalRecordModal".into()))
        );
    }

    #[test]
    fn ts_classifier_entity_fallback() {
        let f = GeneratedFile::new(
            "packages/shared/src/types/entities.ts",
            "export interface ModalRecord { id: string; }\n",
        );
        assert_eq!(
            classify_ts(&f.path, &f.content),
            Some((NodeKind::Entity, "ModalRecord".into()))
        );
    }
}
