//! `orb behaviors catalog | describe | source` — the installed behavior
//! packages as data. Resolution is the existing machinery (`discover` for the
//! package set, `ExternalOrbitalLoader` for specifiers); this module only
//! derives each package's index on read (`behaviors_index`), reads the vector
//! snapshot it declares (`orb.vectors`) and shapes them to the `@almadar/core`
//! `CatalogEntrySchema` / `TraitDescriptionSchema` contracts.

use orbital_compiler::loader::{ExternalOrbitalLoader, ImportChain, LoaderConfig, LoaderError};
use orbital_core::behavior_index::{header_tags, indexed_file_at, lolo_path_of, EXPOSURES};
use orbital_core::behavior_packages::{BehaviorPackage, STD_PACKAGE};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::behaviors_index::{package_index, IndexEntry};
use super::behaviors_texts::{
    describe_all, index_texts, texts_of, EntityFacts, KnobFacts, OrbitalFacts, RegistryEntry,
    TextKind, TraitFacts, CAPABILITY_KEY_SUFFIX,
};
use super::behaviors_vectors::{
    lookup, probe, Embedder, ProviderEmbedder, Sources, Wanted, CANONICAL_MODEL,
};
use super::npm_project::discover_from;
use super::sidecar_fetch::UreqFetcher;

const TRAITS_SEGMENT: &str = ".traits.";
const STD_ALIAS: &str = "std";
// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TraitValueOut {
    behavior: String,
    #[serde(rename = "trait")]
    trait_name: String,
}

#[derive(Serialize)]
struct BehaviorValueOut {
    behavior: String,
}

#[derive(Serialize)]
pub(crate) struct CatalogEntryOut {
    behavior: String,
    package: String,
    topic: String,
    level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exposure: Option<String>,
    /// The `@almadar/ui` pattern the behavior is the factory of; absent when it is not one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
    description: String,
    capabilities: Vec<String>,
    traits: Vec<TraitValueOut>,
    /// The behavior as a value ready to embed or store (`behavior` typed).
    value: BehaviorValueOut,
    vectors: BTreeMap<String, Vec<f64>>,
    model: String,
}

struct Scope {
    package: usize,
    dirs: Vec<String>,
}

fn resolve_scope(packages: &[BehaviorPackage], raw: &str) -> anyhow::Result<Scope> {
    let mut parts = raw.split('/').filter(|p| !p.is_empty());
    let head = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty scope path"))?;
    let package = packages
        .iter()
        .position(|p| {
            if head == STD_ALIAS {
                p.name == STD_PACKAGE
            } else {
                p.prefix == head
            }
        })
        .ok_or_else(|| {
            let installed: Vec<&str> = packages.iter().map(|p| p.prefix.as_str()).collect();
            anyhow::anyhow!(
                "scope `{raw}` names no installed behavior package prefix (installed: {})",
                installed.join(", ")
            )
        })?;
    Ok(Scope {
        package,
        dirs: parts.map(str::to_string).collect(),
    })
}

/// The phrases of an entry's `@capabilities`, the organism's comma-separated phrase string (the same
/// string the embedding text carries).
fn capability_phrases(declared: Option<&str>) -> Vec<String> {
    declared
        .into_iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Which vectors a catalog carries.
pub(crate) enum Vectors<'a> {
    /// None: the entries carry no `vectors` and no `model`.
    Omit,
    /// The stored ones; a text with none is embedded by this embedder, or is an error when `None`.
    Attach(Option<&'a dyn Embedder>),
}

pub(crate) fn run_catalog(
    paths: Vec<String>,
    exposure: Vec<String>,
    json: bool,
    no_vectors: bool,
) -> anyhow::Result<()> {
    let out = if no_vectors {
        catalog_with(&paths, &exposure, Vectors::Omit, None)?
    } else {
        catalog(&paths, &exposure)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for e in out {
            println!(
                "{:<48} {:<9} {:<10} {}",
                e.behavior,
                e.level,
                e.exposure.as_deref().unwrap_or("-"),
                e.description
            );
        }
    }
    Ok(())
}

/// The installed behaviors under `paths` — what `orb behaviors catalog` prints: vectors attached,
/// missing ones embedded through the configured provider.
pub(crate) fn catalog(
    paths: &[String],
    exposure: &[String],
) -> anyhow::Result<Vec<CatalogEntryOut>> {
    let embedder = ProviderEmbedder::from_env();
    catalog_with(
        paths,
        exposure,
        Vectors::Attach(embedder.as_ref().map(|e| e as &dyn Embedder)),
        None,
    )
}

/// [`catalog`] with the vectors chosen; `user_dir` replaces `~/.almadar/vectors`.
pub(crate) fn catalog_with(
    paths: &[String],
    exposure: &[String],
    vectors: Vectors<'_>,
    user_dir: Option<&Path>,
) -> anyhow::Result<Vec<CatalogEntryOut>> {
    if paths.is_empty() {
        anyhow::bail!("give at least one scope path, e.g. `almadar-std/ui/core/molecules`");
    }
    for e in exposure {
        if !EXPOSURES.contains(&e.as_str()) {
            anyhow::bail!("unknown exposure `{e}` (one of {})", EXPOSURES.join(", "));
        }
    }
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let scopes: Vec<Scope> = paths
        .iter()
        .map(|p| resolve_scope(&packages, p))
        .collect::<anyhow::Result<_>>()?;

    let mut entries: BTreeMap<String, CatalogEntryOut> = BTreeMap::new();
    let mut contributing: BTreeSet<usize> = BTreeSet::new();
    let mut wanted: Vec<Wanted> = Vec::new();
    let mut per_package: BTreeMap<usize, Vec<IndexEntry>> = BTreeMap::new();
    let mut indexed: BTreeMap<usize, Vec<IndexEntry>> = BTreeMap::new();
    for scope in &scopes {
        if !indexed.contains_key(&scope.package) {
            let index = package_index(&packages, scope.package)?;
            for skip in &index.skipped {
                tracing::warn!(target: orbital_core::logging::ns::CLI, "skipped {}/{}: {}", packages[scope.package].prefix, skip.name, skip.error);
            }
            indexed.insert(scope.package, index.entries);
        }
        for entry in &indexed[&scope.package] {
            if in_scope(&entry.dirs, &scope.dirs) {
                per_package.entry(scope.package).or_default().push(entry.clone());
            }
        }
    }

    for (pi, rows) in &per_package {
        let pkg = &packages[*pi];
        for entry in rows {
            let name = &entry.name;
            if !exposure.is_empty()
                && !entry
                    .exposure
                    .as_deref()
                    .is_some_and(|e| exposure.iter().any(|x| x == e))
            {
                continue;
            }
            let key = format!("{}/{name}", pkg.prefix);
            let traits = entry
                .traits
                .iter()
                .map(|n| TraitValueOut {
                    behavior: key.clone(),
                    trait_name: n.clone(),
                })
                .collect();
            contributing.insert(*pi);
            if matches!(vectors, Vectors::Attach(_)) {
                let (organism, capability) = index_texts(entry);
                wanted.push(Wanted {
                    key: key.clone(),
                    text: organism,
                });
                if let Some(text) = capability {
                    wanted.push(Wanted {
                        key: format!("{key}{CAPABILITY_KEY_SUFFIX}"),
                        text,
                    });
                }
            }
            entries.insert(
                key.clone(),
                CatalogEntryOut {
                    value: BehaviorValueOut {
                        behavior: key.clone(),
                    },
                    behavior: key,
                    package: pkg.name.clone(),
                    topic: entry.topic.clone(),
                    level: entry.level.clone(),
                    exposure: entry.exposure.clone(),
                    pattern: entry.pattern.clone(),
                    description: entry.description.clone(),
                    capabilities: capability_phrases(entry.capabilities.as_deref()),
                    traits,
                    vectors: BTreeMap::new(),
                    model: String::new(),
                },
            );
        }
    }

    if let Vectors::Attach(embedder) = vectors {
        let mut sources =
            Sources::for_packages(contributing.iter().map(|pi| &packages[*pi]), &UreqFetcher)?;
        if let Some(dir) = user_dir {
            sources.user_dir = dir.to_path_buf();
        }
        // The index text carries the resolved trait capabilities, so it equals the embedding bake's
        // text unless the index is stale. A text with no stored vector is rebuilt from a full
        // describe of its behavior before it is embedded, so a vector is only ever keyed by the
        // text the embedding bake produces.
        let fallback_model = embedder.map_or(CANONICAL_MODEL, |e| e.model());
        let (_, probed) = probe(&sources, &wanted, fallback_model)?;
        let unresolved: HashSet<&str> = wanted
            .iter()
            .filter(|w| !probed.contains_key(&w.key) && !w.key.ends_with(CAPABILITY_KEY_SUFFIX))
            .map(|w| w.key.as_str())
            .collect();
        if !unresolved.is_empty() {
            let mut names: Vec<&str> = unresolved.iter().copied().collect();
            names.sort_unstable();
            eprintln!(
                "behaviors catalog: {} organism text(s) missed the vector store (stale index or live edit); rebuilding them from a describe: {}",
                names.len(),
                names.join(", ")
            );
            let mut exact: HashMap<String, String> = HashMap::new();
            for (pi, rows) in &per_package {
                let prefix = &packages[*pi].prefix;
                let registry: Vec<(String, RegistryEntry)> = rows
                    .iter()
                    .filter(|e| unresolved.contains(format!("{prefix}/{}", e.name).as_str()))
                    .map(|e| (e.name.clone(), RegistryEntry::from(e)))
                    .collect();
                if registry.is_empty() {
                    continue;
                }
                let facts = describe_all(prefix, &registry, Some(TextKind::Organism))?;
                for t in texts_of(&registry, &facts, Some(TextKind::Organism)) {
                    exact.insert(format!("{prefix}/{}", t.key), t.text);
                }
            }
            for w in &mut wanted {
                if let Some(text) = exact.remove(&w.key) {
                    w.text = text;
                }
            }
        }
        let (model, found) = lookup(&sources, &wanted, embedder)?;
        for entry in entries.values_mut() {
            for (facet, key) in [
                ("description", entry.behavior.clone()),
                (
                    "capabilities",
                    format!("{}{CAPABILITY_KEY_SUFFIX}", entry.behavior),
                ),
            ] {
                if let Some(v) = found.get(&key) {
                    entry.vectors.insert(facet.to_string(), v.clone());
                }
            }
            entry.model = model.clone();
        }
    }

    Ok(entries.into_values().collect())
}

// ---------------------------------------------------------------------------
// describe / source
// ---------------------------------------------------------------------------

/// `(behavior specifier, trait name)` from `<spec>.traits.<Trait>`.
fn split_trait_spec(spec: &str) -> anyhow::Result<(String, String)> {
    let at = spec
        .rfind(TRAITS_SEGMENT)
        .filter(|&i| i > 0)
        .ok_or_else(|| anyhow::anyhow!("`{spec}` is not `<behavior>{TRAITS_SEGMENT}<Trait>`"))?;
    let trait_name = &spec[at + TRAITS_SEGMENT.len()..];
    if trait_name.is_empty() {
        anyhow::bail!("`{spec}` names no trait after `{TRAITS_SEGMENT}`");
    }
    Ok((spec[..at].to_string(), trait_name.to_string()))
}

pub(crate) struct Resolved {
    path: PathBuf,
    package: Option<BehaviorPackage>,
    schema: orbital_core::schema::OrbitalSchema,
}

fn resolve(spec: &str) -> anyhow::Result<Resolved> {
    resolve_from(spec, None)
}

/// [`resolve`], a relative specifier resolving from the program file `from` (as its `uses` do).
fn resolve_from(spec: &str, from: Option<&Path>) -> anyhow::Result<Resolved> {
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let mut loader = ExternalOrbitalLoader::new(LoaderConfig {
        base_path: cwd,
        packages: packages.clone(),
        ..Default::default()
    });
    resolve_with_loader(&mut loader, &packages, spec, from)
}

/// [`resolve_from`] on a loader the caller keeps, so a batch reads each program once.
pub(crate) fn resolve_with_loader(
    loader: &mut ExternalOrbitalLoader,
    packages: &[BehaviorPackage],
    spec: &str,
    from: Option<&Path>,
) -> anyhow::Result<Resolved> {
    let cwd = std::env::current_dir()?;
    let from = from.map(|f| cwd.join(f));
    let from = from.as_deref();
    let mut chain = orbital_compiler::loader::ImportChain::new();
    let loaded = loader
        .load(spec, from, &mut chain)
        .map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))?;
    let path = loaded.source_path.clone();
    let schema = loaded.schema.clone();
    let package = packages
        .iter()
        .find(|p| path.starts_with(p.registry.root()))
        .cloned();
    Ok(Resolved {
        path,
        package,
        schema,
    })
}

/// A behavior one `uses` names, as the loader resolved it: its package (absent for a local file), its `@pattern`
/// header tag, the file it was read from and its program.
pub(crate) struct ImportedBehavior {
    pub(crate) package: Option<BehaviorPackage>,
    pub(crate) pattern: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) program: Value,
}

/// Resolve one `uses` specifier through `loader`, from the importing file `from`, reading that behavior alone;
/// `None` when no file answers the specifier.
pub(crate) fn resolve_import(
    loader: &mut ExternalOrbitalLoader,
    packages: &[BehaviorPackage],
    spec: &str,
    from: Option<&Path>,
) -> anyhow::Result<Option<ImportedBehavior>> {
    let mut chain = ImportChain::new();
    let loaded = match loader.load(spec, from, &mut chain) {
        Ok(loaded) => loaded,
        Err(LoaderError::FileNotFound(_)) => return Ok(None),
        Err(e) => anyhow::bail!("cannot resolve `{spec}`: {e}"),
    };
    let resolved = Resolved {
        path: loaded.source_path.clone(),
        package: packages
            .iter()
            .find(|p| loaded.source_path.starts_with(p.registry.root()))
            .cloned(),
        schema: loaded.schema.clone(),
    };
    let pattern = match &resolved.package {
        Some(pkg) => {
            let name = resolved
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let file = indexed_file_at(pkg, name, resolved.path.clone());
            match lolo_path_of(pkg, &file).map(|p| (std::fs::read_to_string(&p), p)) {
                Some((Ok(text), _)) => header_tags(&text).map_err(|e| anyhow::anyhow!(e))?.pattern,
                Some((Err(e), _)) if e.kind() == std::io::ErrorKind::NotFound => None,
                Some((Err(e), p)) => anyhow::bail!("failed to read {}: {e}", p.display()),
                None => None,
            }
        }
        None => None,
    };
    Ok(Some(ImportedBehavior {
        program: source_value(&resolved)?,
        package: resolved.package,
        pattern,
        path: resolved.path,
    }))
}

#[derive(Serialize)]
struct KnobOut {
    key: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f64>>,
}

#[derive(Serialize)]
struct EventsOut {
    emits: Vec<String>,
    listens: Vec<String>,
    transitions: Vec<String>,
    /// Events whose `emits`/`listens` declaration carries `@tier "domain"`; absent when the source has no declarations.
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<Vec<String>>,
}

fn domain_events(td: &orbital_core::schema::TraitDefinition) -> Vec<String> {
    let mut seen = BTreeSet::new();
    td.emits
        .iter()
        .flatten()
        .map(|e| (&e.event, &e.tier))
        .chain(td.listens.iter().flatten().map(|l| (&l.event, &l.tier)))
        .filter(|(_, tier)| tier.as_deref() == Some("domain"))
        .filter(|(event, _)| seen.insert((*event).clone()))
        .map(|(event, _)| event.clone())
        .collect()
}

/// A trait's state machine as the resolved program runs it (`TraitMachineSchema`).
#[derive(Serialize)]
struct MachineOut {
    #[serde(rename = "linkedEntity", skip_serializing_if = "Option::is_none")]
    linked_entity: Option<String>,
    #[serde(rename = "stateMachine")]
    state_machine: orbital_core::schema::StateMachine,
}

#[derive(Serialize)]
struct DescriptionOut {
    value: Value,
    knobs: Vec<KnobOut>,
    events: EventsOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine: Option<MachineOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<orbital_lolo::unlower::PrintedTrait>,
    model: String,
}

/// A behavior as the runtime runs it: lazy behaviors split, every import and composed trait inlined.
fn resolved_program(
    r: &Resolved,
    spec: &str,
) -> anyhow::Result<orbital_core::schema::OrbitalSchema> {
    use orbital_compiler::{resolve_schema_with_path, CompilerOptions};
    let options = CompilerOptions::from_env();
    let mut schema = r.schema.clone();
    orbital_compiler::phases::lazy::split_lazy_and_resolve(&mut schema, &options, Some(&r.path))
        .map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))?;
    resolve_schema_with_path(schema, &options, Some(&r.path))
        .map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))
}

/// The orbital that holds `trait_name` in a resolved program.
fn trait_orbital<'a>(
    program: &'a orbital_core::schema::OrbitalSchema,
    trait_name: &str,
) -> Option<&'a str> {
    use orbital_core::schema::TraitReference;
    program.orbitals.iter().find_map(|o| {
        o.traits
            .iter()
            .any(|t| matches!(t, TraitReference::Inline(td) if td.name == trait_name))
            .then_some(o.name.as_str())
    })
}

fn inline_trait<'a>(
    program: &'a orbital_core::schema::OrbitalSchema,
    trait_name: &str,
) -> Option<&'a orbital_core::schema::TraitDefinition> {
    use orbital_core::schema::TraitReference;
    program
        .orbitals
        .iter()
        .flat_map(|o| o.traits.iter())
        .find_map(|t| match t {
            TraitReference::Inline(td) if td.name == trait_name => Some(td),
            _ => None,
        })
}

fn describe_from_schema(
    schema: &orbital_core::schema::OrbitalSchema,
    trait_name: &str,
) -> anyhow::Result<(Vec<KnobOut>, EventsOut, String)> {
    let td = inline_trait(schema, trait_name)
        .ok_or_else(|| anyhow::anyhow!("no inline trait `{trait_name}` in `{}`", schema.name))?;
    let mut keys: Vec<(&String, &orbital_core::schema::ConfigField)> =
        td.config.iter().flatten().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    let knobs = keys
        .into_iter()
        .map(|(k, f)| {
            Ok(KnobOut {
                key: k.clone(),
                ty: f.field_type.clone(),
                default: f.default.as_ref().map(serde_json::to_value).transpose()?,
                label: f.label.clone(),
                description: f.description.clone(),
                tier: f.tier.clone(),
                vector: None,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let unique = |it: Vec<String>| {
        let mut seen = BTreeSet::new();
        it.into_iter()
            .filter(|e| seen.insert(e.clone()))
            .collect::<Vec<_>>()
    };
    let events = EventsOut {
        emits: unique(td.emits.iter().flatten().map(|e| e.event.clone()).collect()),
        listens: unique(
            td.listens
                .iter()
                .flatten()
                .map(|l| l.event.clone())
                .collect(),
        ),
        transitions: unique(
            td.state_machine
                .transitions
                .iter()
                .map(|t| t.event.clone())
                .collect(),
        ),
        domain: Some(domain_events(td)),
    };
    Ok((knobs, events, String::new()))
}

const ORBITALS_SEGMENT: &str = ".orbitals.";

type DeclaredConfigOut = BTreeMap<String, orbital_core::schema::ConfigField>;

/// One knob as a payload field (the shape a `listens` payload declaration lowers to), its
/// struct and list shapes resolved: what a tool offering the knob takes. An `event` knob or
/// struct member lists the events it may name as `values`.
#[derive(Serialize)]
struct KnobField {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<Vec<KnobField>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<orbital_core::schema::SExpression>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    synonyms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    /// The `@config.<knob>` this knob's value forwards: its text is owned (and set) there.
    #[serde(rename = "forwardedFrom", skip_serializing_if = "Option::is_none")]
    forwarded_from: Option<String>,
}

/// The events a knob may name: `scalar` for an `event` knob, `member` for an `event`
/// member of a struct knob (the compiler's `trait_event_names` / `orbital_event_names`).
struct EventVocab<'a> {
    scalar: &'a HashSet<String>,
    member: &'a HashSet<String>,
}

fn event_values(events: &HashSet<String>) -> Option<Vec<String>> {
    if events.is_empty() {
        return None;
    }
    let mut values: Vec<String> = events.iter().cloned().collect();
    values.sort();
    Some(values)
}

fn field_type_name(t: &orbital_core::schema::FieldType) -> anyhow::Result<String> {
    match serde_json::to_value(t)? {
        Value::String(s) => Ok(s),
        other => anyhow::bail!("a field type serialized as {other}, not a type name"),
    }
}

fn sorted_fields(
    props: &HashMap<String, orbital_core::schema::FieldDefinition>,
    events: &HashSet<String>,
) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|k| field_of(k, &props[k], events))
        .collect()
}

fn field_of(
    name: &str,
    def: &orbital_core::schema::FieldDefinition,
    events: &HashSet<String>,
) -> anyhow::Result<KnobField> {
    let bare =
        |ty: String, values: Option<Vec<String>>, properties: Option<Vec<KnobField>>| KnobField {
            name: name.to_string(),
            ty,
            required: def.required,
            values,
            properties,
            default: None,
            label: None,
            description: def.description.clone(),
            synonyms: None,
            tier: None,
            forwarded_from: None,
        };
    // `items` of a field reference is the value type it names, never a list element, and the entity is unknown here.
    if def.field_type == orbital_core::schema::FieldType::Field {
        return Ok(bare("field".to_string(), None, None));
    }
    if let Some(items) = &def.items {
        let element = field_of("", items, events)?;
        return Ok(bare(
            format!("[{}]", element.ty),
            element.values,
            element.properties,
        ));
    }
    let properties = def
        .properties
        .as_ref()
        .map(|p| sorted_fields(p, events))
        .transpose()?;
    let values = match (&def.values, &def.field_type) {
        (None, orbital_core::schema::FieldType::Event) => event_values(events),
        (values, _) => values.clone(),
    };
    Ok(bare(field_type_name(&def.field_type)?, values, properties))
}

fn knob_field(
    key: &str,
    cf: &orbital_core::schema::ConfigField,
    vocab: &EventVocab<'_>,
) -> anyhow::Result<KnobField> {
    let (ty, values, properties) = if cf.field_type == "field" {
        ("field".to_string(), None, None)
    } else if let Some(items) = &cf.items {
        let element = field_of("", items, vocab.member)?;
        (
            format!("[{}]", element.ty),
            element.values.or_else(|| cf.values.clone()),
            element.properties,
        )
    } else if let Some(props) = &cf.properties {
        (
            "object".to_string(),
            cf.values.clone(),
            Some(sorted_fields(props, vocab.member)?),
        )
    } else if cf.values.is_none() && cf.field_type == "event" {
        (cf.field_type.clone(), event_values(vocab.scalar), None)
    } else {
        (cf.field_type.clone(), cf.values.clone(), None)
    };
    Ok(KnobField {
        name: key.to_string(),
        ty,
        required: None,
        values,
        properties,
        default: cf.default.clone(),
        label: cf.label.clone(),
        description: cf.description.clone(),
        synonyms: cf.synonyms.clone(),
        tier: cf.tier.clone(),
        forwarded_from: cf.forwarded_from.clone(),
    })
}

fn knob_fields(
    config: Option<&HashMap<String, orbital_core::schema::ConfigField>>,
    vocab: &EventVocab<'_>,
) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<(&String, &orbital_core::schema::ConfigField)> =
        config.into_iter().flatten().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    keys.into_iter()
        .map(|(k, f)| knob_field(k, f, vocab))
        .collect()
}

#[derive(Serialize)]
struct EntityOut {
    name: String,
    fields: Vec<orbital_core::schema::FieldDefinition>,
    /// Tagged `[identity]`: the record `@user` resolves to.
    identity: bool,
    /// Stored (`persistent`, the default when undeclared) or page-only (`runtime`).
    persistence: orbital_core::schema::Persistence,
}

#[derive(Serialize)]
struct TraitOut {
    #[serde(rename = "trait")]
    name: String,
    config: DeclaredConfigOut,
    knobs: Vec<KnobField>,
    events: EventsOut,
    /// Declared `-> @rebindable`: the trait may be bound to another record.
    rebindable: bool,
    /// The upstream trait this one composes (`trait X = Alias.traits.Y`), as written; absent for a trait the behavior defines itself.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
}

#[derive(Serialize)]
struct PageOut {
    name: String,
    path: String,
    /// The traits the page mounts: what the behavior declares as standing on its own.
    traits: Vec<String>,
    /// The page's menu entry (`@label`/`@icon`); absent when the page declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

#[derive(Serialize)]
struct OrbitalOut {
    value: Value,
    config: DeclaredConfigOut,
    knobs: Vec<KnobField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity: Option<EntityOut>,
    /// Entities a sibling orbital owns that this one references: the names an import of it
    /// must map through `entities {}`.
    borrows: BTreeSet<String>,
    /// What this orbital needs from its siblings, derived from the program
    /// (`orbital_core::expects`): identity, entities, pages, events.
    expects: Vec<orbital_core::schema::ExpectDeclaration>,
    traits: Vec<TraitOut>,
    pages: Vec<PageOut>,
    /// Controls this orbital leaves for an embedding organism (`emitsScope external`) that nothing in
    /// it handles: an importer must route each one or the control is dead.
    forwards: Vec<ForwardOut>,
}

#[derive(Serialize)]
struct ForwardOut {
    #[serde(rename = "trait")]
    trait_name: String,
    event: String,
    /// The `pattern.prop` sites that render the control.
    via: Vec<String>,
}

#[derive(Serialize)]
struct BehaviorDescriptionOut {
    behavior: String,
    config: DeclaredConfigOut,
    knobs: Vec<KnobField>,
    orbitals: Vec<OrbitalOut>,
    /// The behaviors this one imports (`uses … from`), as written: what it already contains.
    uses: Vec<String>,
}

/// The composed traits of `orbital` as the behavior is written: local name to the
/// `Alias.traits.Y` path, for the call sites whose local name differs from the path.
fn call_site_refs(
    schema: &orbital_core::schema::OrbitalSchema,
    orbital: &str,
) -> HashMap<String, String> {
    use orbital_core::schema::TraitReference;
    schema
        .orbitals
        .iter()
        .filter(|o| o.name == orbital)
        .flat_map(|o| o.traits.iter())
        .filter_map(|t| match t {
            TraitReference::Reference {
                reference: Some(path),
                name,
                ..
            } => {
                let local = name.as_deref().unwrap_or(path);
                (local != path).then(|| (local.to_string(), path.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Every behavior `schema`'s orbitals import, once each, sorted.
fn uses_of(schema: &orbital_core::schema::OrbitalSchema) -> Vec<String> {
    let set: BTreeSet<String> = schema
        .orbitals
        .iter()
        .flat_map(|o| o.uses.iter().flatten())
        .map(|u| u.from.clone())
        .collect();
    set.into_iter().collect()
}

fn declared_config_of(
    config: Option<&HashMap<String, orbital_core::schema::ConfigField>>,
) -> DeclaredConfigOut {
    config
        .into_iter()
        .flatten()
        .map(|(k, f)| (k.clone(), f.clone()))
        .collect()
}

/// What an import of each orbital must map: one probe program imports every orbital of the
/// behavior with no `entities {}`, and the compiler's own import check names the entities each
/// one references but a sibling owns.
fn borrowed_entities(
    path: &Path,
    source: &orbital_core::schema::OrbitalSchema,
    program: &orbital_core::schema::OrbitalSchema,
    options: &orbital_compiler::CompilerOptions,
) -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    use orbital_core::schema::EntityRef;
    // A behavior that uses another lazily cannot be imported (the importer cannot resolve its
    // lazy orbitals), so no import of it maps anything.
    if !orbital_compiler::phases::lazy::split_lazy(&mut source.clone()).is_empty() {
        return Ok(BTreeMap::new());
    }
    let file = path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    let names: Vec<String> = program.orbitals.iter().map(|o| o.name.clone()).collect();
    let orbitals: Vec<Value> = program
        .orbitals
        .iter()
        .map(|o| {
            let mut reference = serde_json::json!({ "ref": format!("Source.orbitals.{}", o.name) });
            if let EntityRef::Inline(e) = &o.entity {
                reference["entity"] = Value::String(e.name.clone());
            }
            serde_json::json!({
                "name": o.name,
                "uses": [{ "from": format!("./{file}"), "as": "Source" }],
                "entity": format!("Source.orbitals.{}.entity", o.name),
                "traits": [],
                "pages": [],
                "reference": reference,
            })
        })
        .collect();
    let mut probe: orbital_core::schema::OrbitalSchema = serde_json::from_value(
        serde_json::json!({ "name": "DescribeProbe", "orbitals": orbitals }),
    )?;
    let probe_path = path.with_file_name("__describe_probe__.orb");
    let mut orbital_ref = Vec::new();
    orbital_compiler::phases::inline::inline_schema_with_orbital_diagnostics(
        &mut probe,
        &options.inline,
        Some(&probe_path),
        &mut orbital_ref,
        &mut Vec::new(),
    )
    .map_err(|e| anyhow::anyhow!("cannot import the orbitals of {}: {e}", path.display()))?;
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in orbital_ref {
        // A path naming an orbital (`orbitals[name=X]`) is a reference inside an imported file,
        // not one of the probe's own orbitals: it is that file's concern, not what the import maps.
        let Some(index) = d
            .path
            .strip_prefix("orbitals[")
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|i| i.parse::<usize>().ok())
        else {
            continue;
        };
        let name = names.get(index).ok_or_else(|| {
            anyhow::anyhow!("import diagnostic for orbital #{index} of {}", names.len())
        })?;
        out.entry(name.clone())
            .or_default()
            .extend(d.entity_unmapped.into_iter().map(|(_, e)| e));
    }
    Ok(out)
}

/// A whole behavior (or one orbital of it), described from its RESOLVED program: the app-level
/// knobs, and per orbital its own knobs, its traits' knobs under the names an import's
/// `traits {}` keys use, its primary entity's fields and its pages.
fn describe_behavior(
    behavior: &str,
    only_orbital: Option<&str>,
) -> anyhow::Result<BehaviorDescriptionOut> {
    describe_resolved(&resolve(behavior)?, behavior, only_orbital)
}

fn describe_resolved(
    resolved_src: &Resolved,
    behavior: &str,
    only_orbital: Option<&str>,
) -> anyhow::Result<BehaviorDescriptionOut> {
    use orbital_compiler::phases::validation::config_type::{
        orbital_event_names, trait_event_names,
    };
    use orbital_compiler::CompilerOptions;
    use orbital_core::schema::{EntityRef, PageRef, TraitReference};
    let options = CompilerOptions::from_env();
    let program = resolved_program(resolved_src, behavior)?;
    let forwards =
        orbital_compiler::phases::validation::final_program::forwarded_controls(&program);
    let canonical = match (
        &resolved_src.package,
        resolved_src.path.file_stem().and_then(|s| s.to_str()),
    ) {
        (Some(pkg), Some(name)) => format!("{}/{name}", pkg.prefix),
        _ => behavior.to_string(),
    };
    let borrows = borrowed_entities(&resolved_src.path, &resolved_src.schema, &program, &options)?;
    // Derived on the resolved program: imports are inlined, so an imported orbital's own
    // references are visible, and siblings' records keep the names an import maps.
    let program_json = serde_json::to_value(&program)?;
    let program_events: HashSet<String> = program
        .orbitals
        .iter()
        .flat_map(orbital_event_names)
        .collect();
    let mut orbitals = Vec::new();
    for orbital in &program.orbitals {
        if only_orbital.is_some_and(|o| o != orbital.name) {
            continue;
        }
        let entity = match &orbital.entity {
            EntityRef::Inline(e) => Some(EntityOut {
                name: e.name.clone(),
                fields: e.fields.clone(),
                identity: e.identity,
                persistence: e.persistence.clone().unwrap_or_default(),
            }),
            _ => None,
        };
        let orbital_events = orbital_event_names(orbital);
        let refs = call_site_refs(&resolved_src.schema, &orbital.name);
        let mut traits = Vec::new();
        for t in &orbital.traits {
            if let TraitReference::Inline(td) = t {
                let unique = |it: Vec<String>| {
                    let mut seen = BTreeSet::new();
                    it.into_iter()
                        .filter(|e| seen.insert(e.clone()))
                        .collect::<Vec<_>>()
                };
                let own = trait_event_names(td);
                traits.push(TraitOut {
                    rebindable: td.entity_rebindable == Some(true),
                    reference: refs.get(&td.name).cloned(),
                    name: td.name.clone(),
                    config: declared_config_of(td.config.as_ref()),
                    knobs: knob_fields(
                        td.config.as_ref(),
                        &EventVocab {
                            scalar: &own,
                            member: &orbital_events,
                        },
                    )?,
                    events: EventsOut {
                        emits: unique(td.emits.iter().flatten().map(|e| e.event.clone()).collect()),
                        listens: unique(
                            td.listens
                                .iter()
                                .flatten()
                                .map(|l| l.event.clone())
                                .collect(),
                        ),
                        transitions: unique(
                            td.state_machine
                                .transitions
                                .iter()
                                .map(|t| t.event.clone())
                                .collect(),
                        ),
                        domain: Some(domain_events(td)),
                    },
                });
            }
        }
        let pages = orbital
            .pages
            .iter()
            .flatten()
            .filter_map(|p| match p {
                PageRef::Inline(page) => Some(PageOut {
                    name: page.name.clone(),
                    path: page.path.clone(),
                    traits: page
                        .traits
                        .iter()
                        .flatten()
                        .filter_map(|t| t.trait_name().map(str::to_string))
                        .collect(),
                    label: page.label.clone(),
                    icon: page.icon.clone(),
                }),
                _ => None,
            })
            .collect();
        orbitals.push(OrbitalOut {
            value: serde_json::json!({ "behavior": canonical, "orbital": orbital.name }),
            config: declared_config_of(orbital.config.as_ref()),
            knobs: knob_fields(
                orbital.config.as_ref(),
                &EventVocab {
                    scalar: &orbital_events,
                    member: &orbital_events,
                },
            )?,
            borrows: borrows.get(&orbital.name).cloned().unwrap_or_default(),
            expects: orbital_core::expects::derive_expectations(&program_json, &orbital.name, None)
                .map_err(|e| anyhow::anyhow!("cannot derive `{}` expectations: {e}", orbital.name))?
                .expectations,
            entity,
            traits,
            pages,
            forwards: forwards
                .iter()
                .filter(|c| c.orbital == orbital.name)
                .map(|c| ForwardOut {
                    trait_name: c.trait_name.clone(),
                    event: c.event.clone(),
                    via: c.sites.clone(),
                })
                .collect(),
        });
    }
    if let Some(o) = only_orbital {
        if orbitals.is_empty() {
            anyhow::bail!("`{canonical}` has no orbital `{o}`");
        }
    }
    Ok(BehaviorDescriptionOut {
        behavior: canonical,
        config: declared_config_of(program.config.as_ref()),
        knobs: knob_fields(
            program.config.as_ref(),
            &EventVocab {
                scalar: &program_events,
                member: &program_events,
            },
        )?,
        orbitals,
        uses: uses_of(&resolved_src.schema),
    })
}

pub(crate) fn run_expects(spec: String, _json: bool) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(&expectations(&spec)?)?);
    Ok(())
}

/// Each orbital's derived expectations for the `.orb` file or behavior `spec` names, as
/// `orb behaviors expects` prints them.
pub(crate) fn expectations(spec: &str) -> anyhow::Result<Value> {
    let file = std::path::Path::new(spec);
    let schema: Value = if file.is_file() {
        serde_json::from_str(&std::fs::read_to_string(file)?)
            .map_err(|e| anyhow::anyhow!("`{spec}` is not a .orb program: {e}"))?
    } else {
        serde_json::to_value(&resolve(&spec)?.schema)?
    };
    let loaded: std::cell::RefCell<HashMap<String, Option<Value>>> = Default::default();
    let load = |from: &str| -> Option<Value> {
        if let Some(hit) = loaded.borrow().get(from) {
            return hit.clone();
        }
        let value = resolve(from)
            .ok()
            .and_then(|r| serde_json::to_value(&r.schema).ok());
        loaded.borrow_mut().insert(from.to_string(), value.clone());
        value
    };
    let mut out = serde_json::Map::new();
    let names: Vec<String> = schema["orbitals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["name"].as_str().map(str::to_string))
        .collect();
    for name in names {
        let derived = orbital_core::expects::derive_expectations(&schema, &name, Some(&load))
            .map_err(|e| anyhow::anyhow!("cannot derive `{name}` expectations: {e}"))?;
        out.insert(name, serde_json::to_value(derived)?);
    }
    Ok(Value::Object(out))
}

/// What `orb behaviors describe` reads: a specifier, or a trait/orbital value.
pub(crate) enum DescribeTarget {
    Spec(String),
    Value(Value),
}

pub(crate) fn run_describe(spec: String, _json: bool, knob_vectors: bool) -> anyhow::Result<()> {
    let target = if spec == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        DescribeTarget::Value(
            serde_json::from_str(&buf)
                .map_err(|e| anyhow::anyhow!("stdin is not a trait value: {e}"))?,
        )
    } else {
        DescribeTarget::Spec(spec)
    };
    let embedder = ProviderEmbedder::from_env();
    let vectors = if knob_vectors {
        Vectors::Attach(embedder.as_ref().map(|e| e as &dyn Embedder))
    } else {
        Vectors::Omit
    };
    let out = describe_with(target, None, vectors, None)?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// [`describe`] of a trait value a program names: a relative value resolves from that program.
pub(crate) fn describe_from(target: DescribeTarget, from: Option<&Path>) -> anyhow::Result<Value> {
    describe_with(target, from, Vectors::Omit, None)
}

/// [`describe_from`] with the trait's knob vectors chosen (`--knob-vectors`); `user_dir` replaces
/// `~/.almadar/vectors`.
pub(crate) fn describe_with(
    target: DescribeTarget,
    from: Option<&Path>,
    vectors: Vectors<'_>,
    user_dir: Option<&Path>,
) -> anyhow::Result<Value> {
    let (value, behavior, trait_name) = match target {
        DescribeTarget::Value(value) => {
            let field = |k: &str| {
                value
                    .get(k)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("trait value has no string `{k}`"))
            };
            if value.get("orbital").is_some() {
                return Ok(serde_json::to_value(describe_behavior(
                    &field("behavior")?,
                    Some(&field("orbital")?),
                )?)?);
            }
            let (b, t) = (field("behavior")?, field("trait")?);
            (Some(value), b, t)
        }
        DescribeTarget::Spec(spec) if spec.contains(TRAITS_SEGMENT) => {
            let (b, t) = split_trait_spec(&spec)?;
            (None, b, t)
        }
        DescribeTarget::Spec(spec) => {
            let (behavior, orbital) = match spec.rfind(ORBITALS_SEGMENT).filter(|&i| i > 0) {
                Some(i) => (
                    spec[..i].to_string(),
                    Some(spec[i + ORBITALS_SEGMENT.len()..].to_string()),
                ),
                None => (spec.clone(), None),
            };
            return Ok(serde_json::to_value(describe_behavior(
                &behavior,
                orbital.as_deref(),
            )?)?);
        }
    };

    let resolved = resolve_from(&behavior, from)?;
    let program = resolved_program(&resolved, &behavior)?;
    // Knobs and events come from the program as it resolves. Knob vectors are attached only when
    // asked for (D-A14), each embedded on a miss and kept in the user store.
    let (mut knobs, events, _) = describe_from_schema(&program, &trait_name)?;
    let mut model = String::new();
    if let (Vectors::Attach(embedder), Some(pkg)) = (vectors, &resolved.package) {
        let name = resolved
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("{} has no file stem", resolved.path.display()))?;
        let orbital = trait_orbital(&program, &trait_name).unwrap_or_default();
        let canonical = format!("{}/{name}", pkg.prefix);
        let facts = text_facts(&canonical, name)?;
        let registry = [(
            name.to_string(),
            RegistryEntry {
                name: name.to_string(),
                ..Default::default()
            },
        )];
        let texts: HashMap<String, String> = texts_of(&registry, &[facts], Some(TextKind::Knob))
            .into_iter()
            .map(|t| (t.key, t.text))
            .collect();
        let mut wanted = Vec::new();
        for knob in &knobs {
            let key = format!("{name}/{orbital}/{trait_name}/{}", knob.key);
            if let Some(text) = texts.get(&key) {
                wanted.push(Wanted {
                    key,
                    text: text.clone(),
                });
            }
        }
        let mut sources = Sources::for_packages([pkg], &UreqFetcher)?;
        if let Some(dir) = user_dir {
            sources.user_dir = dir.to_path_buf();
        }
        let (found_model, found) = lookup(&sources, &wanted, embedder)?;
        for knob in &mut knobs {
            let key = format!("{name}/{orbital}/{trait_name}/{}", knob.key);
            knob.vector = found.get(&key).cloned();
        }
        model = found_model;
    }
    let described = inline_trait(&program, &trait_name);
    let machine = described.map(|td| MachineOut {
        linked_entity: td.linked_entity.clone(),
        state_machine: td.state_machine.clone(),
    });
    let source = described
        .map(orbital_lolo::unlower::print_trait)
        .transpose()
        .map_err(|e| anyhow::anyhow!("cannot print `{trait_name}`: {e}"))?;
    let value = match value {
        Some(v) => v,
        None => {
            let canonical = match (
                &resolved.package,
                resolved.path.file_stem().and_then(|s| s.to_str()),
            ) {
                (Some(pkg), Some(name)) => format!("{}/{name}", pkg.prefix),
                _ => behavior.clone(),
            };
            serde_json::json!({ "behavior": canonical, "trait": trait_name })
        }
    };
    Ok(serde_json::to_value(DescriptionOut {
        value,
        knobs,
        events,
        machine,
        source,
        model,
    })?)
}

pub(crate) fn run_source(spec: String, _json: bool) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&source(&spec)?)?);
    Ok(())
}

/// The source file a behavior specifier resolves to (the `.lolo` an author reads as an example).
pub(crate) fn behavior_source_path(spec: &str) -> anyhow::Result<PathBuf> {
    let behavior = match spec.rfind(TRAITS_SEGMENT) {
        Some(i) if i > 0 => &spec[..i],
        _ => spec,
    };
    Ok(resolve(behavior)?.path)
}

/// `orb behaviors source` as a value: the behavior a specifier names, as a program.
pub(crate) fn source(spec: &str) -> anyhow::Result<Value> {
    let behavior = match spec.rfind(TRAITS_SEGMENT) {
        Some(i) if i > 0 => spec[..i].to_string(),
        _ => spec.to_string(),
    };
    source_value(&resolve(&behavior)?)
}

fn source_value(resolved: &Resolved) -> anyhow::Result<Value> {
    if resolved.path.extension().is_some_and(|e| e == "orb") {
        let raw = std::fs::read_to_string(&resolved.path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", resolved.path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", resolved.path.display()))
    } else {
        Ok(serde_json::to_value(&resolved.schema)?)
    }
}

// ---------------------------------------------------------------------------
// texts
// ---------------------------------------------------------------------------

/// Field names the sample-row stamping owns; the knob subject line leaves them out.
const RESERVED_FIELD_NAMES: [&str; 3] = ["id", "createdAt", "updatedAt"];

fn knob_facts(f: &KnobField) -> KnobFacts {
    KnobFacts {
        key: f.name.clone(),
        label: f.label.clone(),
        description: f.description.clone(),
        synonyms: f.synonyms.clone(),
        enum_values: f.values.clone().unwrap_or_default(),
    }
}

/// The facts the embedding texts read for one behavior: its resolved description (orbitals,
/// entity fields, traits, knobs) and the program as written (which orbitals import another
/// behavior's, which config keys an orbital declares itself).
pub(crate) fn text_facts(spec: &str, organism: &str) -> anyhow::Result<Vec<OrbitalFacts>> {
    let resolved = resolve(spec)?;
    let described = describe_resolved(&resolved, spec, None)?;
    let written = source_value(&resolved)?;
    let written: HashMap<&str, (bool, BTreeSet<&str>)> = written
        .get("orbitals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let name = o.get("name")?.as_str()?;
            let imported = o.get("reference").is_some_and(|r| !r.is_null());
            let config = o
                .get("config")
                .and_then(Value::as_object)
                .map(|c| c.keys().map(String::as_str).collect())
                .unwrap_or_default();
            Some((name, (imported, config)))
        })
        .collect();
    described
        .orbitals
        .iter()
        .map(|orbital| {
            let name = orbital.value["orbital"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{organism}: a described orbital has no name"))?;
            let (imported, config) = written.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "{organism}: orb described orbital {name}, which the program does not declare"
                )
            })?;
            Ok(OrbitalFacts {
                organism: organism.to_string(),
                orbital: name.to_string(),
                entity: orbital.entity.as_ref().map(|e| EntityFacts {
                    name: e.name.clone(),
                    field_names: e
                        .fields
                        .iter()
                        .filter_map(|f| f.name.clone())
                        .filter(|n| !RESERVED_FIELD_NAMES.contains(&n.as_str()))
                        .collect(),
                }),
                traits: orbital
                    .traits
                    .iter()
                    .map(|t| {
                        TraitFacts {
                            name: t.name.clone(),
                            reference: t.reference.clone(),
                            knobs: if *imported {
                                Vec::new()
                            } else {
                                t.knobs.iter().map(knob_facts).collect()
                            },
                        }
                    })
                    .collect(),
                knobs: orbital
                    .knobs
                    .iter()
                    .filter(|k| config.contains(k.name.as_str()))
                    .map(knob_facts)
                    .collect(),
            })
        })
        .collect()
}

/// One package's behaviors under the scopes, in path order.
pub(crate) struct ScopedPackage {
    pub prefix: String,
    pub behaviors: Vec<IndexEntry>,
}

/// Whether a behavior in `dirs` lies under the scope's directories.
fn in_scope(dirs: &[String], scope: &[String]) -> bool {
    dirs.len() >= scope.len() && dirs[..scope.len()] == *scope
}

/// The behaviors under `paths` (the scopes `catalog` takes), per package in first-mention
/// order, each package's behaviors in path order.
pub(crate) fn scoped_behaviors(paths: &[String]) -> anyhow::Result<Vec<ScopedPackage>> {
    if paths.is_empty() {
        anyhow::bail!("give at least one scope path, e.g. `almadar-std/ui/core/molecules`");
    }
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let scopes: Vec<Scope> = paths
        .iter()
        .map(|p| resolve_scope(&packages, p))
        .collect::<anyhow::Result<_>>()?;
    let mut order: Vec<usize> = Vec::new();
    for scope in &scopes {
        if !order.contains(&scope.package) {
            order.push(scope.package);
        }
    }
    order
        .into_iter()
        .map(|pi| {
            let behaviors = package_index(&packages, pi)?
                .entries
                .into_iter()
                .filter(|e| {
                    scopes
                        .iter()
                        .filter(|s| s.package == pi)
                        .any(|s| in_scope(&e.dirs, &s.dirs))
                })
                .collect();
            Ok(ScopedPackage {
                prefix: packages[pi].prefix.clone(),
                behaviors,
            })
        })
        .collect()
}

#[cfg(test)]
mod capability_phrase_tests {
    #[test]
    fn capability_phrases_split_the_declared_string() {
        assert_eq!(
            super::capability_phrases(Some("program repair,  evaluate a program, ")),
            ["program repair", "evaluate a program"]
        );
        assert!(super::capability_phrases(None).is_empty());
        assert!(super::capability_phrases(Some(" , ")).is_empty());
    }
}

#[cfg(test)]
mod uses_of_tests {
    use super::uses_of;
    use orbital_core::schema::OrbitalSchema;

    fn schema(json: serde_json::Value) -> OrbitalSchema {
        serde_json::from_value(json).expect("schema")
    }

    #[test]
    fn lists_every_orbitals_uses_once_sorted() {
        let s = schema(serde_json::json!({ "name": "s", "orbitals": [
            { "name": "A", "uses": [ { "from": "almadar-behaviors/std-snake-loop", "as": "Loop" }, { "from": "std/behaviors/std-app-layout", "as": "Shell" } ],
              "entity": { "name": "A", "fields": [] }, "traits": [], "pages": [] },
            { "name": "B", "uses": [ { "from": "almadar-behaviors/std-snake-loop", "as": "L2" } ],
              "entity": { "name": "B", "fields": [] }, "traits": [], "pages": [] }
        ] }));
        assert_eq!(
            uses_of(&s),
            vec![
                "almadar-behaviors/std-snake-loop".to_string(),
                "std/behaviors/std-app-layout".to_string()
            ]
        );
    }

    #[test]
    fn control_a_behavior_without_imports_uses_nothing() {
        let s = schema(serde_json::json!({ "name": "s", "orbitals": [
            { "name": "A", "entity": { "name": "A", "fields": [] }, "traits": [], "pages": [] }
        ] }));
        assert!(uses_of(&s).is_empty());
    }
}
