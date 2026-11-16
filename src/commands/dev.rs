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

/// A resolution written to disk is kept there (a command's derived cache, the caller's own files), never also held in
/// this process's resolve cache for the rest of the run.
fn resolve_schema_into(parent: &Path, schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<ResolvedProgram> {
    let options = orbital_compiler::CompilerOptions::from_env();
    let (resolved, lazy) = orbital_compiler::phases::lazy::resolve_program_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    let (resolved, lazy) = (&resolved, &lazy);
    let program = ResolvedProgram { dir: scratch_dir_in(parent, "orbital-dev-")? };
    let out = program.path();
    std::fs::write(&out, serde_json::to_string(resolved)?)?;
    orbital_compiler::phases::lazy::write_resolved_lazy(program.dir.path(), lazy)?;
    Ok(program)
}

use orbital_compiler::phases::lazy::{LazyResolveCache, ResolvedLazy};

/// One `uses lazy` behavior of a resolved program: where it is stored, never its resolved program when the lazy
/// cache holds it (read back one page at a time); a behavior the cache does not hold keeps its program here.
#[derive(serde::Serialize, serde::Deserialize)]
struct LazyPage {
    orbital: String,
    orb_ref: String,
    key: Option<String>,
    held: Option<orbital_core::OrbitalSchema>,
    lazy: Vec<LazyPage>,
}

impl LazyPage {
    fn of(resolved: ResolvedLazy, stored: bool) -> Self {
        let stored_here = stored && resolved.cache_key.is_some();
        LazyPage {
            orbital: resolved.orbital,
            orb_ref: resolved.orb_ref,
            held: (!stored_here).then_some(resolved.schema),
            key: resolved.cache_key.filter(|_| stored_here),
            lazy: resolved.lazy.into_iter().map(|child| LazyPage::of(child, stored)).collect(),
        }
    }
}

/// A lazy page whose stored program is gone from the lazy cache (removed or unreadable): the entry resolves again.
#[derive(Debug)]
pub(crate) struct LazyPageGone(pub String);

impl std::fmt::Display for LazyPageGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the lazy behavior `{}` is no longer in the lazy cache", self.0)
    }
}

impl std::error::Error for LazyPageGone {}

/// A program resolved once, with every file its resolution read. Readers share it; none copies it.
pub(crate) struct ResolvedEntry {
    schema: orbital_core::OrbitalSchema,
    lazy: Vec<LazyPage>,
    cache: Option<std::sync::Arc<dyn LazyResolveCache>>,
    reads: orbital_core::read_set::ReadSet,
}

impl ResolvedEntry {
    fn new(
        schema: orbital_core::OrbitalSchema,
        lazy: Vec<ResolvedLazy>,
        cache: Option<std::sync::Arc<dyn LazyResolveCache>>,
        reads: orbital_core::read_set::ReadSet,
    ) -> Self {
        let stored = cache.is_some();
        ResolvedEntry { schema, lazy: lazy.into_iter().map(|page| LazyPage::of(page, stored)).collect(), cache, reads }
    }

    pub(crate) fn schema(&self) -> &orbital_core::OrbitalSchema {
        &self.schema
    }

    pub(crate) fn has_lazy(&self) -> bool {
        !self.lazy.is_empty()
    }

    pub(crate) fn lazy_len(&self) -> usize {
        self.lazy.len()
    }

    fn program_of(&self, page: &LazyPage) -> Result<orbital_core::OrbitalSchema, LazyPageGone> {
        if let Some(held) = &page.held {
            return Ok(held.clone());
        }
        page.key
            .as_deref()
            .zip(self.cache.as_ref())
            .and_then(|(key, cache)| cache.get(key))
            .map(|stored| stored.resolved.schema)
            .ok_or_else(|| LazyPageGone(page.orb_ref.clone()))
    }

    /// The `uses lazy` program whose file is `file` (its path joined to its importer's folder), read from the lazy
    /// cache; `Ok(None)` when no lazy behavior of this program has that file.
    pub(crate) fn lazy_program(&self, file: &str) -> Result<Option<orbital_core::OrbitalSchema>, LazyPageGone> {
        fn find<'a>(pages: &'a [LazyPage], folder: &str, file: &str) -> Option<&'a LazyPage> {
            pages.iter().find_map(|page| {
                let orb_ref = if folder.is_empty() { page.orb_ref.clone() } else { format!("{folder}/{}", page.orb_ref) };
                if orb_ref == file {
                    return Some(page);
                }
                let child_folder = orb_ref.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
                find(&page.lazy, &child_folder, file)
            })
        }
        find(&self.lazy, "", file).map(|page| self.program_of(page)).transpose()
    }

    /// Every lazy behavior with its resolved program, read from the lazy cache for this call only.
    pub(crate) fn lazy_resolved(&self) -> Result<Vec<ResolvedLazy>, LazyPageGone> {
        fn read(entry: &ResolvedEntry, pages: &[LazyPage]) -> Result<Vec<ResolvedLazy>, LazyPageGone> {
            pages
                .iter()
                .map(|page| {
                    Ok(ResolvedLazy {
                        orbital: page.orbital.clone(),
                        orb_ref: page.orb_ref.clone(),
                        schema: entry.program_of(page)?,
                        lazy: read(entry, &page.lazy)?,
                        cache_key: page.key.clone(),
                    })
                })
                .collect()
        }
        read(self, &self.lazy)
    }
}

type ResolvedKey = (PathBuf, String);

/// Programs resolved in this process: every entry a caller still holds, and the most recent few kept for a reopen.
#[derive(Default)]
struct ResolvedCache {
    entries: std::collections::HashMap<ResolvedKey, std::sync::Weak<ResolvedEntry>>,
    recent: std::collections::VecDeque<std::sync::Arc<ResolvedEntry>>,
}

/// How many recently resolved programs are kept with no caller holding them.
const RESOLVED_RECENT: usize = 8;

impl ResolvedCache {
    fn get(&mut self, key: &ResolvedKey) -> Option<std::sync::Arc<ResolvedEntry>> {
        let entry = self.entries.get(key)?.upgrade();
        match &entry {
            Some(entry) => self.touch(entry),
            None => {
                self.entries.remove(key);
            }
        }
        entry
    }

    fn insert(&mut self, key: ResolvedKey, entry: &std::sync::Arc<ResolvedEntry>) {
        self.entries.insert(key, std::sync::Arc::downgrade(entry));
        self.touch(entry);
        self.entries.retain(|_, held| held.strong_count() > 0);
    }

    fn touch(&mut self, entry: &std::sync::Arc<ResolvedEntry>) {
        self.recent.retain(|kept| !std::sync::Arc::ptr_eq(kept, entry));
        self.recent.push_front(std::sync::Arc::clone(entry));
        self.recent.truncate(RESOLVED_RECENT);
    }

    fn remove(&mut self, key: &ResolvedKey) {
        if let Some(entry) = self.entries.remove(key).and_then(|held| held.upgrade()) {
            self.recent.retain(|kept| !std::sync::Arc::ptr_eq(kept, &entry));
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

static RESOLVED: std::sync::LazyLock<std::sync::Mutex<ResolvedCache>> = std::sync::LazyLock::new(Default::default);

/// One gate per program being resolved: a request for a program another thread is resolving waits for that resolution
/// and reads its entry, so a program is never resolved twice at once.
static RESOLVING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<ResolvedKey, std::sync::Arc<std::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(Default::default);

thread_local! {
    /// The programs this thread is resolving, so a resolution that reaches its own program again does not wait on itself.
    static RESOLVING_HERE: std::cell::RefCell<std::collections::HashSet<ResolvedKey>> = Default::default();
}

/// The compiler settings a resolution depends on besides what it reads.
fn settings_key(options: &orbital_compiler::CompilerOptions) -> String {
    use sha2::{Digest, Sha256};
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
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Resolved lazy behaviors kept on disk between processes, under the derived-data directory in one folder per toolchain
/// (this binary and the installed language data): `<key>.json` holds an entry, `<slot>.slot` names a behavior's current key.
/// Whether an entry is still current is the resolver's to decide from its reads; this is storage only.
struct DerivedLazyCache {
    dir: PathBuf,
}

/// Toolchain folders unused this long are removed: another binary (the release `orb` beside `orb-dev`) keeps its own.
const LAZY_TOOLCHAIN_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// The file in a toolchain folder naming the binary that wrote it.
const LAZY_BINARY_MARK: &str = "binary";

impl DerivedLazyCache {
    fn in_dir(root: &Path, toolchain: &str) -> Self {
        Self { dir: root.join(toolchain) }
    }

    /// The cache of this binary and language data; `None` when they cannot be identified.
    fn for_toolchain() -> Option<Self> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        feed(&mut hasher, "lazy-resolve", b"1");
        feed_toolchain(&mut hasher).ok()?;
        let toolchain: String = hasher.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
        let binary = std::env::current_exe().ok()?;
        Some(Self::open(&derived_dir().join("lazy"), &toolchain, &binary))
    }

    /// The folder for `toolchain` under `root`, marked as written by `binary`: a folder another toolchain of the same
    /// binary wrote is removed (a rebuilt binary never reads it again), and any other folder unused for the TTL.
    fn open(root: &Path, toolchain: &str, binary: &Path) -> Self {
        let marker = binary.to_string_lossy();
        for folder in std::fs::read_dir(root).into_iter().flatten().flatten() {
            if folder.file_name().to_string_lossy() == toolchain {
                continue;
            }
            let same_binary = std::fs::read_to_string(folder.path().join(LAZY_BINARY_MARK)).is_ok_and(|wrote| wrote == marker);
            let unused = folder.metadata().and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age > LAZY_TOOLCHAIN_TTL);
            if same_binary || unused {
                let _ = std::fs::remove_dir_all(folder.path());
            }
        }
        let cache = Self::in_dir(root, toolchain);
        if let Err(e) = cache.write(LAZY_BINARY_MARK, marker.as_bytes()) {
            tracing::debug!(target: "almadar:cli:resolve-cache", dir = %cache.dir.display(), error = %e, "lazy cache: binary mark not written");
        }
        cache
    }

    /// `bytes` written to `name` whole or not at all.
    fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let staged = tempfile::NamedTempFile::new_in(&self.dir)?;
        std::fs::write(staged.path(), bytes)?;
        staged.persist(self.dir.join(name)).map(|_| ()).map_err(|e| e.error)
    }
}

impl orbital_compiler::phases::lazy::LazyResolveCache for DerivedLazyCache {
    fn get(&self, key: &str) -> Option<orbital_compiler::phases::lazy::CachedLazy> {
        let raw = std::fs::read(self.dir.join(format!("{key}.json"))).ok()?;
        orbital_core::intern::scope(|| orbital_core::json_value::from_json_slice(&raw)).ok()
    }

    fn put(&self, slot: &str, key: &str, entry: orbital_compiler::phases::lazy::CachedLazy) {
        let slot_file = self.dir.join(format!("{slot}.slot"));
        let stored = orbital_core::def_refs::to_vec_with_defs(&entry).map_err(std::io::Error::other).and_then(|bytes| self.write(&format!("{key}.json"), &bytes));
        if let Err(e) = stored.and_then(|_| {
            if let Ok(old) = std::fs::read_to_string(&slot_file) {
                if old != key {
                    let _ = std::fs::remove_file(self.dir.join(format!("{old}.json")));
                }
            }
            self.write(&format!("{slot}.slot"), key.as_bytes())
        }) {
            tracing::debug!(target: "almadar:cli:resolve-cache", dir = %self.dir.display(), error = %e, "lazy cache: entry not stored");
        }
    }
}

/// A resolved program as it is stored: the program, where its lazy pages are, and every file its resolution read.
#[derive(serde::Serialize)]
struct StoringProgram<'a> {
    schema: &'a orbital_core::OrbitalSchema,
    lazy: &'a [LazyPage],
    reads: &'a orbital_core::read_set::ReadSet,
}

#[derive(serde::Deserialize)]
struct StoredProgram {
    schema: orbital_core::OrbitalSchema,
    lazy: Vec<LazyPage>,
    reads: orbital_core::read_set::ReadSet,
}

impl DerivedLazyCache {
    /// The file a program is stored in: one per program path and compiler settings.
    fn program_file(key: &ResolvedKey) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        feed(&mut hasher, "path", path_bytes(&key.0));
        feed(&mut hasher, "settings", key.1.as_bytes());
        let id: String = hasher.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!("program-{id}.json")
    }

    /// Stores `entry` as the program `key` names, replacing what was stored for it.
    fn put_program(&self, key: &ResolvedKey, entry: &ResolvedEntry) {
        let stored = orbital_core::def_refs::to_vec_with_defs(&StoringProgram { schema: &entry.schema, lazy: &entry.lazy, reads: &entry.reads })
            .map_err(std::io::Error::other)
            .and_then(|bytes| self.write(&Self::program_file(key), &bytes));
        if let Err(e) = stored {
            tracing::debug!(target: "almadar:cli:resolve-cache", dir = %self.dir.display(), path = %key.0.display(), error = %e, "program not stored");
        }
    }

    fn get_program(&self, key: &ResolvedKey) -> Option<StoredProgram> {
        let raw = std::fs::read(self.dir.join(Self::program_file(key))).ok()?;
        orbital_core::intern::scope(|| orbital_core::json_value::from_json_slice(&raw)).ok()
    }
}

/// The program `key` names as `cache` stored it, while every file its resolution read is unchanged; its lazy pages are
/// read from the same cache.
fn stored_program(cache: &std::sync::Arc<DerivedLazyCache>, key: &ResolvedKey) -> Option<ResolvedEntry> {
    let stored = cache.get_program(key).filter(|stored| stored.reads.is_current())?;
    Some(ResolvedEntry {
        schema: stored.schema,
        lazy: stored.lazy,
        cache: Some(std::sync::Arc::clone(cache) as std::sync::Arc<dyn LazyResolveCache>),
        reads: stored.reads,
    })
}

static LAZY_CACHE: std::sync::LazyLock<Option<std::sync::Arc<DerivedLazyCache>>> =
    std::sync::LazyLock::new(|| DerivedLazyCache::for_toolchain().map(std::sync::Arc::new));

/// `schema` resolved and stored under `key`, its reads recorded from `begin` (which the caller has already run).
fn store(
    key: ResolvedKey,
    schema: orbital_core::OrbitalSchema,
    options: &orbital_compiler::CompilerOptions,
    path: &Path,
) -> anyhow::Result<std::sync::Arc<ResolvedEntry>> {
    let done = orbital_compiler::phases::lazy::resolve_program_with_path(schema, options, Some(path));
    let reads = orbital_core::read_set::end();
    let (schema, lazy) = done.map_err(|e| anyhow::anyhow!("resolve {}: {e}", path.display()))?;
    let entry = std::sync::Arc::new(ResolvedEntry::new(schema, lazy, options.lazy_cache.0.clone(), reads));
    if let Some(cache) = LAZY_CACHE.as_ref() {
        let (cache, stored, key) = (std::sync::Arc::clone(cache), std::sync::Arc::clone(&entry), key.clone());
        std::thread::spawn(move || {
            cache.put_program(&key, &stored);
            orbital_core::runtime::memory_release::release_freed_memory();
        });
    }
    let mut resolved = RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    resolved.insert(key, &entry);
    tracing::debug!(
        target: "almadar:cli:resolve-cache",
        path = %path.display(),
        outcome = "miss",
        resolved_bytes = ?serde_json::to_vec(&entry.schema).map(|b| b.len()).map_err(|e| e.to_string()),
        entries = resolved.len(),
        "resolve cache"
    );
    Ok(entry)
}

/// The program at `path` resolved once per path and compiler settings in this process, reused while every file the
/// resolution read, the program's own file included, is as it was read.
pub(crate) fn resolved_file(path: &Path) -> anyhow::Result<std::sync::Arc<ResolvedEntry>> {
    let mut options = orbital_compiler::CompilerOptions::from_env();
    if let Some(cache) = LAZY_CACHE.as_ref() {
        options.lazy_cache = orbital_compiler::phases::lazy::LazyCache(Some(std::sync::Arc::clone(cache) as std::sync::Arc<dyn orbital_compiler::phases::lazy::LazyResolveCache>));
    }
    let key = (path.to_path_buf(), settings_key(&options));
    if let Some(entry) = resolved_hit(&key, path) {
        return Ok(entry);
    }
    let reentrant = RESOLVING_HERE.with(|here| here.borrow().contains(&key));
    let gate = (!reentrant).then(|| {
        let mut gates = RESOLVING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::sync::Arc::clone(gates.entry(key.clone()).or_default())
    });
    let held = gate.as_ref().map(|gate| gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    let waited = match gate.is_some().then(|| resolved_hit(&key, path)).flatten().or_else(|| disk_hit(&key, path)) {
        Some(entry) => Ok(entry),
        None => {
            RESOLVING_HERE.with(|here| here.borrow_mut().insert(key.clone()));
            let resolved = resolve_uncached(key.clone(), &options, path);
            RESOLVING_HERE.with(|here| here.borrow_mut().remove(&key));
            resolved
        }
    };
    drop(held);
    if let Some(gate) = gate {
        let mut gates = RESOLVING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if std::sync::Arc::strong_count(&gate) == 2 {
            gates.remove(&key);
        }
    }
    waited
}

/// The program `key` names read back from disk (see [`stored_program`]) and kept as a memory entry; its reads replay.
fn disk_hit(key: &ResolvedKey, path: &Path) -> Option<std::sync::Arc<ResolvedEntry>> {
    let entry = std::sync::Arc::new(stored_program(LAZY_CACHE.as_ref()?, key)?);
    orbital_core::runtime::memory_release::release_freed_memory();
    entry.reads.replay();
    let mut resolved = RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    resolved.insert(key.clone(), &entry);
    tracing::debug!(target: "almadar:cli:resolve-cache", path = %path.display(), outcome = "disk-hit", entries = resolved.len(), "resolve cache");
    Some(entry)
}

/// The cached entry for `key` while every file its resolution read is unchanged; a hit replays those reads.
fn resolved_hit(key: &ResolvedKey, path: &Path) -> Option<std::sync::Arc<ResolvedEntry>> {
    let mut resolved = RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = resolved.get(key).filter(|entry| entry.reads.is_current())?;
    entry.reads.replay();
    tracing::debug!(target: "almadar:cli:resolve-cache", path = %path.display(), outcome = "hit", entries = resolved.len(), "resolve cache");
    Some(entry)
}

/// `path` read and resolved with the cache unlocked: a build-time describe resolves the program it names through this cache.
fn resolve_uncached(key: ResolvedKey, options: &orbital_compiler::CompilerOptions, path: &Path) -> anyhow::Result<std::sync::Arc<ResolvedEntry>> {
    orbital_core::read_set::begin();
    let parsed = orbital_core::read_set::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))
        .and_then(|content| {
            crate::utils::parse_schema_auto(&content, &path.display().to_string())
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))
        });
    let schema = match parsed {
        Ok(schema) => schema,
        Err(e) => {
            orbital_core::read_set::end();
            return Err(e);
        }
    };
    store(key, schema, options, path)
}

/// [`resolved_file`] once more after a lazy page it named left the lazy cache: the program is resolved again.
fn resolved_file_again(path: &Path) -> anyhow::Result<std::sync::Arc<ResolvedEntry>> {
    let options = orbital_compiler::CompilerOptions::from_env();
    RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&(path.to_path_buf(), settings_key(&options)));
    resolved_file(path)
}

/// The program at `path` and every lazy behavior with its resolved program, read for this call.
pub(crate) fn resolved_with_lazy(path: &Path) -> anyhow::Result<(std::sync::Arc<ResolvedEntry>, Vec<ResolvedLazy>)> {
    let entry = resolved_file(path)?;
    match entry.lazy_resolved() {
        Ok(lazy) => Ok((entry, lazy)),
        Err(_) => {
            let entry = resolved_file_again(path)?;
            let lazy = entry.lazy_resolved()?;
            Ok((entry, lazy))
        }
    }
}

/// The `uses lazy` program of the program at `path` whose file is `file`; `Ok(None)` when it has none with that file.
pub(crate) fn resolved_lazy_page(path: &Path, file: &str) -> anyhow::Result<Option<orbital_core::OrbitalSchema>> {
    match resolved_file(path)?.lazy_program(file) {
        Ok(page) => Ok(page),
        Err(_) => Ok(resolved_file_again(path)?.lazy_program(file)?),
    }
}

/// [`resolve_to_temp`] for a command program: one std ships is resolved once per state of the files its resolution
/// read and cached under the derived-data directory; any other program resolves as usual.
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

/// The cache entry name and key of a std command program, `None` for a program std does not ship. The key holds what
/// resolution depends on besides the files it reads: this binary, the language data and the resolve options.
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
    feed(&mut hasher, "resolve", b"2");
    feed_toolchain(&mut hasher)?;
    feed(&mut hasher, "package", format!("{}@{}:{}", std.name, std.version, std.prefix).as_bytes());
    feed(&mut hasher, "program", path_bytes(&program));
    feed(&mut hasher, "nominalize", if options.nominalize { b"1" } else { b"0" });
    let app_root = options.inline.loader_config.scoped_paths.get(orbital_compiler::phases::APP_SCOPE);
    feed(&mut hasher, "app-root", app_root.map_or(&[][..], |p| path_bytes(p)));
    for root in &roots {
        feed(&mut hasher, "tree", path_bytes(root));
    }
    let key = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Some((entry_stem(&program, &roots), key)))
}

fn program_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The resolved program cached under `cache/<stem>-<key>` with the files `build` read: a hit, while every one of them is
/// as it was read, copies the entry to a fresh run-owned directory under `parent`; a miss runs `build` and stores its
/// directory, replacing the stem's older entries. An entry appears whole or not at all.
fn cached_resolve(
    cache: &Path,
    parent: &Path,
    stem: &str,
    key: &str,
    build: impl FnOnce() -> anyhow::Result<ResolvedProgram>,
) -> anyhow::Result<ResolvedProgram> {
    let entry = cache.join(format!("{stem}-{key}"));
    if entry.join("program.orb").is_file() {
        let _read = entry_lock(cache, stem, key, false)?;
        if let Some(program) = cached_entry(&entry, parent, stem)? {
            return Ok(program);
        }
    }
    orbital_core::read_set::begin();
    let built = build();
    let reads = orbital_core::read_set::end();
    reads.replay();
    let program = built?;
    std::fs::create_dir_all(cache)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!("{stem}-{key}."))
        .suffix(".tmp")
        .tempdir_in(cache)?;
    copy_tree(program.dir.path(), staging.path())?;
    std::fs::write(staging.path().join(READS_FILE), serde_json::to_vec(&reads)?)?;
    let staged = staging.keep();
    let _write = entry_lock(cache, stem, key, true)?;
    if entry.exists() {
        let retired = tempfile::Builder::new().prefix(&format!("{stem}-{key}.")).suffix(".old").tempdir_in(cache)?.keep();
        match std::fs::rename(&entry, retired.join("entry")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let _ = std::fs::remove_dir_all(&retired);
    }
    if std::fs::rename(&staged, &entry).is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    tracing::debug!(target: "almadar:cli:resolve-cache", stem, outcome = "disk-miss", reads = reads.paths().count(), "command cache");
    for old in std::fs::read_dir(cache)?.flatten() {
        let name = old.file_name().to_string_lossy().into_owned();
        let other_key = |n: &str| n.rsplit_once('-').is_some_and(|(s, k)| s == stem && k != key);
        let stale = (!name.contains('.') && other_key(&name))
            || name.strip_suffix(".lock").is_some_and(other_key)
            || (name.starts_with(&format!("{stem}-")) && name.ends_with(".old"));
        if stale {
            let _ = if old.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(old.path()) } else { std::fs::remove_file(old.path()) };
        }
    }
    Ok(program)
}

/// The entry at `entry` copied to a fresh run-owned directory under `parent` while every file its resolution read is as
/// it was read; `None` when there is no entry, it is stale, or another run replaced it while this one read it.
fn cached_entry(entry: &Path, parent: &Path, stem: &str) -> anyhow::Result<Option<ResolvedProgram>> {
    let vanished = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let raw = match std::fs::read(entry.join(READS_FILE)) {
        Ok(raw) => raw,
        Err(e) if vanished(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let reads: orbital_core::read_set::ReadSet = orbital_core::json_value::from_json_slice(&raw)?;
    if !reads.is_current() {
        tracing::debug!(target: "almadar:cli:resolve-cache", stem, outcome = "disk-stale", "command cache");
        return Ok(None);
    }
    let program = ResolvedProgram { dir: scratch_dir_in(parent, "orbital-cmd-")? };
    match copy_tree(entry, program.dir.path()) {
        Ok(()) if program.path().is_file() => {}
        Ok(()) => return Ok(None),
        Err(e) if vanished(&e) => {
            tracing::debug!(target: "almadar:cli:resolve-cache", stem, outcome = "disk-replaced", "command cache");
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    reads.replay();
    tracing::debug!(target: "almadar:cli:resolve-cache", stem, outcome = "disk-hit", "command cache");
    Ok(Some(program))
}

/// The lock that orders runs sharing one cache entry: readers copy it under a shared lock, a writer swaps it under an
/// exclusive one, so no run reads an entry another run is replacing.
fn entry_lock(cache: &Path, stem: &str, key: &str, exclusive: bool) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(cache)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(cache.join(format!("{stem}-{key}.lock")))?;
    if exclusive { file.lock()? } else { file.lock_shared()? }
    Ok(file)
}

/// The files a cache entry's resolution read, beside the resolved program.
const READS_FILE: &str = "reads.json";

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name() == READS_FILE {
            continue;
        }
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
    use orbital_compiler::phases::lazy::{CachedLazy, LazyResolveCache, ResolvedLazy};
    use std::cell::Cell;

    fn cached(orbital: &str) -> CachedLazy {
        CachedLazy {
            resolved: ResolvedLazy {
                orbital: orbital.to_string(),
                orb_ref: format!("lazy/{orbital}.orb"),
                schema: orbital_core::json_value::from_json_value(&serde_json::json!({ "name": orbital, "orbitals": [] })).unwrap(),
                lazy: Vec::new(),
                cache_key: None,
            },
            reads: orbital_core::read_set::ReadSet::default(),
        }
    }

    #[test]
    fn a_resolved_lazy_behavior_stored_on_disk_is_read_back_by_a_later_process() {
        let root = tempfile::tempdir().unwrap();
        DerivedLazyCache::in_dir(root.path(), "tool-a").put("slot-1", "key-1", cached("Late"));
        let later = DerivedLazyCache::in_dir(root.path(), "tool-a");
        assert_eq!(later.get("key-1").map(|e| e.resolved.orbital), Some("Late".to_string()));
        assert!(later.get("key-2").is_none(), "control: a key never stored is a miss");
    }

    #[test]
    fn storing_a_lazy_behavior_again_replaces_its_older_entry_and_keeps_other_behaviors() {
        let root = tempfile::tempdir().unwrap();
        let cache = DerivedLazyCache::in_dir(root.path(), "tool-a");
        cache.put("slot-1", "old", cached("Late"));
        cache.put("slot-2", "other", cached("Other"));
        cache.put("slot-1", "new", cached("Late"));
        assert!(cache.get("old").is_none(), "the slot's older entry is gone");
        assert!(cache.get("new").is_some() && cache.get("other").is_some());
    }

    #[test]
    fn edge_an_unreadable_entry_and_another_toolchains_entry_are_misses() {
        let root = tempfile::tempdir().unwrap();
        let cache = DerivedLazyCache::in_dir(root.path(), "tool-a");
        cache.put("slot-1", "key-1", cached("Late"));
        assert!(DerivedLazyCache::in_dir(root.path(), "tool-b").get("key-1").is_none(), "a newer binary never reads an older one's entries");
        let file = std::fs::read_dir(root.path().join("tool-a")).unwrap().flatten().find(|e| e.file_name().to_string_lossy().ends_with("key-1.json")).unwrap().path();
        std::fs::write(&file, "not json").unwrap();
        assert!(cache.get("key-1").is_none(), "a corrupt entry is resolved again, never trusted");
    }

    #[test]
    fn a_new_toolchain_folder_replaces_the_folders_the_same_binary_wrote() {
        let root = tempfile::tempdir().unwrap();
        let dev = Path::new("/bin/orb-dev.bin");
        DerivedLazyCache::open(root.path(), "build-1", dev).put("slot-1", "key-1", cached("Late"));
        DerivedLazyCache::open(root.path(), "build-2", dev);
        assert!(!root.path().join("build-1").exists(), "the same binary's older folder is removed");
        assert!(root.path().join("build-2").exists());
    }

    #[test]
    fn control_another_binarys_folder_and_an_unmarked_folder_stay() {
        let root = tempfile::tempdir().unwrap();
        DerivedLazyCache::open(root.path(), "release-1", Path::new("/bin/orb")).put("slot-1", "key-1", cached("Late"));
        std::fs::create_dir_all(root.path().join("unmarked")).unwrap();
        DerivedLazyCache::open(root.path(), "build-1", Path::new("/bin/orb-dev.bin"));
        assert!(root.path().join("release-1").exists(), "another binary keeps its folder");
        assert!(root.path().join("unmarked").exists(), "a folder that names no binary waits for the TTL");
    }

    #[test]
    fn edge_reopening_the_current_toolchain_keeps_its_entries() {
        let root = tempfile::tempdir().unwrap();
        let dev = Path::new("/bin/orb-dev.bin");
        DerivedLazyCache::open(root.path(), "build-1", dev).put("slot-1", "key-1", cached("Late"));
        assert!(DerivedLazyCache::open(root.path(), "build-1", dev).get("key-1").is_some());
    }

    fn page(orbital: &str, key: Option<&str>) -> ResolvedLazy {
        let mut resolved = cached(orbital).resolved;
        resolved.cache_key = key.map(str::to_string);
        resolved
    }

    fn entry_over(cache: Option<std::sync::Arc<DerivedLazyCache>>, pages: Vec<ResolvedLazy>) -> ResolvedEntry {
        let schema = orbital_core::json_value::from_json_value(&serde_json::json!({ "name": "root", "orbitals": [] })).unwrap();
        ResolvedEntry::new(schema, pages, cache.map(|c| c as std::sync::Arc<dyn LazyResolveCache>), orbital_core::read_set::ReadSet::default())
    }

    #[test]
    fn a_lazy_page_the_cache_holds_is_read_from_it_and_not_kept_in_the_entry() {
        let root = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DerivedLazyCache::in_dir(root.path(), "tool-a"));
        cache.put("slot-1", "key-1", cached("Late"));
        let entry = entry_over(Some(cache), vec![page("Late", Some("key-1"))]);
        assert!(entry.lazy[0].held.is_none(), "the page's program stays on disk");
        let read = entry.lazy_program("lazy/Late.orb").unwrap().expect("the page");
        assert_eq!(read.name, "Late");
        assert!(entry.lazy_program("lazy/Other.orb").unwrap().is_none(), "control: a file no page has");
    }

    fn site_with_post(dir: &Path) -> PathBuf {
        std::fs::write(dir.join("post.orb"), POST).unwrap();
        let path = dir.join("site.orb");
        std::fs::write(&path, SITE).unwrap();
        path
    }

    #[test]
    fn a_stored_program_reads_back_as_it_was_resolved_with_its_lazy_pages_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = site_with_post(dir.path());
        let resolved = resolve_file(&path);
        let root = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DerivedLazyCache::in_dir(root.path(), "tool-a"));
        cache.put("slot-1", "key-1", cached("Late"));
        let entry = ResolvedEntry { schema: resolved.schema.clone(), lazy: vec![LazyPage::of(page("Late", Some("key-1")), true)], cache: None, reads: resolved.reads.clone() };
        let key = (path.clone(), "settings-a".to_string());
        cache.put_program(&key, &entry);
        let back = stored_program(&cache, &key).expect("stored, and every file it read is unchanged");
        assert_eq!(serde_json::to_value(back.schema()).unwrap(), serde_json::to_value(entry.schema()).unwrap());
        assert!(back.reads.paths().any(|f| f.ends_with("post.orb")), "its reads come back with it");
        assert_eq!(back.lazy_program("lazy/Late.orb").unwrap().map(|p| p.name), Some("Late".to_string()), "its lazy pages read from the same cache");
        assert!(stored_program(&cache, &(path, "settings-b".to_string())).is_none(), "control: other compiler settings are another program");
    }

    #[test]
    fn a_stored_program_is_not_read_back_once_a_file_it_read_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = site_with_post(dir.path());
        let resolved = resolve_file(&path);
        let root = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DerivedLazyCache::in_dir(root.path(), "tool-a"));
        let key = (path.clone(), "settings-a".to_string());
        cache.put_program(&key, &resolved);
        std::fs::write(dir.path().join("post.orb"), POST.replace("PostState", "PostStateRenamed")).unwrap();
        assert!(stored_program(&cache, &key).is_none(), "an import changed, so the program resolves again");
    }

    #[test]
    fn edge_an_unreadable_stored_program_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = site_with_post(dir.path());
        let root = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DerivedLazyCache::in_dir(root.path(), "tool-a"));
        let key = (path, "settings-a".to_string());
        cache.put_program(&key, &resolve_file(&key.0));
        let file = std::fs::read_dir(root.path().join("tool-a")).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("program-")).unwrap().path();
        std::fs::write(&file, "not json").unwrap();
        assert!(stored_program(&cache, &key).is_none(), "a corrupt program is resolved again, never trusted");
    }

    #[test]
    fn control_with_no_lazy_cache_a_page_keeps_its_program() {
        let entry = entry_over(None, vec![page("Late", Some("key-1"))]);
        assert!(entry.lazy[0].held.is_some());
        assert_eq!(entry.lazy_program("lazy/Late.orb").unwrap().map(|p| p.name), Some("Late".to_string()));
    }

    #[test]
    fn edge_a_page_gone_from_the_lazy_cache_is_reported_not_answered_empty() {
        let root = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DerivedLazyCache::in_dir(root.path(), "tool-a"));
        let entry = entry_over(Some(cache), vec![page("Late", Some("never-stored"))]);
        assert!(entry.lazy_program("lazy/Late.orb").is_err());
        assert!(entry.lazy_resolved().is_err());
    }

    #[test]
    fn a_lazy_cache_entry_is_stored_with_shared_subtrees_once_and_read_back_whole() {
        let root = tempfile::tempdir().unwrap();
        let cache = DerivedLazyCache::in_dir(root.path(), "tool-a");
        let mut entry = cached("Late");
        entry.resolved.schema = orbital_core::json_value::from_json_value(&serde_json::json!({ "name": "Late", "orbitals": [{ "name": "A", "entity": { "name": "E", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] }, "traits": [{ "name": "T", "linkedEntity": "E", "stateMachine": { "states": [{ "name": "idle", "isInitial": true }], "transitions": [
            { "from": "idle", "to": "idle", "event": "X", "effects": [["set", "@entity.v", { "presets": ["naqsh", "glass", "sand"] }]] },
            { "from": "idle", "to": "idle", "event": "Y", "effects": [["set", "@entity.v", { "presets": ["naqsh", "glass", "sand"] }]] }
        ] } }] }] }))
        .unwrap();
        let plain = serde_json::to_vec(&entry.resolved.schema).unwrap();
        cache.put("slot-1", "key-1", entry);
        let stored = std::fs::read_to_string(root.path().join("tool-a/key-1.json")).unwrap();
        assert_eq!(stored.matches("\"sand\"").count(), 1, "{stored}");
        let back = cache.get("key-1").expect("read back");
        assert_eq!(serde_json::to_vec(&back.resolved.schema).unwrap(), plain);
    }

    fn resolved_entry() -> std::sync::Arc<ResolvedEntry> {
        std::sync::Arc::new(entry_over(None, Vec::new()))
    }

    #[test]
    fn the_resolve_cache_keeps_the_most_recent_programs_and_releases_older_ones() {
        let mut cache = ResolvedCache::default();
        let keys: Vec<ResolvedKey> = (0..RESOLVED_RECENT + 2).map(|i| (PathBuf::from(format!("/p{i}.lolo")), String::new())).collect();
        for key in &keys {
            cache.insert(key.clone(), &resolved_entry());
        }
        assert!(cache.get(&keys[0]).is_none() && cache.get(&keys[1]).is_none(), "the oldest are released");
        assert!(keys[2..].iter().all(|key| cache.get(key).is_some()), "the most recent {RESOLVED_RECENT} stay");
    }

    #[test]
    fn control_a_program_a_caller_still_holds_stays_past_the_bound() {
        let mut cache = ResolvedCache::default();
        let held = resolved_entry();
        cache.insert((PathBuf::from("/held.lolo"), String::new()), &held);
        for i in 0..RESOLVED_RECENT + 2 {
            cache.insert((PathBuf::from(format!("/p{i}.lolo")), String::new()), &resolved_entry());
        }
        let again = cache.get(&(PathBuf::from("/held.lolo"), String::new())).expect("still held by its caller");
        assert!(std::sync::Arc::ptr_eq(&again, &held));
    }

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
        assert_eq!(entries, vec!["p-k2".to_string(), "p-k2.lock".to_string(), "q-k1".to_string(), "q-k1.lock".to_string()], "the stem's older key and its lock go");
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

    fn built_reading(parent: &Path, calls: &Cell<u32>, dep: &Path) -> anyhow::Result<ResolvedProgram> {
        let text = orbital_core::read_set::read_to_string(dep)?;
        built(parent, calls, &text)
    }

    #[test]
    fn an_entry_is_reused_until_a_file_its_build_read_changes_and_is_then_rebuilt_in_place() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let (dep, other) = (src.path().join("dep.lolo"), src.path().join("other.lolo"));
        std::fs::write(&dep, "one").unwrap();
        std::fs::write(&other, "x").unwrap();
        let calls = Cell::new(0);
        let run = || cached_resolve(cache.path(), work.path(), "p", "k1", || built_reading(work.path(), &calls, &dep)).unwrap();
        run();
        std::fs::write(&other, "y").unwrap();
        assert_eq!(std::fs::read_to_string(run().path()).unwrap(), "one", "control: a file the build never read changed");
        assert_eq!(calls.get(), 1);
        std::fs::write(&dep, "two").unwrap();
        assert_eq!(std::fs::read_to_string(run().path()).unwrap(), "two");
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_to_string(run().path()).unwrap(), "two");
        assert_eq!(calls.get(), 2, "the rebuilt entry is current again");
        assert_eq!(entries_without_locks(cache.path()), vec!["p-k1".to_string()], "one entry, nothing left from the swap");
        std::fs::create_dir_all(cache.path().join("p-k1.abc.old/entry")).unwrap();
        std::fs::create_dir_all(cache.path().join("p-k1.def.tmp")).unwrap();
        std::fs::write(&dep, "three").unwrap();
        run();
        assert_eq!(entries_without_locks(cache.path()), vec!["p-k1".to_string(), "p-k1.def.tmp".to_string()], "a retired entry left by a killed run goes; another run's staging stays");
    }

    #[test]
    fn an_entry_another_run_replaced_while_this_one_read_it_is_a_miss_not_a_failure() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "A")).unwrap();
        let entry = cache.path().join("p-k1");
        std::fs::remove_file(entry.join(READS_FILE)).unwrap();
        let out = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "B")).unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_to_string(out.path()).unwrap(), "B");
        std::fs::remove_file(cache.path().join("p-k1/program.orb")).unwrap();
        let again = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "C")).unwrap();
        assert_eq!((calls.get(), std::fs::read_to_string(again.path()).unwrap()), (3, "C".to_string()));
        let hit = cached_resolve(cache.path(), work.path(), "p", "k1", || built(work.path(), &calls, "D")).unwrap();
        assert_eq!((calls.get(), std::fs::read_to_string(hit.path()).unwrap()), (3, "C".to_string()), "control: a whole entry hits");
    }

    #[test]
    fn runs_rebuilding_one_stale_entry_at_once_each_answer_their_program() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let dep = src.path().join("dep.lolo");
        std::fs::write(&dep, "one").unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), work.path(), "p", "k1", || built_reading(work.path(), &calls, &dep)).unwrap();
        std::fs::write(&dep, "two").unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (cache, work, dep) = (cache.path().to_path_buf(), work.path().to_path_buf(), dep.clone());
                std::thread::spawn(move || {
                    let calls = Cell::new(0);
                    let program = cached_resolve(&cache, &work, "p", "k1", || built_reading(&work, &calls, &dep)).unwrap();
                    std::fs::read_to_string(program.path()).unwrap()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), "two");
        }
        assert_eq!(entries_without_locks(cache.path()), vec!["p-k1".to_string()], "one entry is left");
    }

    #[test]
    fn a_hit_counts_its_reads_in_the_open_recording() {
        let cache = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let dep = src.path().join("dep.lolo");
        std::fs::write(&dep, "one").unwrap();
        let calls = Cell::new(0);
        cached_resolve(cache.path(), work.path(), "p", "k1", || built_reading(work.path(), &calls, &dep)).unwrap();
        orbital_core::read_set::begin();
        cached_resolve(cache.path(), work.path(), "p", "k1", || built_reading(work.path(), &calls, &dep)).unwrap();
        let outer = orbital_core::read_set::end();
        assert_eq!(calls.get(), 1);
        assert!(outer.paths().any(|p| p == &dep));
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

    fn entries_without_locks(dir: &Path) -> Vec<String> {
        entries(dir).into_iter().filter(|n| !n.ends_with(".lock")).collect()
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
        resolved_file(path).unwrap()
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

    /// The file path: no JSON hash of the program, the program's own file is in the read set, and a hit is the same shared
    /// entry (never a copy).
    #[test]
    fn a_file_resolved_twice_is_one_shared_entry_until_the_file_or_an_import_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let path = dir.path().join("site.orb");
        std::fs::write(&path, SITE).unwrap();
        let first = resolved_file(&path).unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &resolved_file(&path).unwrap()));
        assert!(first.reads.paths().any(|f| f.ends_with("site.orb")), "the program's own file is read-tracked");
        std::fs::write(&path, SITE.replace("HomeState", "HomeStateRenamed")).unwrap();
        let edited = resolved_file(&path).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&first, &edited), "an edit to the program file resolves again");
        assert!(serde_json::to_string(edited.schema()).unwrap().contains("HomeStateRenamed"));
        std::fs::write(dir.path().join("post.orb"), POST.replace("PostState", "PostStateRenamed")).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&edited, &resolved_file(&path).unwrap()), "an edit to an import resolves again");
    }

    fn wide_program(name: &str, orbitals: usize) -> String {
        let orbital = |i: usize| format!(r#"{{"name":"O{i}","entity":{{"name":"S{i}","persistence":"runtime","fields":[{{"name":"id","type":"string","required":true}}]}},"traits":[{{"name":"T{i}","linkedEntity":"S{i}","stateMachine":{{"states":[{{"name":"a","isInitial":true}},{{"name":"b"}}],"events":[{{"key":"GO","name":"Go"}}],"transitions":[{{"from":"a","to":"b","event":"GO","effects":[["set","@entity.id","x"]]}}]}}}}],"pages":[]}}"#);
        format!(r#"{{"name":"{name}","version":"1.0.0","orbitals":[{}]}}"#, (0..orbitals).map(orbital).collect::<Vec<_>>().join(","))
    }

    fn resolved_together(paths: &[PathBuf]) -> Vec<std::sync::Arc<ResolvedEntry>> {
        let start = std::sync::Arc::new(std::sync::Barrier::new(paths.len()));
        let workers: Vec<_> = paths
            .iter()
            .cloned()
            .map(|path| {
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    resolved_file(&path).expect("resolves")
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().expect("worker")).collect()
    }

    #[test]
    fn requests_for_one_program_arriving_together_share_one_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.orb");
        std::fs::write(&path, wide_program("wide", 300)).unwrap();
        let entries = resolved_together(&[path.clone(), path.clone(), path.clone(), path]);
        assert!(entries.iter().all(|e| std::sync::Arc::ptr_eq(e, &entries[0])), "every request got the one resolved entry");
    }

    #[test]
    fn control_different_programs_arriving_together_resolve_on_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (dir.path().join("one.orb"), dir.path().join("two.orb"));
        std::fs::write(&one, wide_program("one", 50)).unwrap();
        std::fs::write(&two, wide_program("two", 50)).unwrap();
        let entries = resolved_together(&[one, two]);
        assert!(!std::sync::Arc::ptr_eq(&entries[0], &entries[1]));
        assert_eq!((entries[0].schema().name.as_str(), entries[1].schema().name.as_str()), ("one", "two"));
    }

    #[test]
    fn a_resolution_whose_build_time_describe_resolves_another_program_finishes() {
        crate::register_describer_for_tests();
        // The describer resolves behaviors under the working directory only.
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        std::fs::write(dir.path().join("door.orb"), r#"{"name":"Door","version":"1.0.0","orbitals":[{"name":"Door","entity":{"name":"DoorState","persistence":"runtime","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"DoorLife","linkedEntity":"DoorState","stateMachine":{"states":[{"name":"shut","isInitial":true},{"name":"open"}],"events":[{"key":"OPEN","name":"Open"}],"transitions":[{"from":"shut","to":"open","event":"OPEN","effects":[]}]}}],"pages":[]}]}"#).unwrap();
        let path = dir.path().join("diagram.orb");
        let door = dir.path().join("door.orb").display().to_string();
        std::fs::write(&path, r#"{"name":"diagram","version":"1.0.0","orbitals":[{"name":"Diagram","entity":{"name":"DiagramState","persistence":"runtime","fields":[{"name":"id","type":"string","required":true}]},"traits":[{"name":"Draw","linkedEntity":"DiagramState","stateMachine":{"states":[{"name":"idle","isInitial":true}],"events":[{"key":"INIT","name":"Init"},{"key":"DESCRIBED","name":"Described"}],"transitions":[{"from":"idle","to":"idle","event":"INIT","effects":[["behavior/describe",{"behavior":"DOOR","trait":"DoorLife"},{"emit":{"success":"DESCRIBED"}}]]}]}}],"pages":[]}]}"#.replace("DOOR", &door)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(resolved_file(&worker).map(|entry| serde_json::to_value(entry.schema()).unwrap_or_default()));
        });
        let resolved = rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the resolution finished instead of waiting on its own cache lock")
            .expect("resolves");
        let init = &resolved["orbitals"][0]["traits"][0]["stateMachine"]["transitions"][0]["effects"][0];
        assert_eq!(init[0], "emit", "the describe was answered at build time: {init}");
        assert_eq!(init[1], "DESCRIBED");
    }

    #[test]
    fn a_program_edited_many_times_keeps_one_entry_for_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.orb"), POST).unwrap();
        let path = dir.path().join("site.orb");
        let held = |path: &Path| RESOLVED.lock().unwrap().entries.keys().filter(|(p, _)| p == path).count();
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
        assert_eq!(entries_without_locks(cache.path()), vec!["p-k1".to_string()], "the cache entry stays");
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
