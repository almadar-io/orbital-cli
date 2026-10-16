//! The behavior index of an installed package, derived on read from its tree, its `.lolo` headers and
//! its lowered programs: what `behaviors-registry.json` used to carry for the readers that remain
//! (`catalog`, `texts`, `trait-capabilities`, the vector snapshot and import). The structural half
//! (topic, level, exposure, `@capabilities`) is `orbital_core::behavior_index`; this module adds what
//! only the lowered program says (description, entity, events, traits) and what only the resolved
//! program says (the capabilities its traits resolve to).
//!
//! The program-side rules are the retired TS extraction's, ported 1:1 onto the lowered program as a
//! JSON value, so the embedding texts built from them keep their hashes.

use orbital_compiler::loader::{ExternalOrbitalLoader, ImportChain, LoaderConfig};
use orbital_core::behavior_index::{
    build_description, hand_authored_behaviors, header_tags, indexed_files, lolo_path_of,
    resolve_exposure, IndexedFile, PackageSource,
};
use orbital_core::behavior_packages::{index_registry_source, BehaviorPackage, STD_PACKAGE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Bump when the entry shape or a rule below changes: it is part of the cache key.
const INDEX_RULES: &str = "behavior-index-2";
const WORKERS: usize = 8;
const MAX_IMPORT_DEPTH: usize = 16;

/// One shipped behavior, as the readers of the index need it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IndexEntry {
    pub name: String,
    /// Directories between the package's tree root and the file: the scope a path prefix selects.
    pub dirs: Vec<String>,
    pub level: String,
    pub topic: String,
    pub exposure: Option<String>,
    pub description: String,
    /// The `@capabilities` header tag: the organism's comma-separated phrases.
    pub capabilities: Option<String>,
    /// The `@almadar/ui` pattern the behavior is the factory of (`;; @pattern`).
    pub pattern: Option<String>,
    pub default_entity_name: String,
    pub connectable_events: Vec<String>,
    pub traits: Vec<String>,
    /// The capabilities the traits resolve to (composed traits included), sorted and unique.
    pub trait_capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// program-side rules (pure, over the lowered program as JSON)
// ---------------------------------------------------------------------------

type Lookup<'a> = &'a dyn Fn(&str) -> Option<Rc<Value>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgramFacts {
    pub description: String,
    pub state_pattern: String,
    pub default_entity_name: String,
    pub connectable_events: Vec<String>,
    pub traits: Vec<String>,
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn local_machine(t: &Value) -> Option<&Value> {
    t.get("stateMachine").filter(|m| m.is_object())
}

fn posix_basename(from: &str) -> &str {
    from.trim_end_matches('/').rsplit('/').next().unwrap_or(from)
}

/// `<Alias>.traits.<Trait>` resolved through the orbital's `uses`: the target behavior's name and the trait.
fn trait_ref_target(t: &Value, orbital: &Value) -> Option<(String, String)> {
    let mut parts = str_of(t, "ref")?.split('.');
    let (alias, kind, name) = (parts.next()?, parts.next()?, parts.next()?);
    if kind != "traits" || alias.is_empty() || name.is_empty() {
        return None;
    }
    let from = array(orbital, "uses")
        .iter()
        .find(|u| str_of(u, "as") == Some(alias))
        .and_then(|u| str_of(u, "from"))?;
    Some((posix_basename(from).to_string(), name.to_string()))
}

/// The state machine a trait runs: its own, else the one of the trait it composes (following the
/// chain through the first orbital of each target behavior).
fn trait_machine(
    t: &Value,
    orbital: &Value,
    seen: &mut HashSet<String>,
    lookup: Lookup,
) -> Option<Value> {
    if let Some(m) = local_machine(t) {
        return Some(m.clone());
    }
    let (behavior, name) = trait_ref_target(t, orbital)?;
    if !seen.insert(format!("{behavior}::{name}")) {
        return None;
    }
    let target = lookup(&behavior)?;
    let first = array(&target, "orbitals").first()?;
    let next = array(first, "traits")
        .iter()
        .find(|x| x.is_object() && str_of(x, "name") == Some(name.as_str()))?;
    trait_machine(next, first, seen, lookup)
}

/// A raw `orbital.traits[i]` entry's machine: the sibling it names when a bare string.
fn raw_trait_machine(raw: &Value, orbital: &Value, lookup: Lookup) -> Option<Value> {
    match raw {
        Value::String(name) => {
            let sibling = array(orbital, "traits")
                .iter()
                .find(|x| x.is_object() && str_of(x, "name") == Some(name.as_str()))?;
            trait_machine(sibling, orbital, &mut HashSet::new(), lookup)
        }
        Value::Object(_) => trait_machine(raw, orbital, &mut HashSet::new(), lookup),
        _ => None,
    }
}

/// The one machine an orbital contributes: the first trait with its own, else the first that resolves to one.
fn orbital_machine(orbital: &Value, lookup: Lookup) -> Option<Value> {
    let traits = array(orbital, "traits");
    traits
        .iter()
        .filter(|t| t.is_object())
        .find_map(local_machine)
        .cloned()
        .or_else(|| {
            traits
                .iter()
                .find_map(|t| raw_trait_machine(t, orbital, lookup))
        })
}

/// The orbital an imported orbital stands for (`reference.ref = Alias.orbitals.Name`): the target
/// behavior and the orbital's position in it.
fn import_target(orbital: &Value, lookup: Lookup) -> Option<(Rc<Value>, usize)> {
    let reference = orbital.get("reference")?;
    let mut parts = str_of(reference, "ref")?.split('.');
    let (alias, kind, name) = (parts.next()?, parts.next()?, parts.next()?);
    if kind != "orbitals" || name.is_empty() {
        return None;
    }
    let from = array(orbital, "uses")
        .iter()
        .find(|u| str_of(u, "as") == Some(alias))
        .and_then(|u| str_of(u, "from"))?;
    let base = posix_basename(from);
    let base = base.strip_suffix(".orb").unwrap_or(base);
    let target = lookup(base)?;
    let at = array(&target, "orbitals")
        .iter()
        .position(|o| str_of(o, "name") == Some(name))?;
    Some((target, at))
}

/// The traits an orbital signs: its own, then the imported orbital's that survive `only`/`omit`
/// under the orbital's name as prefix.
fn signature_traits(orbital: &Value, lookup: Lookup, depth: usize) -> Vec<String> {
    let mut names: Vec<String> = array(orbital, "traits")
        .iter()
        .map(|t| match t {
            Value::String(s) => s.clone(),
            other => str_of(other, "name")
                .or_else(|| str_of(other, "ref"))
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    let own_name = str_of(orbital, "name").unwrap_or("");
    if depth < MAX_IMPORT_DEPTH {
        if let Some((target, at)) = import_target(orbital, lookup) {
            let reference = &orbital["reference"];
            let only: Option<Vec<&str>> = reference
                .get("only")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect());
            let omit: Vec<&str> = array(reference, "omit")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for t in signature_traits(&array(&target, "orbitals")[at], lookup, depth + 1) {
                let kept = match &only {
                    Some(only) => only.contains(&t.as_str()),
                    None => !omit.contains(&t.as_str()),
                };
                if kept {
                    names.push(format!("{own_name}{t}"));
                }
            }
        }
    }
    names
}

/// The names of the entities an orbital signs: its inline entity, else the imported orbital's.
fn signature_entities(orbital: &Value, lookup: Lookup, depth: usize) -> Vec<Option<String>> {
    let own: Vec<Option<String>> = match orbital.get("entity") {
        Some(e) if e.is_object() && e.get("extends").is_none() => {
            vec![str_of(e, "name").map(str::to_string)]
        }
        _ => Vec::new(),
    };
    if !own.is_empty() || depth >= MAX_IMPORT_DEPTH || orbital.get("reference").is_none() {
        return own;
    }
    match import_target(orbital, lookup) {
        Some((target, at)) => signature_entities(&array(&target, "orbitals")[at], lookup, depth + 1),
        None => own,
    }
}

/// The facts a lowered program yields; `None` when it has no orbital (such a behavior is not indexed).
pub(crate) fn program_facts(orb: &Value, lookup: Lookup) -> Option<ProgramFacts> {
    let orbitals = array(orb, "orbitals");
    let first = orbitals.first()?;

    let machines: Vec<Value> = orbitals
        .iter()
        .filter_map(|o| orbital_machine(o, lookup))
        .collect();
    let mut states: Vec<&str> = machines
        .iter()
        .flat_map(|m| array(m, "states"))
        .filter_map(|s| str_of(s, "name"))
        .collect();
    states.sort_unstable();
    let mut seen = HashSet::new();
    let connectable_events: Vec<String> = machines
        .iter()
        .flat_map(|m| array(m, "events"))
        .filter_map(|e| str_of(e, "key"))
        .filter(|k| *k != "INIT")
        .filter(|k| seen.insert(*k))
        .map(str::to_string)
        .collect();

    let entity = first.get("entity");
    let referenced = match entity {
        Some(e) if e.is_string() || (e.is_object() && e.get("extends").is_some()) => {
            signature_entities(first, lookup, 0).into_iter().next()
        }
        _ => None,
    };
    let default_entity_name = match referenced {
        Some(Some(name)) => name,
        _ => entity
            .and_then(|e| str_of(e, "name"))
            .filter(|n| !n.is_empty())
            .unwrap_or("Item")
            .to_string(),
    };

    let mut seen_traits = HashSet::new();
    let traits: Vec<String> = orbitals
        .iter()
        .flat_map(|o| signature_traits(o, lookup, 0))
        .filter(|t| seen_traits.insert(t.clone()))
        .collect();

    Some(ProgramFacts {
        description: str_of(orb, "description").unwrap_or("").to_string(),
        state_pattern: states.join(", "),
        default_entity_name,
        connectable_events,
        traits,
    })
}

// ---------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------

/// One thread's view of the installed packages: a loader, and the lowered programs it has read.
struct Programs<'a> {
    packages: &'a [BehaviorPackage],
    owners: &'a HashMap<String, usize>,
    loader: ExternalOrbitalLoader,
    cache: HashMap<String, Option<Rc<Value>>>,
}

impl<'a> Programs<'a> {
    fn new(packages: &'a [BehaviorPackage], owners: &'a HashMap<String, usize>) -> Self {
        let loader = ExternalOrbitalLoader::new(LoaderConfig {
            base_path: std::env::current_dir().unwrap_or_default(),
            packages: packages.to_vec(),
            ..Default::default()
        });
        Self {
            packages,
            owners,
            loader,
            cache: HashMap::new(),
        }
    }

    fn orb_of(&mut self, name: &str) -> anyhow::Result<Option<Rc<Value>>> {
        if let Some(hit) = self.cache.get(name) {
            return Ok(hit.clone());
        }
        let Some(&owner) = self.owners.get(name) else {
            self.cache.insert(name.to_string(), None);
            return Ok(None);
        };
        let spec = format!("{}/{name}", self.packages[owner].prefix);
        let loaded = self
            .loader
            .load(&spec, None, &mut ImportChain::new())
            .map_err(|e| anyhow::anyhow!("cannot load `{spec}`: {e}"))?;
        let value = Rc::new(serde_json::to_value(&loaded.schema)?);
        self.cache.insert(name.to_string(), Some(value.clone()));
        Ok(Some(value))
    }
}

/// name -> the package (by position) whose tree holds it; the first package wins a shared name.
fn owners_of(packages: &[BehaviorPackage]) -> anyhow::Result<HashMap<String, usize>> {
    let mut owners = HashMap::new();
    for (i, pkg) in packages.iter().enumerate() {
        for name in index_registry_source(&pkg.registry)?.into_keys() {
            owners.entry(name).or_insert(i);
        }
    }
    Ok(owners)
}

fn read_lolo(pkg: &BehaviorPackage, file: &IndexedFile) -> anyhow::Result<Option<String>> {
    let Some(path) = lolo_path_of(pkg, file) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => anyhow::bail!("failed to read {}: {e}", path.display()),
    }
}

/// One behavior's entry; `None` when its program has no orbital.
fn entry_of(
    pkg: &BehaviorPackage,
    file: &IndexedFile,
    hand_authored: &HashSet<String>,
    programs: &std::cell::RefCell<Programs>,
) -> anyhow::Result<Option<IndexEntry>> {
    let fail = |e: String| anyhow::anyhow!("{}/{}: {e}", pkg.prefix, file.name);
    let orb = programs
        .borrow_mut()
        .orb_of(&file.name)?
        .ok_or_else(|| fail("the package holds no such program".to_string()))?;
    let failure = std::cell::RefCell::new(None);
    let lookup = |name: &str| match programs.borrow_mut().orb_of(name) {
        Ok(found) => found,
        Err(e) => {
            failure.borrow_mut().get_or_insert(e);
            None
        }
    };
    let facts = program_facts(&orb, &lookup);
    if let Some(e) = failure.into_inner() {
        return Err(e);
    }
    let Some(facts) = facts else {
        return Ok(None);
    };
    let lolo = read_lolo(pkg, file)?;
    let tags = match &lolo {
        Some(text) => header_tags(text).map_err(|e| fail(e))?,
        None => Default::default(),
    };
    let exposure = resolve_exposure(
        file,
        PackageSource::of(&pkg.name),
        hand_authored.contains(&file.name),
        tags.exposure.as_deref(),
    );
    let level = file.level();
    let topic = file.topic();
    let description = build_description(
        &file.name,
        &facts.description,
        lolo.as_deref(),
        level,
        &topic,
        &facts.state_pattern,
    );
    let spec = format!("{}/{}", pkg.prefix, file.name);
    let trait_capabilities = {
        let mut p = programs.borrow_mut();
        let Programs {
            packages, loader, ..
        } = &mut *p;
        let resolved = super::behaviors_catalog::resolve_with_loader(loader, packages, &spec, None)?;
        super::behaviors_catalog::resolved_trait_capabilities(&resolved, &spec)?
    };
    Ok(Some(IndexEntry {
        name: file.name.clone(),
        dirs: file.dirs.clone(),
        level: level.as_str().to_string(),
        topic,
        exposure,
        description,
        capabilities: tags.capabilities,
        pattern: tags.pattern,
        default_entity_name: facts.default_entity_name,
        connectable_events: facts.connectable_events,
        traits: facts.traits,
        trait_capabilities,
    }))
}

/// The entries of `packages[pi]`, all of them or only those named by `only`.
fn derive(
    packages: &[BehaviorPackage],
    pi: usize,
    only: Option<&HashSet<&str>>,
) -> anyhow::Result<Vec<IndexEntry>> {
    let pkg = &packages[pi];
    let files: Vec<IndexedFile> = indexed_files(pkg)?
        .into_iter()
        .filter(|f| only.is_none_or(|names| names.contains(f.name.as_str())))
        .collect();
    let hand_authored = match &pkg.lolo_source {
        Some(root) => hand_authored_behaviors(root).map_err(|e| anyhow::anyhow!(e))?,
        None => HashSet::new(),
    };
    let owners = owners_of(packages)?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    type Slot = std::sync::Mutex<Option<anyhow::Result<Option<IndexEntry>>>>;
    let slots: Vec<Slot> = files.iter().map(|_| Slot::default()).collect();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(files.len()) {
            scope.spawn(|| {
                let programs = std::cell::RefCell::new(Programs::new(packages, &owners));
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(file) = files.get(i) else {
                        break;
                    };
                    let entry = entry_of(pkg, file, &hand_authored, &programs);
                    if let Ok(mut slot) = slots[i].lock() {
                        *slot = Some(entry);
                    }
                }
            });
        }
    });
    let mut out = Vec::with_capacity(files.len());
    for slot in slots {
        let entry = slot
            .into_inner()
            .map_err(|_| anyhow::anyhow!("an index worker panicked"))?
            .ok_or_else(|| anyhow::anyhow!("a behavior was never indexed"))??;
        out.extend(entry);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------

fn derived_dir() -> PathBuf {
    match std::env::var_os("ALMADAR_DERIVED_CACHE") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".almadar")
            .join("derived"),
    }
}

fn feed(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// What the derived index of `packages[pi]` depends on: the rules, this binary, the installed language
/// data, and the contents of the package's tree, its `.lolo` mirror and its hand-authored lists, plus
/// every other package's unless it is the substrate (std never composes from another package).
fn fingerprint(packages: &[BehaviorPackage], pi: usize) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    feed(&mut hasher, "rules", INDEX_RULES.as_bytes());
    let exe = std::env::current_exe()?;
    let meta = std::fs::metadata(&exe)?;
    let modified = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    feed(&mut hasher, "exe", format!("{}:{modified}", meta.len()).as_bytes());
    let data = orbital_core::language_data::language_data()
        .map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))?;
    for (label, text) in [
        ("patterns", &data.patterns),
        ("mapping", &data.component_mapping),
        ("events", &data.event_contracts),
        ("services", &data.services),
        ("binding", &data.binding_rules),
        ("types", &data.program_types),
    ] {
        feed(&mut hasher, label, text.as_bytes());
    }
    let substrate = packages[pi].name == STD_PACKAGE;
    for (i, pkg) in packages.iter().enumerate() {
        if i != pi && substrate {
            continue;
        }
        feed(&mut hasher, "package", format!("{}@{}:{}", pkg.name, pkg.version, pkg.prefix).as_bytes());
        feed(&mut hasher, "files", pkg.files.join("\n").as_bytes());
        let mut paths: Vec<PathBuf> = index_registry_source(&pkg.registry)?.into_values().collect();
        paths.sort();
        for path in paths {
            let rel = path.strip_prefix(pkg.registry.root()).unwrap_or(&path);
            feed(&mut hasher, &rel.to_string_lossy(), &std::fs::read(&path)?);
        }
        if let Some(root) = &pkg.lolo_source {
            if matches!(pkg.registry, orbital_core::behavior_packages::RegistrySource::Materialized(_)) {
                let mut lolo: Vec<PathBuf> = Vec::new();
                collect_lolo(root, &mut lolo);
                lolo.sort();
                for path in lolo {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    feed(&mut hasher, &rel.to_string_lossy(), &std::fs::read(&path)?);
                }
            }
            let mut boards: Vec<String> = hand_authored_behaviors(root)
                .map_err(|e| anyhow::anyhow!(e))?
                .into_iter()
                .collect();
            boards.sort();
            feed(&mut hasher, "hand-authored", boards.join("\n").as_bytes());
        }
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn collect_lolo(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lolo(&path, out);
        } else if path.extension().is_some_and(|e| e == "lolo") {
            out.push(path);
        }
    }
}

/// Every behavior `packages[pi]` ships, in path order, derived from the live sources and cached under the
/// derived-data directory by what they depend on.
pub(crate) fn package_index(
    packages: &[BehaviorPackage],
    pi: usize,
) -> anyhow::Result<Vec<IndexEntry>> {
    let pkg = &packages[pi];
    let dir = derived_dir().join("behavior-index");
    let key = fingerprint(packages, pi)?;
    let file = dir.join(format!("{}-{key}.json", pkg.prefix));
    if let Ok(raw) = std::fs::read_to_string(&file) {
        if let Ok(entries) = serde_json::from_str::<Vec<IndexEntry>>(&raw) {
            return Ok(entries);
        }
    }
    let entries = derive(packages, pi, None)?;
    if std::fs::create_dir_all(&dir).is_ok() {
        let tmp = dir.join(format!("{}-{key}.{}.tmp", pkg.prefix, std::process::id()));
        let stale_prefix = format!("{}-", pkg.prefix);
        if std::fs::write(&tmp, serde_json::to_vec(&entries)?).is_ok()
            && std::fs::rename(&tmp, &file).is_ok()
        {
            if let Ok(listing) = std::fs::read_dir(&dir) {
                for old in listing.flatten() {
                    let name = old.file_name().to_string_lossy().to_string();
                    let stale = name
                        .strip_prefix(&stale_prefix)
                        .and_then(|rest| rest.strip_suffix(".json"))
                        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
                    if stale && old.path() != file {
                        let _ = std::fs::remove_file(old.path());
                    }
                }
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The index the retired `behaviors-registry.json` carried, for a sample of the workspace's std and io
    /// behaviors (those whose sources predate the file), as the readers use it.
    const GOLDEN: &str = include_str!("../../tests/fixtures/behavior_index_golden.json");

    #[test]
    fn the_derived_index_equals_the_retired_registry_for_a_sample() {
        let cwd = std::env::current_dir().unwrap();
        let Ok(packages) = super::super::npm_project::discover_from(&cwd) else {
            return;
        };
        crate::utils::require_language_data().expect("the workspace installs the language");
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let golden: Value = serde_json::from_str(GOLDEN).unwrap();
        let mut checked = 0;
        let mut unloadable = 0;
        for (prefix, sample) in golden.as_object().unwrap() {
            let Some(pi) = packages.iter().position(|p| &p.prefix == prefix && p.is_link) else {
                continue;
            };
            let mut derived = Vec::new();
            for name in sample.as_object().unwrap().keys() {
                match derive(&packages, pi, Some(&HashSet::from([name.as_str()]))) {
                    Ok(found) => derived.extend(found),
                    Err(e) if format!("{e:#}").contains("Failed to lower") => {
                        eprintln!("skipped {prefix}/{name}: a source being edited does not lower: {e:#}");
                        unloadable += 1;
                    }
                    Err(e) => panic!("{prefix}/{name}: {e:#}"),
                }
            }
            let mut mismatches = Vec::new();
            for entry in derived {
                let want = &sample[entry.name.as_str()];
                let strings = |key: &str| -> Vec<String> {
                    want[key]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                        .unwrap_or_default()
                };
                let checks: [(&str, bool); 10] = [
                    ("level", want["level"] == entry.level.as_str()),
                    ("topic", want["topic"] == entry.topic.as_str()),
                    ("exposure", want["exposure"].as_str() == entry.exposure.as_deref()),
                    ("description", want["description"] == entry.description.as_str()),
                    ("capabilities", want["capabilities"].as_str() == entry.capabilities.as_deref()),
                    ("pattern", want["pattern"].as_str() == entry.pattern.as_deref()),
                    ("defaultEntityName", want["defaultEntityName"] == entry.default_entity_name.as_str()),
                    ("connectableEvents", strings("connectableEvents") == entry.connectable_events),
                    ("traits", strings("traits") == entry.traits),
                    ("traitCapabilities", strings("traitCapabilities") == entry.trait_capabilities),
                ];
                for (field, ok) in checks {
                    if !ok {
                        let want_field = want.get(field).map(Value::to_string).unwrap_or_default();
                        let got = match field {
                            "traits" => serde_json::to_string(&entry.traits).unwrap(),
                            "connectableEvents" => serde_json::to_string(&entry.connectable_events).unwrap(),
                            "traitCapabilities" => serde_json::to_string(&entry.trait_capabilities).unwrap(),
                            "description" => entry.description.clone(),
                            "defaultEntityName" => entry.default_entity_name.clone(),
                            _ => String::new(),
                        };
                        let differs = |a: &str, b: &str| -> String {
                            let (a, b): (Vec<String>, Vec<String>) = (
                                serde_json::from_str(a).unwrap_or_default(),
                                serde_json::from_str(b).unwrap_or_default(),
                            );
                            format!(
                                "only in the registry {:?}, only derived {:?}",
                                a.iter().filter(|x| !b.contains(x)).collect::<Vec<_>>(),
                                b.iter().filter(|x| !a.contains(x)).collect::<Vec<_>>()
                            )
                        };
                        let (want_field, got) = if matches!(field, "traits" | "connectableEvents" | "traitCapabilities") {
                            (differs(&want_field, &got), String::new())
                        } else {
                            (want_field, got)
                        };
                        mismatches.push(format!("{prefix}/{}: {field}: want {want_field}, got {got}", entry.name));
                    }
                }
                checked += 1;
            }
            assert!(mismatches.is_empty(), "differs from the retired registry: {mismatches:#?}");
        }
        eprintln!("compared {checked} behaviors, {unloadable} skipped");
        assert!(checked > 0 && unloadable * 4 <= checked, "most of the sample must be comparable");
    }

    fn facts(orb: &Value, others: &[(&str, Value)]) -> Option<ProgramFacts> {
        let others: HashMap<String, Rc<Value>> = others
            .iter()
            .map(|(n, v)| (n.to_string(), Rc::new(v.clone())))
            .collect();
        program_facts(orb, &|name| others.get(name).cloned())
    }

    fn machine(states: &[&str], events: &[&str]) -> Value {
        json!({
            "states": states.iter().map(|s| json!({"name": s})).collect::<Vec<_>>(),
            "events": events.iter().map(|e| json!({"key": e})).collect::<Vec<_>>(),
            "transitions": []
        })
    }

    #[test]
    fn a_local_machine_gives_sorted_states_and_connectable_events_without_init() {
        let orb = json!({
            "description": "d",
            "orbitals": [{
                "name": "O",
                "entity": {"name": "Note", "fields": []},
                "traits": [{"name": "T", "stateMachine": machine(&["idle", "busy"], &["INIT", "GO", "STOP", "GO"])}]
            }]
        });
        let f = facts(&orb, &[]).unwrap();
        assert_eq!(f.state_pattern, "busy, idle");
        assert_eq!(f.connectable_events, ["GO", "STOP"]);
        assert_eq!(f.default_entity_name, "Note");
        assert_eq!(f.traits, ["T"]);
        assert_eq!(f.description, "d");
    }

    #[test]
    fn only_the_first_machine_of_an_orbital_counts_and_each_orbital_adds_its_own() {
        let orb = json!({"orbitals": [
            {"name": "A", "traits": [
                {"name": "T1", "stateMachine": machine(&["a"], &["ONE"])},
                {"name": "T2", "stateMachine": machine(&["b"], &["TWO"])}
            ]},
            {"name": "B", "traits": [{"name": "T3", "stateMachine": machine(&["c"], &["THREE"])}]}
        ]});
        let f = facts(&orb, &[]).unwrap();
        assert_eq!(f.connectable_events, ["ONE", "THREE"]);
        assert_eq!(f.state_pattern, "a, c");
        assert_eq!(f.traits, ["T1", "T2", "T3"]);
    }

    #[test]
    fn a_composed_trait_runs_the_machine_of_the_trait_it_names() {
        let orb = json!({"orbitals": [{
            "name": "O",
            "uses": [{"from": "std/behaviors/std-base", "as": "Base"}],
            "traits": [{"ref": "Base.traits.Core", "name": "Mine"}]
        }]});
        let base = json!({"orbitals": [{"name": "BaseOrbital", "traits": [
            {"name": "Other", "stateMachine": machine(&["x"], &["NO"])},
            {"name": "Core", "stateMachine": machine(&["on", "off"], &["TOGGLE", "INIT"])}
        ]}]});
        let f = facts(&orb, &[("std-base", base.clone())]).unwrap();
        assert_eq!(f.connectable_events, ["TOGGLE"]);
        assert_eq!(f.state_pattern, "off, on");
        assert_eq!(f.traits, ["Mine"]);
        // Control: a target the lookup cannot find contributes no machine.
        let missing = facts(&orb, &[]).unwrap();
        assert!(missing.connectable_events.is_empty());
        assert_eq!(missing.state_pattern, "");
    }

    #[test]
    fn a_bare_trait_name_runs_the_sibling_trait_it_names_and_a_ref_cycle_ends() {
        let orb = json!({"orbitals": [{
            "name": "O",
            "traits": ["Local", {"name": "Local", "stateMachine": machine(&["s"], &["EV"])}]
        }]});
        assert_eq!(facts(&orb, &[]).unwrap().connectable_events, ["EV"]);
        let cyc = json!({"orbitals": [{
            "name": "O",
            "uses": [{"from": "std-loop", "as": "L"}],
            "traits": [{"ref": "L.traits.T", "name": "T"}]
        }]});
        let looped = json!({"orbitals": [{
            "name": "P",
            "uses": [{"from": "std-loop", "as": "L"}],
            "traits": [{"name": "T", "ref": "L.traits.T"}]
        }]});
        assert!(facts(&cyc, &[("std-loop", looped)]).unwrap().connectable_events.is_empty());
    }

    #[test]
    fn the_entity_name_follows_the_orbital_the_entity_refers_to() {
        let upstream = json!({"orbitals": [{"name": "Up", "entity": {"name": "Upstream"}}]});
        let importing = json!({"orbitals": [{
            "name": "Mine",
            "uses": [{"from": "std/behaviors/std-up", "as": "U"}],
            "reference": {"ref": "U.orbitals.Up"},
            "entity": {"extends": "U.orbitals.Up.entity", "name": "Local"},
            "traits": []
        }]});
        assert_eq!(
            facts(&importing, &[("std-up", upstream.clone())]).unwrap().default_entity_name,
            "Upstream"
        );
        // Control: the same orbital with its upstream unreachable falls back to its own name, then `Item`.
        assert_eq!(facts(&importing, &[]).unwrap().default_entity_name, "Local");
        let by_string = json!({"orbitals": [{"name": "O", "entity": "U.orbitals.Up.entity"}]});
        assert_eq!(facts(&by_string, &[]).unwrap().default_entity_name, "Item");
        let none = json!({"orbitals": [{"name": "O"}]});
        assert_eq!(facts(&none, &[]).unwrap().default_entity_name, "Item");
        let nameless = json!({"orbitals": [{"name": "O", "entity": {"name": ""}}]});
        assert_eq!(facts(&nameless, &[]).unwrap().default_entity_name, "Item");
    }

    #[test]
    fn an_imported_orbital_signs_its_surviving_traits_under_the_importing_orbitals_name() {
        let upstream = json!({"orbitals": [{"name": "Up", "traits": ["Keep", {"name": "Drop"}, {"ref": "x.traits.Y"}]}]});
        let make = |reference: Value| {
            json!({"orbitals": [{
                "name": "Mine",
                "uses": [{"from": "std-up.orb", "as": "U"}],
                "reference": reference,
                "traits": ["Own"]
            }]})
        };
        let omit = make(json!({"ref": "U.orbitals.Up", "omit": ["Drop"]}));
        assert_eq!(
            facts(&omit, &[("std-up", upstream.clone())]).unwrap().traits,
            ["Own", "MineKeep", "Minex.traits.Y"]
        );
        let only = make(json!({"ref": "U.orbitals.Up", "only": ["Drop"]}));
        assert_eq!(
            facts(&only, &[("std-up", upstream.clone())]).unwrap().traits,
            ["Own", "MineDrop"]
        );
        let wrong_kind = make(json!({"ref": "U.traits.Up"}));
        assert_eq!(facts(&wrong_kind, &[("std-up", upstream)]).unwrap().traits, ["Own"]);
    }

    #[test]
    fn a_program_with_no_orbital_is_not_indexed_and_trait_names_dedupe_across_orbitals() {
        assert!(facts(&json!({"orbitals": []}), &[]).is_none());
        assert!(facts(&json!({}), &[]).is_none());
        let orb = json!({"orbitals": [
            {"name": "A", "traits": [{"name": "Shared"}]},
            {"name": "B", "traits": [{"name": "Shared"}, {"name": "Own"}]}
        ]});
        assert_eq!(facts(&orb, &[]).unwrap().traits, ["Shared", "Own"]);
    }
}
