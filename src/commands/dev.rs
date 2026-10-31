//! Native resolution helpers shared by CLI programs and execution.

use std::path::{Path, PathBuf};

/// A resolved program written to a directory this run owns; the directory is removed when this is dropped, on every
/// exit path. The name carries a random suffix, so no two runs, in one process or several, share a path.
pub(crate) struct ResolvedProgram {
    dir: tempfile::TempDir,
}

impl ResolvedProgram {
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join("program.orb")
    }
}

/// A fresh directory under `parent`, named `<prefix><random>` and removed when the guard drops.
pub(crate) fn scratch_dir_in(parent: &Path, prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .map_err(|e| anyhow::anyhow!("create a scratch directory under {}: {e}", parent.display()))
}

/// [`scratch_dir_in`] under the system temp directory.
pub(crate) fn scratch_dir(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    scratch_dir_in(&std::env::temp_dir(), prefix)
}

/// Resolve a schema (Inline + Compose + Enrich) into a run-owned directory. Native hosts boot from a path; the CLI
/// owns resolution.
pub(crate) fn resolve_to_temp(path: &Path) -> anyhow::Result<ResolvedProgram> {
    let schema = orbital_compiler::parse_file(path)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    resolve_schema_to_temp(schema, path)
}

/// [`resolve_to_temp`] for a program already parsed: `path` is where it was read from, so its relative imports resolve.
pub(crate) fn resolve_schema_to_temp(schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<ResolvedProgram> {
    resolve_schema_into(&std::env::temp_dir(), schema, path)
}

fn resolve_schema_into(parent: &Path, schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<ResolvedProgram> {
    let (resolved, lazy) = resolve_in_memory(schema, path)?;
    let program = ResolvedProgram { dir: scratch_dir_in(parent, "orbital-dev-")? };
    let out = program.path();
    std::fs::write(&out, serde_json::to_string(&resolved)?)?;
    let program_dir = path.parent().unwrap_or(Path::new("."));
    orbital_compiler::phases::messages::write_catalogs(&out, &resolved, program_dir)?;
    orbital_compiler::phases::lazy::write_resolved_lazy(program.dir.path(), &lazy, program_dir)?;
    Ok(program)
}

/// A program resolved once, with every file its resolution read.
pub(crate) struct ResolvedEntry {
    schema: orbital_core::OrbitalSchema,
    lazy: Vec<orbital_compiler::phases::lazy::ResolvedLazy>,
    reads: orbital_core::read_set::ReadSet,
    program: String,
}

type ResolvedKey = (PathBuf, String);

static RESOLVED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<ResolvedKey, std::sync::Arc<ResolvedEntry>>>> =
    std::sync::LazyLock::new(Default::default);

/// The program resolved once per path and compiler settings in this process, reused while the parsed program hashes the
/// same and every file the resolution read is as it was read; a changed program replaces its path's entry.
pub(crate) fn resolved_entry(schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<std::sync::Arc<ResolvedEntry>> {
    use sha2::{Digest, Sha256};
    let options = orbital_compiler::CompilerOptions::from_env();
    let value = serde_json::to_value(&schema).map_err(|e| anyhow::anyhow!("serialize {}: {e}", path.display()))?;
    let program = format!("{:x}", Sha256::digest(serde_json::to_string(&orbital_core::util::canonical_json(&value))?.as_bytes()));
    let mut hasher = Sha256::new();
    for (label, bytes) in [
        ("packages", format!("{:?}", options.inline.loader_config.packages).into_bytes()),
        ("scopes", format!("{:?}", options.inline.loader_config.scoped_paths).into_bytes()),
        ("nominalize", vec![u8::from(options.nominalize)]),
    ] {
        hasher.update(label.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let key = (path.to_path_buf(), hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut resolved = RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = resolved.get(&key).filter(|entry| entry.program == program && entry.reads.is_current()) {
        entry.reads.replay();
        return Ok(std::sync::Arc::clone(entry));
    }
    orbital_core::read_set::begin();
    let done = orbital_compiler::phases::lazy::resolve_program_with_path(schema, &options, Some(path));
    let reads = orbital_core::read_set::end();
    let (schema, lazy) = done.map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    let entry = std::sync::Arc::new(ResolvedEntry { schema, lazy, reads, program });
    resolved.insert(key, std::sync::Arc::clone(&entry));
    Ok(entry)
}

/// The resolved program and its lazy behaviors, never written: for a reader that takes the program as data.
pub(crate) fn resolve_in_memory(
    schema: orbital_core::OrbitalSchema,
    path: &Path,
) -> anyhow::Result<(orbital_core::OrbitalSchema, Vec<orbital_compiler::phases::lazy::ResolvedLazy>)> {
    let entry = resolved_entry(schema, path)?;
    Ok((entry.schema.clone(), entry.lazy.clone()))
}

/// [`resolve_to_temp`] for a command program: one std ships is resolved once per state of what it depends on and
/// cached under the derived-data directory; any other program resolves as usual.
pub(crate) fn resolve_command_to_temp(path: &Path) -> anyhow::Result<ResolvedProgram> {
    match std_command_key(path)? {
        Some((stem, key)) => cached_resolve(
            &derived_dir().join("programs"),
            &std::env::temp_dir(),
            &stem,
            &key,
            || resolve_to_temp(path),
        ),
        None => resolve_to_temp(path),
    }
}

/// The derived-data directory the command caches live under.
fn derived_dir() -> PathBuf {
    match std::env::var_os("ALMADAR_DERIVED_CACHE") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".almadar").join("derived"),
    }
}

/// A path as the bytes the file system holds, so two paths that differ only in non-UTF-8 bytes key differently.
fn path_bytes(path: &Path) -> &[u8] {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    }
    #[cfg(not(unix))]
    {
        path.as_os_str().as_encoded_bytes()
    }
}

/// One length-framed field of a cache key.
fn feed(hasher: &mut sha2::Sha256, label: &str, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// What a cached resolution depends on besides its sources: this binary and the installed language data.
fn feed_toolchain(hasher: &mut sha2::Sha256) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let meta = std::fs::metadata(&exe)?;
    let modified = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    feed(hasher, "exe", format!("{}:{modified}", meta.len()).as_bytes());
    let data = orbital_core::language_data::language_data().map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))?;
    for (label, text) in [
        ("patterns", &data.patterns),
        ("mapping", &data.component_mapping),
        ("events", &data.event_contracts),
        ("services", &data.services),
        ("binding", &data.binding_rules),
        ("types", &data.program_types),
        ("protocols", &data.protocols),
    ] {
        feed(hasher, label, text.as_bytes());
    }
    Ok(())
}

/// The entry name of `program` resolved against the std tree at `roots`: one per tree, so trees never evict each other.
fn entry_stem(program: &Path, roots: &[PathBuf]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for root in roots {
        feed(&mut hasher, "root", path_bytes(root));
    }
    let tree: String = hasher.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{}~{tree}", program_stem(program))
}

/// The cache entry name and key of a std command program, `None` for a program std does not ship. Std composes from no other
/// package, so what resolution reads is std's own tree, this binary, the language data and the resolve options.
fn std_command_key(path: &Path) -> anyhow::Result<Option<(String, String)>> {
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
    feed(&mut hasher, "program", path_bytes(&program));
    feed(&mut hasher, "nominalize", if options.nominalize { b"1" } else { b"0" });
    let app_root = options.inline.loader_config.scoped_paths.get(orbital_compiler::phases::APP_SCOPE);
    feed(&mut hasher, "app-root", app_root.map_or(&[][..], |p| path_bytes(p)));
    for root in &roots {
        feed(&mut hasher, "tree", path_bytes(root));
        feed(&mut hasher, "state", tree_state(root)?.as_bytes());
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
        let stamp = orbital_core::read_set::stamp(&file).ok_or_else(|| anyhow::anyhow!("{} vanished while its tree was read", file.display()))?;
        let rel = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
        state.push_str(&format!("{rel}\t{stamp}\n"));
    }
    Ok(state)
}

fn program_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The resolved program cached under `cache/<stem>-<key>`: a hit copies the entry to a fresh run-owned directory under
/// `parent`, a miss runs `build` and stores its directory, replacing the stem's older entries. An entry appears whole
/// or not at all.
fn cached_resolve(
    cache: &Path,
    parent: &Path,
    stem: &str,
    key: &str,
    build: impl FnOnce() -> anyhow::Result<ResolvedProgram>,
) -> anyhow::Result<ResolvedProgram> {
    let entry = cache.join(format!("{stem}-{key}"));
    if entry.join("program.orb").is_file() {
        let program = ResolvedProgram { dir: scratch_dir_in(parent, "orbital-cmd-")? };
        copy_tree(&entry, program.dir.path())?;
        return Ok(program);
    }
    let program = build()?;
    std::fs::create_dir_all(cache)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!("{stem}-{key}."))
        .suffix(".tmp")
        .tempdir_in(cache)?;
    copy_tree(program.dir.path(), staging.path())?;
    let staged = staging.keep();
    if std::fs::rename(&staged, &entry).is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    for old in std::fs::read_dir(cache)?.flatten() {
        let name = old.file_name().to_string_lossy().into_owned();
        let stale = !name.contains('.')
            && name.rsplit_once('-').is_some_and(|(s, k)| s == stem && k != key);
        if stale {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    Ok(program)
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

    fn built(parent: &Path, calls: &Cell<u32>, text: &str) -> anyhow::Result<ResolvedProgram> {
        calls.set(calls.get() + 1);
        let program = ResolvedProgram { dir: scratch_dir_in(parent, "orbital-dev-")? };
        let dir = program.dir.path();
        std::fs::create_dir_all(dir.join("lazy/a"))?;
        std::fs::write(dir.join("program.orb"), text)?;
        std::fs::write(dir.join("program.en.json"), "{}")?;
        std::fs::write(dir.join("lazy/a/child.orb"), "{}")?;
        Ok(program)
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
        let first = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "A")).unwrap();
        let second = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read_to_string(second.path()).unwrap(), "A");
        assert_ne!(first.path().parent(), second.path().parent(), "a hit answers its own directory");
        assert_eq!(files(first.dir.path()), files(second.dir.path()));
    }

    #[test]
    fn another_key_builds_again_and_replaces_the_stems_older_entry() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "A")).unwrap();
        cached_resolve(cache.path(), work.path(), "q", "k1", || built(work.path(), &calls, "Q")).unwrap();
        let out = cached_resolve(cache.path(), work.path(), "p", "k2", || built(work.path(), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 3);
        assert_eq!(std::fs::read_to_string(out.path()).unwrap(), "B");
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
        let out = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "A")).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read_to_string(out.path()).unwrap(), "A");
    }

    #[test]
    fn a_failed_build_stores_nothing() {
        let cache = tempfile::tempdir().unwrap();
        let err = cached_resolve(cache.path(), cache.path(), "p", "k1", || anyhow::bail!("resolve failed"));
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
        cached_resolve(cache.path(), work.path(), &live, "k1", || built(work.path(), &calls, "L")).unwrap();
        cached_resolve(cache.path(), work.path(), &scratch, "k2", || built(work.path(), &calls, "S")).unwrap();
        let again = cached_resolve(cache.path(), work.path(), &live, "k1", || built(work.path(), &calls, "X")).unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_to_string(again.path()).unwrap(), "L");
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

    fn entries(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> =
            std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        out.sort();
        out
    }

    fn program_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(format!("{name}.orb"));
        std::fs::write(&path, format!(r#"{{"name":"{name}","version":"1.0.0","orbitals":[]}}"#)).unwrap();
        path
    }

    const POST: &str = r#"{"name":"Post","version":"1.0.0","orbitals":[{"name":"Post","entity":{"name":"PostState","persistence":"runtime","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]}]}"#;
    const SITE: &str = r#"{"name":"site","version":"1.0.0","orbitals":[{"name":"Home","uses":[{"from":"./post.orb","as":"Why"}],"entity":{"name":"HomeState","persistence":"runtime","fields":[{"name":"id","type":"string","required":true}]},"traits":[],"pages":[]}]}"#;

    fn resolve_file(path: &Path) -> std::sync::Arc<ResolvedEntry> {
        resolved_entry(orbital_compiler::parse_file(path).unwrap(), path).unwrap()
    }

    #[test]
    fn a_second_resolve_of_an_unchanged_program_reuses_the_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let path = dir.path().join("site.orb");
        std::fs::write(&path, SITE).unwrap();
        let first = resolve_file(&path);
        assert!(std::sync::Arc::ptr_eq(&first, &resolve_file(&path)));
        assert!(first.reads.paths().any(|f| f.ends_with("post.orb")));
    }

    #[test]
    fn a_changed_direct_import_or_program_resolves_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let path = dir.path().join("site.orb");
        std::fs::write(&path, SITE).unwrap();
        let first = resolve_file(&path);
        std::fs::write(dir.path().join("post.orb"), POST.replace("PostState", "PostStateRenamed")).unwrap();
        let after_import = resolve_file(&path);
        assert!(!std::sync::Arc::ptr_eq(&first, &after_import));
        std::fs::write(&path, SITE.replace("HomeState", "HomeStateRenamed")).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&after_import, &resolve_file(&path)));
    }

    #[test]
    fn a_program_edited_many_times_keeps_one_entry_for_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let path = dir.path().join("site.orb");
        let held = |path: &Path| RESOLVED.lock().unwrap().keys().filter(|(p, _)| p == path).count();
        for n in 0..4 {
            std::fs::write(&path, SITE.replace("HomeState", &format!("HomeState{n}"))).unwrap();
            resolve_file(&path);
            assert_eq!(held(&path), 1, "edit {n}");
        }
    }

    #[test]
    fn a_changed_import_two_levels_down_resolves_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let mid = SITE.replace("\"Home\"", "\"Mid\"").replace("HomeState", "MidState").replace("\"name\":\"site\"", "\"name\":\"mid\"");
        std::fs::write(dir.path().join("mid.orb"), mid).unwrap();
        let path = dir.path().join("site.orb");
        std::fs::write(&path, SITE.replace("./post.orb", "./mid.orb")).unwrap();
        let first = resolve_file(&path);
        assert!(first.reads.paths().any(|f| f.ends_with("post.orb")), "the nested import is in the read set");
        assert!(std::sync::Arc::ptr_eq(&first, &resolve_file(&path)), "control: nothing changed");
        std::fs::write(dir.path().join("post.orb"), POST.replace("PostState", "PostStateRenamed")).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&first, &resolve_file(&path)));
    }

    #[test]
    fn a_resolved_program_exists_while_held_and_leaves_nothing_once_dropped() {
        let src = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = program_file(src.path(), "solo");
        let schema = orbital_compiler::parse_file(&path).unwrap();
        let resolved = resolve_schema_into(tmp.path(), schema, &path).unwrap();
        assert!(resolved.path().is_file(), "control: the program is on disk while the run holds it");
        let held = entries(tmp.path());
        assert_eq!(held.len(), 1);
        assert!(held[0].starts_with("orbital-dev-"), "{held:?}");
        drop(resolved);
        assert!(entries(tmp.path()).is_empty(), "nothing left behind: {:?}", entries(tmp.path()));
    }

    #[test]
    fn a_cached_hit_and_a_cached_miss_both_leave_nothing_once_dropped() {
        let cache = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let miss = cached_resolve(cache.path(), tmp.path(), "p", "k1", || built(tmp.path(), &calls, "A")).unwrap();
        let hit = cached_resolve(cache.path(), tmp.path(), "p", "k1", || built(tmp.path(), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 1);
        assert!(hit.dir.path().file_name().unwrap().to_string_lossy().starts_with("orbital-cmd-"));
        assert_eq!(entries(tmp.path()).len(), 2, "control: both runs hold their directory");
        drop((miss, hit));
        assert!(entries(tmp.path()).is_empty(), "{:?}", entries(tmp.path()));
        assert_eq!(entries(cache.path()), vec!["p-k1".to_string()], "the cache entry stays");
    }

    #[test]
    fn a_failed_resolve_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let path = program_file(src.path(), "ok");
        let schema = orbital_compiler::parse_file(&path).unwrap();
        let missing = tmp.path().join("no-such-parent");
        assert!(resolve_schema_into(&missing, schema, &path).is_err());
        assert!(entries(tmp.path()).is_empty());
    }

    #[test]
    fn parallel_resolves_each_answer_their_own_program_in_their_own_directory() {
        let src = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let name = format!("prog{i}");
                let path = program_file(src.path(), &name);
                let parent = tmp.path().to_path_buf();
                std::thread::spawn(move || {
                    let schema = orbital_compiler::parse_file(&path).unwrap();
                    let resolved = resolve_schema_into(&parent, schema, &path).unwrap();
                    let text = std::fs::read_to_string(resolved.path()).unwrap();
                    (name, text, resolved)
                })
            })
            .collect();
        let done: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut dirs = std::collections::BTreeSet::new();
        for (name, text, resolved) in &done {
            assert!(text.contains(&format!("\"name\":\"{name}\"")), "{name} read {text}");
            assert!(dirs.insert(resolved.path()), "two runs share {}", resolved.path().display());
        }
        assert_eq!(entries(tmp.path()).len(), 16);
        drop(done);
        assert!(entries(tmp.path()).is_empty());
    }
}
