//! Native resolution helpers shared by CLI programs and execution.

use std::path::{Path, PathBuf};

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
    let (mut resolved, mut lazy) =
        orbital_compiler::phases::lazy::resolve_program_with_path(without_proven(schema)?, &options, Some(path))
            .map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    orbital_compiler::phases::validation::effect::mark_proven_program(&mut resolved);
    mark_proven_lazy(&mut lazy);
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

/// `schema` with every `proven` mark removed, wherever it is written: a program is resolved from its own code, never
/// trusted for what an earlier resolve proved (G-CROSS-335).
fn without_proven(schema: orbital_core::OrbitalSchema) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let mut code: orbital_core::schema::SExpression = serde_json::from_value(serde_json::to_value(&schema)?)?;
    code.strip_proven();
    Ok(serde_json::from_value(serde_json::to_value(&code)?)?)
}

fn mark_proven_lazy(lazy: &mut [orbital_compiler::phases::lazy::ResolvedLazy]) {
    for item in lazy {
        orbital_compiler::phases::validation::effect::mark_proven_program(&mut item.schema);
        mark_proven_lazy(&mut item.lazy);
    }
}

/// [`resolve_to_temp`] for a command program: one std ships is resolved once per state of what it depends on and
/// cached under the derived-data directory; any other program resolves as usual.
pub(crate) fn resolve_command_to_temp(path: &Path) -> anyhow::Result<PathBuf> {
    match std_command_key(path)? {
        Some((stem, key)) => cached_resolve(
            &crate::commands::behaviors_index::derived_dir().join("programs"),
            &stem,
            &key,
            || resolve_to_temp(path),
        ),
        None => resolve_to_temp(path),
    }
}

/// The entry name of `program` resolved against the std tree at `roots`: one per tree, so trees never evict each other.
fn entry_stem(program: &Path, roots: &[PathBuf]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for root in roots {
        crate::commands::behaviors_index::feed(&mut hasher, "root", root.to_string_lossy().as_bytes());
    }
    let tree: String = hasher.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{}~{tree}", program_stem(program))
}

/// The cache entry name and key of a std command program, `None` for a program std does not ship. Std composes from no other
/// package, so what resolution reads is std's own tree, this binary, the language data and the resolve options.
fn std_command_key(path: &Path) -> anyhow::Result<Option<(String, String)>> {
    use crate::commands::behaviors_index::{feed, feed_toolchain};
    use sha2::{Digest, Sha256};
    let options = orbital_compiler::CompilerOptions::from_env();
    if options.discovery_error.is_some() {
        return Ok(None);
    }
    let packages = &options.inline.loader_config.packages;
    let Some(std) = packages.iter().find(|p| p.name == orbital_core::behavior_packages::STD_PACKAGE) else {
        return Ok(None);
    };
    let program = std::fs::canonicalize(path)?;
    let mut roots: Vec<PathBuf> = vec![std.registry.root().to_path_buf()];
    roots.extend(std.lolo_source.iter().cloned());
    roots.sort();
    roots.dedup();
    if !roots.iter().any(|root| program.starts_with(root)) {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    feed(&mut hasher, "resolve", b"1");
    feed_toolchain(&mut hasher)?;
    feed(&mut hasher, "package", format!("{}@{}:{}", std.name, std.version, std.prefix).as_bytes());
    feed(&mut hasher, "program", program.to_string_lossy().as_bytes());
    feed(&mut hasher, "nominalize", if options.nominalize { b"1" } else { b"0" });
    let app_root = options.inline.loader_config.scoped_paths.get(orbital_compiler::phases::APP_SCOPE);
    feed(&mut hasher, "app-root", app_root.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default().as_bytes());
    for root in &roots {
        feed(&mut hasher, &root.to_string_lossy(), tree_state(root)?.as_bytes());
    }
    let key = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Some((entry_stem(&program, &roots), key)))
}

/// The state of every file under `root`, by relative path: size, modification and change time, inode. A write,
/// rename or replacement changes it, as git's index tracks a working tree.
fn tree_state(root: &Path) -> anyhow::Result<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), out)?;
            } else {
                out.push(entry.path());
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, &mut files)?;
    files.sort();
    let mut state = String::new();
    for file in files {
        let meta = std::fs::symlink_metadata(&file)?;
        let rel = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
        state.push_str(&format!("{rel}\t{}\n", file_stamp(&meta)));
    }
    Ok(state)
}

#[cfg(unix)]
fn file_stamp(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{} {}.{} {}.{} {}", meta.size(), meta.mtime(), meta.mtime_nsec(), meta.ctime(), meta.ctime_nsec(), meta.ino())
}

#[cfg(windows)]
fn file_stamp(meta: &std::fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    format!("{} {} {}", meta.file_size(), meta.last_write_time(), meta.creation_time())
}

fn program_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The resolved program cached under `cache/<stem>-<key>`: a hit copies the entry to a fresh directory, a miss runs
/// `build` and stores its directory, replacing the stem's older entries. An entry appears whole or not at all.
fn cached_resolve(
    cache: &Path,
    stem: &str,
    key: &str,
    build: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let entry = cache.join(format!("{stem}-{key}"));
    if entry.join("program.orb").is_file() {
        let dir = fresh_dir();
        copy_tree(&entry, &dir)?;
        return Ok(dir.join("program.orb"));
    }
    let out = build()?;
    let built = out.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(cache)?;
    let staging = cache.join(format!("{stem}-{key}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(built, &staging)?;
    if std::fs::rename(&staging, &entry).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    for old in std::fs::read_dir(cache)?.flatten() {
        let name = old.file_name().to_string_lossy().into_owned();
        let stale = !name.contains('.')
            && name.rsplit_once('-').is_some_and(|(s, k)| s == stem && k != key);
        if stale {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    Ok(out)
}

/// One directory per call: programs resolved concurrently in one process never share files.
fn fresh_dir() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("orbital-cmd-{}-{n}", std::process::id()))
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn build_into(dir: &Path, calls: &Cell<u32>, text: &str) -> anyhow::Result<PathBuf> {
        calls.set(calls.get() + 1);
        std::fs::create_dir_all(dir.join("lazy/a"))?;
        std::fs::write(dir.join("program.orb"), text)?;
        std::fs::write(dir.join("program.en.json"), "{}")?;
        std::fs::write(dir.join("lazy/a/child.orb"), "{}")?;
        Ok(dir.join("program.orb"))
    }

    fn files(dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for entry in walk(dir) {
            let rel = entry.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            out.push((rel, std::fs::read_to_string(&entry).unwrap()));
        }
        out.sort();
        out
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
        }
        out
    }

    #[test]
    fn a_hit_answers_the_built_files_without_building_again() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let first = cached_resolve(cache.path(), "p", "k1", || build_into(&work.path().join("1"), &calls, "A")).unwrap();
        let second = cached_resolve(cache.path(), "p", "k1", || build_into(&work.path().join("2"), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "A");
        assert_ne!(first.parent(), second.parent(), "a hit answers its own directory");
        assert_eq!(files(first.parent().unwrap()), files(second.parent().unwrap()));
    }

    #[test]
    fn another_key_builds_again_and_replaces_the_stems_older_entry() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), "p", "k1", || build_into(&work.path().join("1"), &calls, "A")).unwrap();
        cached_resolve(cache.path(), "q", "k1", || build_into(&work.path().join("q"), &calls, "Q")).unwrap();
        let out = cached_resolve(cache.path(), "p", "k2", || build_into(&work.path().join("2"), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 3);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "B");
        let mut entries: Vec<String> = std::fs::read_dir(cache.path()).unwrap().flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        entries.sort();
        assert_eq!(entries, vec!["p-k2".to_string(), "q-k1".to_string()]);
    }

    #[test]
    fn a_partly_written_entry_is_never_read() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("p-k1.99.tmp")).unwrap();
        std::fs::write(cache.path().join("p-k1.99.tmp/program.orb"), "PARTIAL").unwrap();
        let calls = Cell::new(0);
        let out = cached_resolve(cache.path(), "p", "k1", || build_into(&work.path().join("1"), &calls, "A")).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read_to_string(out).unwrap(), "A");
    }

    #[test]
    fn a_failed_build_stores_nothing() {
        let cache = tempfile::tempdir().unwrap();
        let err = cached_resolve(cache.path(), "p", "k1", || anyhow::bail!("resolve failed"));
        assert!(err.is_err());
        assert_eq!(std::fs::read_dir(cache.path()).map(|d| d.count()).unwrap_or(0), 0);
    }

    #[test]
    fn the_tree_state_changes_with_any_file_and_only_then() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/locales/en")).unwrap();
        std::fs::write(root.path().join("a/x.lolo"), "one").unwrap();
        std::fs::write(root.path().join("a/locales/en/x.json"), "{}").unwrap();
        let base = tree_state(root.path()).unwrap();
        assert_eq!(tree_state(root.path()).unwrap(), base, "unchanged tree, same state");
        std::fs::write(root.path().join("a/locales/en/x.json"), "{\"k\":1}").unwrap();
        let edited = tree_state(root.path()).unwrap();
        assert_ne!(edited, base, "a non-.lolo dependency counts");
        std::fs::write(root.path().join("a/new.txt"), "").unwrap();
        assert_ne!(tree_state(root.path()).unwrap(), edited, "a new file counts");
    }

    #[test]
    fn entries_of_one_program_under_two_std_trees_do_not_evict_each_other() {
        let program = Path::new("/a/cli/atoms/std-orb-validate.lolo");
        let live = entry_stem(program, &[PathBuf::from("/live/std")]);
        let scratch = entry_stem(program, &[PathBuf::from("/scratch/std")]);
        assert_ne!(live, scratch);
        assert_eq!(live, entry_stem(program, &[PathBuf::from("/live/std")]));
        assert!(!live.contains('.'), "a staging name is the only one with a dot: {live}");
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), &live, "k1", || build_into(&work.path().join("1"), &calls, "L")).unwrap();
        cached_resolve(cache.path(), &scratch, "k2", || build_into(&work.path().join("2"), &calls, "S")).unwrap();
        let again = cached_resolve(cache.path(), &live, "k1", || build_into(&work.path().join("3"), &calls, "X")).unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_to_string(again).unwrap(), "L");
    }

    #[test]
    fn a_proven_mark_written_in_the_source_is_stripped_before_resolve() {
        let forged = |effect: serde_json::Value| {
            serde_json::json!({ "name": "App", "orbitals": [{ "name": "O",
                "entity": { "name": "E", "persistence": "runtime", "fields": [{ "name": "id", "type": "string", "required": true }] },
                "traits": [{
                "name": "T",
                "stateMachine": { "initial": "idle", "states": [{ "name": "idle" }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "GO", "effects": [effect] }] }
            }], "pages": [] }] })
        };
        let schema: orbital_core::OrbitalSchema =
            serde_json::from_value(forged(serde_json::json!(["set", "@entity.n", ["proven", [0], ["str/upper", "@payload.x"]]]))).unwrap();
        let expected: orbital_core::OrbitalSchema =
            serde_json::from_value(forged(serde_json::json!(["set", "@entity.n", ["str/upper", "@payload.x"]]))).unwrap();
        assert_eq!(
            serde_json::to_value(without_proven(schema).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn the_tree_state_sees_a_same_size_rewrite() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("x.lolo"), "aaaa").unwrap();
        let base = tree_state(root.path()).unwrap();
        let tmp = root.path().join("x.lolo.new");
        std::fs::write(&tmp, "bbbb").unwrap();
        std::fs::rename(&tmp, root.path().join("x.lolo")).unwrap();
        assert_ne!(tree_state(root.path()).unwrap(), base, "a replaced file has a new inode");
    }
}
