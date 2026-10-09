//! `orb behaviors catalog | describe | source` — the installed behavior
//! packages as data. Resolution is the existing machinery (`discover` for the
//! package set, `ExternalOrbitalLoader` for specifiers); this module only
//! reads the sidecars those packages declare (`orb.index`, `orb.embeddings`,
//! `orb.factorySignatures`) and shapes them to the `@almadar/core`
//! `CatalogEntrySchema` / `TraitDescriptionSchema` contracts.

use orbital_compiler::loader::{index_registry, ExternalOrbitalLoader, LoaderConfig};
use orbital_core::behavior_packages::{BehaviorPackage, STD_PACKAGE};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use super::npm_project::discover_from;

const TRAITS_SEGMENT: &str = ".traits.";
const STD_ALIAS: &str = "std";
const LEVELS: [&str; 3] = ["atom", "molecule", "organism"];
const EXPOSURES: [&str; 5] = ["app", "experience", "palette", "both", "internal"];

// ---------------------------------------------------------------------------
// Embedding manifests
// ---------------------------------------------------------------------------

/// One `orb.embeddings` file: the model it was stamped with and its vectors,
/// kept undecoded until asked for. Two on-disk shapes (mirroring
/// `behaviors/embeddings.ts` and `behaviors/knob-embeddings.ts`): `vectors`
/// keyed directly, or an interned `table` + `keys` index.
struct Manifest {
    model: String,
    vectors: HashMap<String, Value>,
    table: Vec<Value>,
    keys: HashMap<String, usize>,
}

impl Manifest {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("invalid embeddings manifest {}: {e}", path.display()))?;
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("{} carries no `model`", path.display()))?
            .to_string();
        let vectors = match value.get("vectors") {
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => HashMap::new(),
        };
        let table = value
            .get("table")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let keys = match value.get("keys") {
            Some(Value::Object(m)) => m
                .iter()
                .filter_map(|(k, v)| v.as_u64().map(|i| (k.clone(), i as usize)))
                .collect(),
            _ => HashMap::new(),
        };
        if vectors.is_empty() && table.is_empty() {
            anyhow::bail!(
                "{} carries neither `vectors` nor `table`/`keys`",
                path.display()
            );
        }
        Ok(Self {
            model,
            vectors,
            table,
            keys,
        })
    }

    fn vector(&self, key: &str) -> Option<Vec<f64>> {
        let stored = self
            .vectors
            .get(key)
            .or_else(|| self.keys.get(key).and_then(|i| self.table.get(*i)))?;
        decode_vector(stored)
    }
}

/// A plain number array, or `{ s, d }`: `d` = base64 of one signed byte per
/// dimension, value = `byte * s / 127`.
fn decode_vector(stored: &Value) -> Option<Vec<f64>> {
    match stored {
        Value::Array(items) => items.iter().map(Value::as_f64).collect(),
        Value::Object(obj) => {
            let scale = obj.get("s")?.as_f64()?;
            let bytes = base64_decode(obj.get("d")?.as_str()?)?;
            Some(
                bytes
                    .into_iter()
                    .map(|b| (f64::from(b as i8) * scale) / 127.0)
                    .collect(),
            )
        }
        _ => None,
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

fn package_manifests(pkg: &BehaviorPackage) -> anyhow::Result<Vec<Manifest>> {
    pkg.embeddings.iter().map(|p| Manifest::read(p)).collect()
}

/// The single model every manifest in `manifests` was stamped with; `""` when
/// there are none; an error naming both when they disagree.
fn single_model<'a>(
    manifests: impl IntoIterator<Item = (&'a str, &'a Manifest)>,
) -> anyhow::Result<String> {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (owner, m) in manifests {
        seen.entry(m.model.as_str()).or_insert(owner);
    }
    let mut it = seen.into_iter();
    match (it.next(), it.next()) {
        (None, _) => Ok(String::new()),
        (Some((model, _)), None) => Ok(model.to_string()),
        (Some((a, owner_a)), Some((b, owner_b))) => anyhow::bail!(
            "embedding manifests were stamped with different models: {a} ({owner_a}) and {b} ({owner_b})"
        ),
    }
}

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
struct CatalogEntryOut {
    behavior: String,
    package: String,
    topic: String,
    level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exposure: Option<String>,
    description: String,
    capabilities: Vec<String>,
    traits: Vec<TraitValueOut>,
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

fn relative_dir(registry: &Path, file: &Path) -> Option<Vec<String>> {
    let rel = file.parent()?.strip_prefix(registry).ok()?;
    Some(
        rel.components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str().map(str::to_string),
                _ => None,
            })
            .collect(),
    )
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

pub(crate) fn run_catalog(
    paths: Vec<String>,
    exposure: Vec<String>,
    json: bool,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        anyhow::bail!("give at least one scope path, e.g. `almadar-std/ui/core/molecules`");
    }
    for e in &exposure {
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
    let mut per_package: BTreeMap<usize, Vec<(String, Value, PathBuf)>> = BTreeMap::new();
    for scope in &scopes {
        let pkg = &packages[scope.package];
        let raw = std::fs::read_to_string(&pkg.index)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", pkg.index.display()))?;
        let index: Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("invalid index {}: {e}", pkg.index.display()))?;
        let behaviors = index
            .get("behaviors")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("{} has no `behaviors` map", pkg.index.display()))?;
        let files = index_registry(&pkg.registry);
        for (name, entry) in behaviors {
            let file = files.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "{} lists `{name}` but {} holds no {name}.orb",
                    pkg.index.display(),
                    pkg.registry.display()
                )
            })?;
            let dir = relative_dir(&pkg.registry, file).ok_or_else(|| {
                anyhow::anyhow!("{} is outside {}", file.display(), pkg.registry.display())
            })?;
            if dir.len() < scope.dirs.len() || dir[..scope.dirs.len()] != scope.dirs[..] {
                continue;
            }
            per_package.entry(scope.package).or_default().push((
                name.clone(),
                entry.clone(),
                file.clone(),
            ));
        }
    }

    for (pi, rows) in &per_package {
        let pkg = &packages[*pi];
        for (name, entry, _) in rows {
            let exposure_of = entry.get("exposure").and_then(Value::as_str);
            if let Some(e) = exposure_of {
                if !EXPOSURES.contains(&e) {
                    anyhow::bail!("{name}: unknown exposure `{e}` in {}", pkg.index.display());
                }
            }
            if !exposure.is_empty() && !exposure_of.is_some_and(|e| exposure.iter().any(|x| x == e))
            {
                continue;
            }
            let level = entry
                .get("level")
                .and_then(Value::as_str)
                .filter(|l| LEVELS.contains(l))
                .ok_or_else(|| {
                    anyhow::anyhow!("{name}: `level` must be one of {}", LEVELS.join(", "))
                })?;
            let topic = entry
                .get("topic")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("{name}: index entry has no `topic`"))?;
            let traits = entry
                .get("traits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        .map(|n| TraitValueOut {
                            behavior: format!("{}/{name}", pkg.prefix),
                            trait_name: n.to_string(),
                        })
                        .ok_or_else(|| anyhow::anyhow!("{name}: a trait entry has no `name`"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let key = format!("{}/{name}", pkg.prefix);
            contributing.insert(*pi);
            entries.insert(
                key.clone(),
                CatalogEntryOut {
                    behavior: key,
                    package: pkg.name.clone(),
                    topic: topic.to_string(),
                    level: level.to_string(),
                    exposure: exposure_of.map(str::to_string),
                    description: entry
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    capabilities: str_list(entry.get("capabilities")),
                    traits,
                    vectors: BTreeMap::new(),
                    model: String::new(),
                },
            );
        }
    }

    let mut manifests: BTreeMap<usize, Vec<Manifest>> = BTreeMap::new();
    for pi in &contributing {
        manifests.insert(*pi, package_manifests(&packages[*pi])?);
    }
    let model = single_model(manifests.iter().flat_map(|(pi, ms)| {
        let owner = packages[*pi].name.as_str();
        ms.iter().map(move |m| (owner, m))
    }))?;
    for entry in entries.values_mut() {
        let pi = packages
            .iter()
            .position(|p| p.name == entry.package)
            .ok_or_else(|| anyhow::anyhow!("package `{}` vanished", entry.package))?;
        let name = entry
            .behavior
            .strip_prefix(&format!("{}/", packages[pi].prefix))
            .unwrap_or(&entry.behavior)
            .to_string();
        for (facet, key) in [
            ("description", name.clone()),
            ("capabilities", format!("{name}#capabilities")),
        ] {
            if let Some(v) = manifests
                .get(&pi)
                .into_iter()
                .flatten()
                .find_map(|m| m.vector(&key))
            {
                entry.vectors.insert(facet.to_string(), v);
            }
        }
        entry.model = model.clone();
    }

    let out: Vec<&CatalogEntryOut> = entries.values().collect();
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

struct Resolved {
    path: PathBuf,
    package: Option<BehaviorPackage>,
    schema: orbital_core::schema::OrbitalSchema,
}

fn resolve(spec: &str) -> anyhow::Result<Resolved> {
    let cwd = std::env::current_dir()?;
    let packages = discover_from(&cwd)?;
    let mut loader = ExternalOrbitalLoader::new(LoaderConfig {
        base_path: cwd,
        packages: packages.clone(),
        ..Default::default()
    });
    let mut chain = orbital_compiler::loader::ImportChain::new();
    let loaded = loader
        .load(spec, None, &mut chain)
        .map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))?;
    let path = loaded.source_path.clone();
    let schema = loaded.schema.clone();
    let package = packages.into_iter().find(|p| path.starts_with(&p.registry));
    Ok(Resolved {
        path,
        package,
        schema,
    })
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
}

#[derive(Serialize)]
struct DescriptionOut {
    value: Value,
    knobs: Vec<KnobOut>,
    events: EventsOut,
    model: String,
}

fn knob_of(def: &Value, vector: Option<Vec<f64>>) -> anyhow::Result<KnobOut> {
    let text = |k: &str| def.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(KnobOut {
        key: text("key").ok_or_else(|| anyhow::anyhow!("a knob definition has no `key`"))?,
        ty: text("type").ok_or_else(|| anyhow::anyhow!("a knob definition has no `type`"))?,
        default: def.get("default").cloned(),
        label: text("label"),
        description: text("description"),
        tier: text("tier"),
        vector,
    })
}

fn describe_from_signatures(
    r: &Resolved,
    pkg: &BehaviorPackage,
    trait_name: &str,
) -> anyhow::Result<(Vec<KnobOut>, EventsOut, String)> {
    let name = r
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} has no file stem", r.path.display()))?;
    let raw = std::fs::read_to_string(&pkg.factory_signatures)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", pkg.factory_signatures.display()))?;
    let catalog: Value = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "invalid factory signatures {}: {e}",
            pkg.factory_signatures.display()
        )
    })?;
    let signatures = catalog
        .get("signatures")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("{} has no `signatures`", pkg.factory_signatures.display())
        })?;
    let knob_defs = catalog.get("knobDefs").and_then(Value::as_array);
    let (orbital, signature) = signatures
        .iter()
        .filter(|s| s.get("organism").and_then(Value::as_str) == Some(name))
        .find_map(|s| {
            s.get("traits")
                .and_then(Value::as_array)?
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(trait_name))
                .map(|t| {
                    (
                        s.get("orbital").and_then(Value::as_str).unwrap_or_default(),
                        t,
                    )
                })
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{name}` has no trait `{trait_name}` in {}",
                pkg.factory_signatures.display()
            )
        })?;

    let defs: Vec<&Value> = match (signature.get("overridableConfigKeyRefs"), knob_defs) {
        (Some(Value::Array(refs)), Some(table)) => refs
            .iter()
            .filter_map(|i| i.as_u64().and_then(|i| table.get(i as usize)))
            .collect(),
        _ => signature
            .get("overridableConfigKeys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect(),
    };
    let manifests = package_manifests(pkg)?;
    let mut knobs = Vec::with_capacity(defs.len());
    let mut used_vector = false;
    for def in defs {
        let key = def.get("key").and_then(Value::as_str).unwrap_or_default();
        let full = format!("{name}/{orbital}/{trait_name}/{key}");
        let vector = manifests.iter().find_map(|m| m.vector(&full));
        used_vector |= vector.is_some();
        knobs.push(knob_of(def, vector)?);
    }
    let events = EventsOut {
        emits: str_list(signature.get("emittedEvents")),
        listens: str_list(signature.get("listenedEvents")),
        transitions: str_list(signature.get("transitionEvents")),
    };
    let model = single_model(manifests.iter().map(|m| (pkg.name.as_str(), m)))?;
    let _ = used_vector;
    Ok((knobs, events, model))
}

fn describe_from_schema(
    schema: &orbital_core::schema::OrbitalSchema,
    trait_name: &str,
) -> anyhow::Result<(Vec<KnobOut>, EventsOut, String)> {
    use orbital_core::schema::TraitReference;
    let td = schema
        .orbitals
        .iter()
        .flat_map(|o| o.traits.iter())
        .find_map(|t| match t {
            TraitReference::Inline(td) if td.name == trait_name => Some(td),
            _ => None,
        })
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
    };
    Ok((knobs, events, String::new()))
}

const ORBITALS_SEGMENT: &str = ".orbitals.";

type DeclaredConfigOut = BTreeMap<String, orbital_core::schema::ConfigField>;

/// One knob as a payload field (the shape a `listens` payload declaration lowers to), its
/// struct and list shapes resolved: what a tool offering the knob takes.
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
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    synonyms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
}

fn field_type_name(t: &orbital_core::schema::FieldType) -> anyhow::Result<String> {
    match serde_json::to_value(t)? {
        Value::String(s) => Ok(s),
        other => anyhow::bail!("a field type serialized as {other}, not a type name"),
    }
}

fn sorted_fields(
    props: &HashMap<String, orbital_core::schema::FieldDefinition>,
) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    keys.into_iter().map(|k| field_of(k, &props[k])).collect()
}

fn field_of(name: &str, def: &orbital_core::schema::FieldDefinition) -> anyhow::Result<KnobField> {
    let bare = |ty: String, values: Option<Vec<String>>, properties: Option<Vec<KnobField>>| KnobField {
        name: name.to_string(),
        ty,
        required: def.required,
        values,
        properties,
        label: None,
        description: def.description.clone(),
        synonyms: None,
        tier: None,
    };
    if let Some(items) = &def.items {
        let element = field_of("", items)?;
        return Ok(bare(format!("[{}]", element.ty), element.values, element.properties));
    }
    let properties = def.properties.as_ref().map(sorted_fields).transpose()?;
    Ok(bare(field_type_name(&def.field_type)?, def.values.clone(), properties))
}

fn knob_field(key: &str, cf: &orbital_core::schema::ConfigField) -> anyhow::Result<KnobField> {
    let (ty, values, properties) = if let Some(items) = &cf.items {
        let element = field_of("", items)?;
        (format!("[{}]", element.ty), element.values.or_else(|| cf.values.clone()), element.properties)
    } else if let Some(props) = &cf.properties {
        ("object".to_string(), cf.values.clone(), Some(sorted_fields(props)?))
    } else {
        (cf.field_type.clone(), cf.values.clone(), None)
    };
    Ok(KnobField {
        name: key.to_string(),
        ty,
        required: None,
        values,
        properties,
        label: cf.label.clone(),
        description: cf.description.clone(),
        synonyms: cf.synonyms.clone(),
        tier: cf.tier.clone(),
    })
}

fn knob_fields(
    config: Option<&HashMap<String, orbital_core::schema::ConfigField>>,
) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<(&String, &orbital_core::schema::ConfigField)> = config.into_iter().flatten().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    keys.into_iter().map(|(k, f)| knob_field(k, f)).collect()
}

#[derive(Serialize)]
struct EntityOut {
    name: String,
    fields: Vec<orbital_core::schema::FieldDefinition>,
}

#[derive(Serialize)]
struct TraitOut {
    #[serde(rename = "trait")]
    name: String,
    config: DeclaredConfigOut,
    knobs: Vec<KnobField>,
    events: EventsOut,
}

#[derive(Serialize)]
struct PageOut {
    name: String,
    path: String,
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
    traits: Vec<TraitOut>,
    pages: Vec<PageOut>,
}

#[derive(Serialize)]
struct BehaviorDescriptionOut {
    behavior: String,
    config: DeclaredConfigOut,
    knobs: Vec<KnobField>,
    orbitals: Vec<OrbitalOut>,
}

fn declared_config_of(
    config: Option<&std::collections::HashMap<String, orbital_core::schema::ConfigField>>,
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
    program: &orbital_core::schema::OrbitalSchema,
    options: &orbital_compiler::CompilerOptions,
) -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    use orbital_core::schema::EntityRef;
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
    let mut probe: orbital_core::schema::OrbitalSchema =
        serde_json::from_value(serde_json::json!({ "name": "DescribeProbe", "orbitals": orbitals }))?;
    let probe_path = path.with_file_name("__describe_probe__.orb");
    let mut orbital_ref = Vec::new();
    orbital_compiler::phases::inline::inline_schema_with_orbital_diagnostics(
        &mut probe,
        &options.inline,
        Some(&probe_path),
        &mut Vec::new(),
        &mut orbital_ref,
        &mut Vec::new(),
    )
    .map_err(|e| anyhow::anyhow!("cannot import the orbitals of {}: {e}", path.display()))?;
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in orbital_ref {
        let index = d
            .path
            .strip_prefix("orbitals[")
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|i| i.parse::<usize>().ok())
            .ok_or_else(|| anyhow::anyhow!("unexpected import diagnostic path `{}`", d.path))?;
        let name = names
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("import diagnostic for orbital #{index} of {}", names.len()))?;
        out.entry(name.clone()).or_default().extend(d.entity_unmapped.into_iter().map(|(_, e)| e));
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
    use orbital_compiler::{resolve_schema_with_path, CompilerOptions};
    use orbital_core::schema::{EntityRef, PageRef, TraitReference};
    let resolved_src = resolve(behavior)?;
    let options = CompilerOptions::from_env();
    let mut schema = resolved_src.schema.clone();
    orbital_compiler::phases::lazy::split_lazy_and_resolve(
        &mut schema,
        &options,
        Some(&resolved_src.path),
    )
    .map_err(|e| anyhow::anyhow!("cannot resolve `{behavior}`: {e}"))?;
    let program = resolve_schema_with_path(schema, &options, Some(&resolved_src.path))
        .map_err(|e| anyhow::anyhow!("cannot resolve `{behavior}`: {e}"))?;
    let canonical = match (
        &resolved_src.package,
        resolved_src.path.file_stem().and_then(|s| s.to_str()),
    ) {
        (Some(pkg), Some(name)) => format!("{}/{name}", pkg.prefix),
        _ => behavior.to_string(),
    };
    let borrows = borrowed_entities(&resolved_src.path, &program, &options)?;
    let mut orbitals = Vec::new();
    for orbital in &program.orbitals {
        if only_orbital.is_some_and(|o| o != orbital.name) {
            continue;
        }
        let entity = match &orbital.entity {
            EntityRef::Inline(e) => Some(EntityOut {
                name: e.name.clone(),
                fields: e.fields.clone(),
            }),
            _ => None,
        };
        let mut traits = Vec::new();
        for t in &orbital.traits {
            if let TraitReference::Inline(td) = t {
                let unique = |it: Vec<String>| {
                    let mut seen = BTreeSet::new();
                    it.into_iter()
                        .filter(|e| seen.insert(e.clone()))
                        .collect::<Vec<_>>()
                };
                traits.push(TraitOut {
                    name: td.name.clone(),
                    config: declared_config_of(td.config.as_ref()),
                    knobs: knob_fields(td.config.as_ref())?,
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
                }),
                _ => None,
            })
            .collect();
        orbitals.push(OrbitalOut {
            value: serde_json::json!({ "behavior": canonical, "orbital": orbital.name }),
            config: declared_config_of(orbital.config.as_ref()),
            knobs: knob_fields(orbital.config.as_ref())?,
            borrows: borrows.get(&orbital.name).cloned().unwrap_or_default(),
            entity,
            traits,
            pages,
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
        knobs: knob_fields(program.config.as_ref())?,
        orbitals,
    })
}

pub(crate) fn run_describe(spec: String, _json: bool) -> anyhow::Result<()> {
    let (value, behavior, trait_name) = if spec == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        let value: Value = serde_json::from_str(&buf)
            .map_err(|e| anyhow::anyhow!("stdin is not a trait value: {e}"))?;
        let field = |k: &str| {
            value
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("trait value has no string `{k}`"))
        };
        if value.get("orbital").is_some() {
            let out = describe_behavior(&field("behavior")?, Some(&field("orbital")?))?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(());
        }
        let (b, t) = (field("behavior")?, field("trait")?);
        (Some(value), b, t)
    } else if spec.contains(TRAITS_SEGMENT) {
        let (b, t) = split_trait_spec(&spec)?;
        (None, b, t)
    } else {
        let (behavior, orbital) = match spec.rfind(ORBITALS_SEGMENT).filter(|&i| i > 0) {
            Some(i) => (
                spec[..i].to_string(),
                Some(spec[i + ORBITALS_SEGMENT.len()..].to_string()),
            ),
            None => (spec.clone(), None),
        };
        let out = describe_behavior(&behavior, orbital.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    };

    let resolved = resolve(&behavior)?;
    let (knobs, events, model) = match &resolved.package {
        Some(pkg) => describe_from_signatures(&resolved, pkg, &trait_name)?,
        None => describe_from_schema(&resolved.schema, &trait_name)?,
    };
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
    let out = DescriptionOut {
        value,
        knobs,
        events,
        model,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

pub(crate) fn run_source(spec: String, _json: bool) -> anyhow::Result<()> {
    let behavior = match spec.rfind(TRAITS_SEGMENT) {
        Some(i) if i > 0 => spec[..i].to_string(),
        _ => spec,
    };
    let resolved = resolve(&behavior)?;
    let value: Value = if resolved.path.extension().is_some_and(|e| e == "orb") {
        let raw = std::fs::read_to_string(&resolved.path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", resolved.path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", resolved.path.display()))?
    } else {
        serde_json::to_value(&resolved.schema)?
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
