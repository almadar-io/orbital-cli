//! The rows a catalog lists: one per behavior an installed package ships, derived on read from its tree, its `.lolo`
//! header tags and its lowered program. No cache lives here: whether a listing is cached, and under what key, is std's.

use orbital_compiler::loader::{ExternalOrbitalLoader, ImportChain, LoaderConfig};
use orbital_core::behavior_index::{
    build_description, hand_authored_behaviors, header_tags, indexed_files, lolo_path_of, resolve_exposure, IndexedFile, PackageSource,
};
use orbital_core::behavior_packages::{index_registry_source, BehaviorPackage};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const WORKERS: usize = 8;
const WORKER_STACK: usize = 8 * 1024 * 1024;
const MAX_IMPORT_DEPTH: usize = 16;
const RELOAD_EVERY: usize = 48;

/// One shipped behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BehaviorRow {
    pub(crate) name: String,
    pub(crate) dirs: Vec<String>,
    pub(crate) level: String,
    pub(crate) topic: String,
    pub(crate) exposure: Option<String>,
    pub(crate) description: String,
    pub(crate) capabilities: Option<String>,
    pub(crate) pattern: Option<String>,
    pub(crate) traits: Vec<String>,
}

type Lookup<'a> = &'a dyn Fn(&str) -> Option<Rc<Value>>;

struct ProgramFacts {
    description: String,
    state_pattern: String,
    traits: Vec<String>,
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn local_machine(t: &Value) -> Option<&Value> {
    t.get("stateMachine").filter(|m| m.is_object())
}

fn basename(from: &str) -> &str {
    from.trim_end_matches('/').rsplit('/').next().unwrap_or(from)
}

/// `<Alias>.<kind>.<Name>` split into its three parts, the alias resolved through the orbital's `uses` to the file it imports.
fn dotted_ref<'a>(path: &'a str, kind: &str, orbital: &'a Value) -> Option<(&'a str, &'a str)> {
    let mut parts = path.split('.');
    let (alias, found, name) = (parts.next()?, parts.next()?, parts.next()?);
    if found != kind || alias.is_empty() || name.is_empty() {
        return None;
    }
    let from = array(orbital, "uses")
        .iter()
        .find(|u| str_of(u, "as") == Some(alias))
        .and_then(|u| str_of(u, "from"))?;
    Some((from, name))
}

/// The state machine a trait runs: its own, else the composed trait's, following the chain through each target's first orbital.
fn trait_machine(t: &Value, orbital: &Value, seen: &mut HashSet<String>, lookup: Lookup) -> Option<Value> {
    if let Some(m) = local_machine(t) {
        return Some(m.clone());
    }
    let (from, name) = dotted_ref(str_of(t, "ref")?, "traits", orbital)?;
    let behavior = basename(from);
    if !seen.insert(format!("{behavior}::{name}")) {
        return None;
    }
    let target = lookup(behavior)?;
    let first = array(&target, "orbitals").first()?;
    let next = array(first, "traits")
        .iter()
        .find(|x| x.is_object() && str_of(x, "name") == Some(name))?;
    trait_machine(next, first, seen, lookup)
}

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

/// The one machine an orbital contributes: its first trait's own, else the first that resolves to one.
fn orbital_machine(orbital: &Value, lookup: Lookup) -> Option<Value> {
    let traits = array(orbital, "traits");
    traits
        .iter()
        .filter(|t| t.is_object())
        .find_map(local_machine)
        .cloned()
        .or_else(|| traits.iter().find_map(|t| raw_trait_machine(t, orbital, lookup)))
}

/// The orbital an imported orbital stands for (`reference.ref = Alias.orbitals.Name`): its program and position.
fn import_target(orbital: &Value, lookup: Lookup) -> Option<(Rc<Value>, usize)> {
    let (from, name) = dotted_ref(str_of(orbital.get("reference")?, "ref")?, "orbitals", orbital)?;
    let base = basename(from);
    let target = lookup(base.strip_suffix(".orb").unwrap_or(base))?;
    let at = array(&target, "orbitals").iter().position(|o| str_of(o, "name") == Some(name))?;
    Some((target, at))
}

/// The traits an orbital signs: its own, then the imported orbital's that survive `only`/`omit`, prefixed by the orbital's name.
fn signature_traits(orbital: &Value, lookup: Lookup, depth: usize) -> Vec<String> {
    let mut names: Vec<String> = array(orbital, "traits")
        .iter()
        .map(|t| match t {
            Value::String(s) => s.clone(),
            other => str_of(other, "name").or_else(|| str_of(other, "ref")).unwrap_or("").to_string(),
        })
        .collect();
    if depth >= MAX_IMPORT_DEPTH {
        return names;
    }
    if let Some((target, at)) = import_target(orbital, lookup) {
        let own_name = str_of(orbital, "name").unwrap_or("");
        let reference = &orbital["reference"];
        let only: Option<Vec<&str>> = reference
            .get("only")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect());
        let omit: Vec<&str> = array(reference, "omit").iter().filter_map(Value::as_str).collect();
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
    names
}

/// The facts a lowered program yields; `None` when it has no orbital.
fn program_facts(orb: &Value, lookup: Lookup) -> Option<ProgramFacts> {
    let orbitals = array(orb, "orbitals");
    orbitals.first()?;
    let machines: Vec<Value> = orbitals.iter().filter_map(|o| orbital_machine(o, lookup)).collect();
    let mut states: Vec<&str> = machines
        .iter()
        .flat_map(|m| array(m, "states"))
        .filter_map(|s| str_of(s, "name"))
        .collect();
    states.sort_unstable();
    let mut seen = HashSet::new();
    let traits: Vec<String> = orbitals
        .iter()
        .flat_map(|o| signature_traits(o, lookup, 0))
        .filter(|t| seen.insert(t.clone()))
        .collect();
    Some(ProgramFacts {
        description: str_of(orb, "description").unwrap_or("").to_string(),
        state_pattern: states.join(", "),
        traits,
    })
}

struct Programs<'a> {
    packages: &'a [BehaviorPackage],
    owners: &'a HashMap<String, usize>,
    loader: ExternalOrbitalLoader,
    cache: HashMap<String, Rc<Value>>,
}

impl<'a> Programs<'a> {
    fn new(packages: &'a [BehaviorPackage], owners: &'a HashMap<String, usize>) -> anyhow::Result<Self> {
        let loader = ExternalOrbitalLoader::new(LoaderConfig {
            base_path: std::env::current_dir()?,
            packages: packages.to_vec(),
            ..Default::default()
        });
        Ok(Self {
            packages,
            owners,
            loader,
            cache: HashMap::new(),
        })
    }

    fn orb_of(&mut self, name: &str) -> anyhow::Result<Option<Rc<Value>>> {
        let Some(&owner) = self.owners.get(name) else { return Ok(None) };
        let prefix = self.packages[owner].prefix.clone();
        self.orb_in(&prefix, name).map(Some)
    }

    fn orb_in(&mut self, prefix: &str, name: &str) -> anyhow::Result<Rc<Value>> {
        let spec = format!("{prefix}/{name}");
        if let Some(hit) = self.cache.get(&spec) {
            return Ok(hit.clone());
        }
        let loaded = self
            .loader
            .load(&spec, None, &mut ImportChain::new())
            .map_err(|e| anyhow::anyhow!("cannot load `{spec}`: {e}"))?;
        let value = Rc::new(serde_json::to_value(&loaded.schema)?);
        self.cache.insert(spec, value.clone());
        Ok(value)
    }
}

fn owners_of(packages: &[BehaviorPackage]) -> anyhow::Result<HashMap<String, usize>> {
    let mut owners = HashMap::new();
    for (i, pkg) in packages.iter().enumerate() {
        for name in index_registry_source(&pkg.registry)?.into_keys() {
            owners.entry(name).or_insert(i);
        }
    }
    Ok(owners)
}

fn row_of(
    pkg: &BehaviorPackage,
    file: &IndexedFile,
    hand_authored: &HashSet<String>,
    programs: &std::cell::RefCell<Programs>,
) -> anyhow::Result<Option<BehaviorRow>> {
    let orb = programs.borrow_mut().orb_in(&pkg.prefix, &file.name)?;
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
    let Some(facts) = facts else { return Ok(None) };
    let lolo = match lolo_path_of(pkg, file) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => anyhow::bail!("failed to read {}: {e}", path.display()),
        },
        None => None,
    };
    let tags = match &lolo {
        Some(text) => header_tags(text).map_err(|e| anyhow::anyhow!("{}/{}: {e}", pkg.prefix, file.name))?,
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
    let description = build_description(&file.name, &facts.description, lolo.as_deref(), level, &topic, &facts.state_pattern);
    Ok(Some(BehaviorRow {
        name: file.name.clone(),
        dirs: file.dirs.clone(),
        level: level.as_str().to_string(),
        topic,
        exposure,
        description,
        capabilities: tags.capabilities,
        pattern: tags.pattern,
        traits: facts.traits,
    }))
}

/// A package's rows, and each behavior that could not be derived with why.
#[derive(Default)]
pub(crate) struct PackageRows {
    pub(crate) rows: Vec<BehaviorRow>,
    pub(crate) skipped: Vec<(String, String)>,
}

/// Every behavior `packages[pi]` ships, in path order; a behavior that does not load is skipped and listed.
pub(crate) fn package_rows(packages: &[BehaviorPackage], pi: usize) -> anyhow::Result<PackageRows> {
    let pkg = &packages[pi];
    let files: Vec<IndexedFile> = indexed_files(pkg)?;
    let hand_authored = match &pkg.lolo_source {
        Some(root) => hand_authored_behaviors(root).map_err(|e| anyhow::anyhow!(e))?,
        None => HashSet::new(),
    };
    let owners = owners_of(packages)?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    type Slot = std::sync::Mutex<Option<anyhow::Result<Option<BehaviorRow>>>>;
    let slots: Vec<Slot> = files.iter().map(|_| Slot::default()).collect();
    let spawned = std::thread::scope(|scope| -> anyhow::Result<()> {
        for _ in 0..WORKERS.min(files.len()) {
            std::thread::Builder::new().stack_size(WORKER_STACK).spawn_scoped(scope, || {
                let Ok(mut fresh) = Programs::new(packages, &owners) else { return };
                let mut done = 0usize;
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(file) = files.get(i) else { break };
                    if done > 0 && done % RELOAD_EVERY == 0 {
                        match Programs::new(packages, &owners) {
                            Ok(p) => fresh = p,
                            Err(_) => return,
                        }
                    }
                    done += 1;
                    let cell = std::cell::RefCell::new(fresh);
                    let row = row_of(pkg, file, &hand_authored, &cell);
                    fresh = cell.into_inner();
                    if let Ok(mut slot) = slots[i].lock() {
                        *slot = Some(row);
                    }
                }
            })?;
        }
        Ok(())
    });
    spawned?;
    let mut out = PackageRows::default();
    for (slot, file) in slots.into_iter().zip(&files) {
        let row = slot
            .into_inner()
            .map_err(|_| anyhow::anyhow!("an index worker panicked"))?
            .ok_or_else(|| anyhow::anyhow!("{}/{} was never read: its worker could not start", pkg.prefix, file.name))?;
        match row {
            Ok(row) => out.rows.extend(row),
            Err(e) => out.skipped.push((file.name.clone(), format!("{e:#}"))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(orb: &Value, others: &[(&str, Value)]) -> Option<ProgramFacts> {
        let others: HashMap<String, Rc<Value>> = others.iter().map(|(n, v)| (n.to_string(), Rc::new(v.clone()))).collect();
        program_facts(orb, &|name| others.get(name).cloned())
    }

    fn machine(states: &[&str]) -> Value {
        json!({ "states": states.iter().map(|s| json!({"name": s})).collect::<Vec<_>>(), "transitions": [] })
    }

    #[test]
    fn a_local_machine_gives_sorted_states_and_the_trait_names() {
        let orb =
            json!({"description": "d", "orbitals": [{"name": "O", "traits": [{"name": "T", "stateMachine": machine(&["idle", "busy"])}]}]});
        let f = facts(&orb, &[]).unwrap();
        assert_eq!(f.state_pattern, "busy, idle");
        assert_eq!(f.traits, ["T"]);
        assert_eq!(f.description, "d");
    }

    #[test]
    fn a_composed_trait_runs_the_machine_of_the_trait_it_names_and_a_missing_target_adds_none() {
        let orb = json!({"orbitals": [{"name": "O", "uses": [{"from": "std/behaviors/std-base", "as": "Base"}], "traits": [{"ref": "Base.traits.Core", "name": "Mine"}]}]});
        let base = json!({"orbitals": [{"name": "B", "traits": [{"name": "Core", "stateMachine": machine(&["on", "off"])}]}]});
        assert_eq!(facts(&orb, &[("std-base", base)]).unwrap().state_pattern, "off, on");
        assert_eq!(facts(&orb, &[]).unwrap().state_pattern, "");
    }

    #[test]
    fn an_imported_orbital_signs_its_surviving_traits_under_the_importing_orbitals_name() {
        let upstream = json!({"orbitals": [{"name": "Up", "traits": ["Keep", {"name": "Drop"}]}]});
        let make = |reference: Value| json!({"orbitals": [{"name": "Mine", "uses": [{"from": "std-up.orb", "as": "U"}], "reference": reference, "traits": ["Own"]}]});
        let omit = make(json!({"ref": "U.orbitals.Up", "omit": ["Drop"]}));
        assert_eq!(facts(&omit, &[("std-up", upstream.clone())]).unwrap().traits, ["Own", "MineKeep"]);
        let only = make(json!({"ref": "U.orbitals.Up", "only": ["Drop"]}));
        assert_eq!(facts(&only, &[("std-up", upstream)]).unwrap().traits, ["Own", "MineDrop"]);
    }

    #[test]
    fn a_program_with_no_orbital_has_no_facts_and_trait_names_dedupe_across_orbitals() {
        assert!(facts(&json!({"orbitals": []}), &[]).is_none());
        let orb = json!({"orbitals": [{"name": "A", "traits": [{"name": "Shared"}]}, {"name": "B", "traits": [{"name": "Shared"}, {"name": "Own"}]}]});
        assert_eq!(facts(&orb, &[]).unwrap().traits, ["Shared", "Own"]);
    }
}
