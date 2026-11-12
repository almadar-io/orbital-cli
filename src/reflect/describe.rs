//! A behavior, orbital or trait described from its resolved program: knobs, events, entity, traits, pages and
//! expectations. The knob vectors a description may carry are attached by std, never here.

use super::{behavior_part, resolve_from, Resolved, TRAITS_SEGMENT};
use orbital_compiler::phases::validation::config_type::{orbital_event_names, trait_event_names};
use orbital_core::collections::IndexMap;
use orbital_core::schema::{ConfigField, EntityRef, FieldDefinition, FieldType, OrbitalSchema, PageRef, TraitDefinition, TraitReference};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

const ORBITALS_SEGMENT: &str = ".orbitals.";

/// What a describe reads: a specifier, or a trait or orbital value.
pub(crate) enum DescribeTarget {
    Spec(String),
    Value(Value),
}

type DeclaredConfig = BTreeMap<String, ConfigField>;

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
}

#[derive(Serialize)]
struct EventsOut {
    emits: Vec<String>,
    listens: Vec<String>,
    transitions: Vec<String>,
    domain: Vec<String>,
}

#[derive(Serialize)]
struct MachineOut {
    #[serde(rename = "linkedEntity", skip_serializing_if = "Option::is_none")]
    linked_entity: Option<String>,
    #[serde(rename = "stateMachine")]
    state_machine: orbital_core::schema::StateMachine,
}

#[derive(Serialize)]
struct TraitDescription {
    value: Value,
    knobs: Vec<KnobOut>,
    events: EventsOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine: Option<MachineOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<orbital_lolo::unlower::PrintedTrait>,
    model: String,
}

/// One knob as a payload field, its struct and list shapes resolved.
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
    #[serde(rename = "forwardedFrom", skip_serializing_if = "Option::is_none")]
    forwarded_from: Option<String>,
}

/// The events a knob may name: `scalar` for an `event` knob, `member` for an `event` member of a struct knob.
struct EventVocab<'a> {
    scalar: &'a HashSet<String>,
    member: &'a HashSet<String>,
}

#[derive(Serialize)]
struct EntityOut {
    name: String,
    fields: Vec<FieldDefinition>,
    identity: bool,
    persistence: orbital_core::schema::Persistence,
}

#[derive(Serialize)]
struct TraitOut {
    #[serde(rename = "trait")]
    name: String,
    config: DeclaredConfig,
    knobs: Vec<KnobField>,
    events: EventsOut,
    rebindable: bool,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
}

#[derive(Serialize)]
struct PageOut {
    name: String,
    path: String,
    traits: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

#[derive(Serialize)]
struct OrbitalOut {
    value: Value,
    config: DeclaredConfig,
    knobs: Vec<KnobField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity: Option<EntityOut>,
    expects: Vec<orbital_core::schema::ExpectDeclaration>,
    traits: Vec<TraitOut>,
    pages: Vec<PageOut>,
}

#[derive(Serialize)]
struct BehaviorDescription {
    behavior: String,
    config: DeclaredConfig,
    knobs: Vec<KnobField>,
    orbitals: Vec<OrbitalOut>,
    uses: Vec<String>,
    /// The look and languages the behavior's app header declares; absent when it declares none.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    header: serde_json::Map<String, Value>,
}

/// The app-header keys a description carries: the ones a program built from the behavior must keep to look and read
/// as it does. Reach (hooks, egress, grants) is not here: composition unions it from the imports. `themeDefinitions`
/// is not here either: resolve carries it for every program, declared theme or not, so it is derived, not declared.
const DESCRIBED_HEADER_KEYS: [&str; 3] = ["theme", "themes", "locales"];

fn described_header(program: &Value) -> serde_json::Map<String, Value> {
    DESCRIBED_HEADER_KEYS
        .into_iter()
        .filter_map(|key| program.get(key).filter(|v| !v.is_null()).map(|v| (key.to_string(), v.clone())))
        .collect()
}

fn unique(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items.filter(|e| seen.insert(e.clone())).collect()
}

fn domain_events(td: &TraitDefinition) -> Vec<String> {
    let tiered = td
        .emits
        .iter()
        .flatten()
        .map(|e| (&e.event, &e.tier))
        .chain(td.listens.iter().flatten().map(|l| (&l.event, &l.tier)))
        .filter(|(_, tier)| tier.as_deref() == Some("domain"))
        .map(|(event, _)| event.clone());
    unique(tiered)
}

fn events_of(td: &TraitDefinition) -> EventsOut {
    EventsOut {
        emits: unique(td.emits.iter().flatten().map(|e| e.event.clone())),
        listens: unique(td.listens.iter().flatten().map(|l| l.event.clone())),
        transitions: unique(td.state_machine.transitions.iter().map(|t| t.event.clone())),
        domain: domain_events(td),
    }
}

fn event_values(events: &HashSet<String>) -> Option<Vec<String>> {
    if events.is_empty() {
        return None;
    }
    let mut values: Vec<String> = events.iter().cloned().collect();
    values.sort();
    Some(values)
}

fn field_type_name(t: &FieldType) -> anyhow::Result<String> {
    match serde_json::to_value(t)? {
        Value::String(s) => Ok(s),
        other => anyhow::bail!("a field type serialized as {other}, not a type name"),
    }
}

fn sorted_fields(props: &IndexMap<String, FieldDefinition>, events: &HashSet<String>) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    keys.into_iter().map(|k| field_of(k, &props[k], events)).collect()
}

fn field_of(name: &str, def: &FieldDefinition, events: &HashSet<String>) -> anyhow::Result<KnobField> {
    let bare = |ty: String, values: Option<Vec<String>>, properties: Option<Vec<KnobField>>| KnobField {
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
    if def.field_type == FieldType::Field {
        return Ok(bare("field".to_string(), None, None));
    }
    if let Some(items) = &def.items {
        let element = field_of("", items, events)?;
        return Ok(bare(format!("[{}]", element.ty), element.values, element.properties));
    }
    let properties = def.properties.as_ref().map(|p| sorted_fields(p, events)).transpose()?;
    let values = match (&def.values, &def.field_type) {
        (None, FieldType::Event) => event_values(events),
        (values, _) => values.clone(),
    };
    Ok(bare(field_type_name(&def.field_type)?, values, properties))
}

fn knob_field(key: &str, cf: &ConfigField, vocab: &EventVocab<'_>) -> anyhow::Result<KnobField> {
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
        ("object".to_string(), cf.values.clone(), Some(sorted_fields(props, vocab.member)?))
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

fn knob_fields(config: Option<&IndexMap<String, ConfigField>>, vocab: &EventVocab<'_>) -> anyhow::Result<Vec<KnobField>> {
    let mut keys: Vec<(&String, &ConfigField)> = config.into_iter().flatten().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    keys.into_iter().map(|(k, f)| knob_field(k, f, vocab)).collect()
}

fn declared_config(config: Option<&IndexMap<String, ConfigField>>) -> DeclaredConfig {
    config.into_iter().flatten().map(|(k, f)| (k.clone(), f.clone())).collect()
}

/// A behavior as the runtime runs it: lazy behaviors split, every import and composed trait inlined.
/// The behavior resolved through the process's one resolve cache, the entry `behavior/resolve` answers from: a
/// describe after a resolve of the same file resolves nothing again.
fn resolved_program(r: &Resolved, spec: &str) -> anyhow::Result<std::sync::Arc<crate::commands::dev::ResolvedEntry>> {
    crate::commands::dev::resolved_file(&r.path).map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))
}

fn inline_trait<'a>(program: &'a OrbitalSchema, name: &str) -> Option<&'a TraitDefinition> {
    program.orbitals.iter().flat_map(|o| o.traits.iter()).find_map(|t| match t {
        TraitReference::Inline(td) if td.name == name => Some(&**td),
        _ => None,
    })
}

/// The composed traits of `orbital` as written: local name to the `Alias.traits.Y` path, where the two differ.
fn call_site_refs(schema: &OrbitalSchema, orbital: &str) -> HashMap<String, String> {
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

fn uses_of(schema: &OrbitalSchema) -> Vec<String> {
    let set: BTreeSet<String> = schema
        .orbitals
        .iter()
        .flat_map(|o| o.uses.iter().flatten())
        .map(|u| u.from.clone())
        .collect();
    set.into_iter().collect()
}

fn canonical_name(resolved: &Resolved, fallback: &str) -> String {
    match (&resolved.package, resolved.path.file_stem().and_then(|s| s.to_str())) {
        (Some(pkg), Some(name)) => format!("{}/{name}", pkg.prefix),
        _ => fallback.to_string(),
    }
}

fn describe_behavior(behavior: &str, only_orbital: Option<&str>) -> anyhow::Result<BehaviorDescription> {
    let resolved = resolve_from(behavior, None)?;
    let entry = resolved_program(&resolved, behavior)?;
    let program = entry.schema();
    let canonical = canonical_name(&resolved, behavior);
    let program_json = serde_json::to_value(&program)?;
    let program_events: HashSet<String> = program.orbitals.iter().flat_map(orbital_event_names).collect();
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
        let refs = call_site_refs(&resolved.schema, &orbital.name);
        let mut traits = Vec::new();
        for t in &orbital.traits {
            let TraitReference::Inline(td) = t else { continue };
            let own = trait_event_names(td);
            traits.push(TraitOut {
                rebindable: td.entity_rebindable == Some(true),
                reference: refs.get(&td.name).cloned(),
                name: td.name.clone(),
                config: declared_config(td.config.as_ref()),
                knobs: knob_fields(
                    td.config.as_ref(),
                    &EventVocab {
                        scalar: &own,
                        member: &orbital_events,
                    },
                )?,
                events: events_of(td),
            });
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
            config: declared_config(orbital.config.as_ref()),
            knobs: knob_fields(
                orbital.config.as_ref(),
                &EventVocab {
                    scalar: &orbital_events,
                    member: &orbital_events,
                },
            )?,
            expects: orbital_core::expects::derive_expectations(&program_json, &orbital.name, None)
                .map_err(|e| anyhow::anyhow!("cannot derive `{}` expectations: {e}", orbital.name))?
                .expectations,
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
    Ok(BehaviorDescription {
        behavior: canonical,
        config: declared_config(program.config.as_ref()),
        knobs: knob_fields(
            program.config.as_ref(),
            &EventVocab {
                scalar: &program_events,
                member: &program_events,
            },
        )?,
        orbitals,
        uses: uses_of(&resolved.schema),
        header: described_header(&program_json),
    })
}

fn describe_trait(value: Option<Value>, behavior: &str, trait_name: &str, from: Option<&Path>) -> anyhow::Result<Value> {
    let resolved = resolve_from(behavior, from)?;
    let entry = resolved_program(&resolved, behavior)?;
    let program = entry.schema();
    let td = inline_trait(program, trait_name).ok_or_else(|| anyhow::anyhow!("no inline trait `{trait_name}` in `{}`", program.name))?;
    let mut keys: Vec<(&String, &ConfigField)> = td.config.iter().flatten().collect();
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
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let source = Some(orbital_lolo::unlower::print_trait(td).map_err(|e| anyhow::anyhow!("cannot print `{trait_name}`: {e}"))?);
    let value = value.unwrap_or_else(|| serde_json::json!({ "behavior": canonical_name(&resolved, behavior), "trait": trait_name }));
    Ok(serde_json::to_value(TraitDescription {
        value,
        knobs,
        events: events_of(td),
        machine: Some(MachineOut {
            linked_entity: td.linked_entity.clone(),
            state_machine: (*td.state_machine).clone(),
        }),
        source,
        model: String::new(),
    })?)
}

/// The description a target asks for; a relative trait value resolves from the program file `from`.
pub(crate) fn describe(target: DescribeTarget, from: Option<&Path>) -> anyhow::Result<Value> {
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
            let behavior = behavior_part(&spec);
            if behavior.len() == spec.len() {
                anyhow::bail!("`{spec}` is not `<behavior>{TRAITS_SEGMENT}<Trait>`");
            }
            let name = &spec[behavior.len() + TRAITS_SEGMENT.len()..];
            if name.is_empty() {
                anyhow::bail!("`{spec}` names no trait after `{TRAITS_SEGMENT}`");
            }
            (None, behavior.to_string(), name.to_string())
        }
        DescribeTarget::Spec(spec) => {
            let (behavior, orbital) = match spec.rfind(ORBITALS_SEGMENT).filter(|&i| i > 0) {
                Some(i) => (spec[..i].to_string(), Some(spec[i + ORBITALS_SEGMENT.len()..].to_string())),
                None => (spec.clone(), None),
            };
            return Ok(serde_json::to_value(describe_behavior(&behavior, orbital.as_deref())?)?);
        }
    };
    describe_trait(value, &behavior, &trait_name, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(header: &str) -> serde_json::Value {
        // The describer resolves behaviors under the working directory only.
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let path = dir.path().join("shop.orb");
        let program = format!(
            r#"{{"name":"Shop","version":"1.0.0"{header},"orbitals":[{{"name":"Shop","entity":{{"name":"ShopState","persistence":"runtime","fields":[{{"name":"id","type":"string","required":true}}]}},"traits":[{{"name":"Counter","linkedEntity":"ShopState","stateMachine":{{"states":[{{"name":"idle","isInitial":true}}],"events":[{{"key":"INIT","name":"Init"}}],"transitions":[{{"from":"idle","to":"idle","event":"INIT","effects":[]}}]}}}}],"pages":[]}}]}}"#
        );
        std::fs::write(&path, program).unwrap();
        serde_json::to_value(describe_behavior(&path.display().to_string(), None).unwrap()).unwrap()
    }

    #[test]
    fn a_behavior_description_carries_the_theme_and_locales_its_header_declares() {
        let description = described(r#","theme":"linear-clean-light","locales":["en","ar"]"#);
        assert_eq!(description["header"]["theme"], "linear-clean-light", "{description}");
        assert_eq!(description["header"]["locales"], serde_json::json!(["en", "ar"]));
    }

    #[test]
    fn control_a_behavior_with_no_theme_or_locales_describes_no_header() {
        let description = described("");
        assert!(description.get("header").is_none(), "{description}");
        assert!(description["orbitals"].as_array().is_some_and(|o| o.len() == 1), "control: the rest is described");
    }

    #[test]
    fn edge_a_header_with_only_locales_carries_only_locales() {
        let description = described(r#","locales":["sl"]"#);
        assert_eq!(description["header"], serde_json::json!({ "locales": ["sl"] }));
    }
}
