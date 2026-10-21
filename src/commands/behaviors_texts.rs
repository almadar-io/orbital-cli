//! `orb behaviors texts` — the strings an installed behavior package embeds, one
//! `{ key, text, hash }` per vector: the organism text, its `<name>#capabilities`
//! text and every knob text. `orb` owns them so the embedding bake and the
//! freshness check read one source (port of `@almadar/calibrate`'s
//! `document-text.ts` / `embed-command.ts`).

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Write;

use super::behaviors_catalog::{scoped_behaviors, text_facts};
use super::behaviors_index::IndexEntry;

pub(crate) const CAPABILITY_KEY_SUFFIX: &str = "#capabilities";
pub(crate) const ORBITAL_KNOB_SENTINEL: &str = "$orbital";
const SUBJECT_FIELD_LIMIT: usize = 10;

/// One knob as the embedding texts read it.
#[derive(Debug, Clone, Default)]
pub(crate) struct KnobFacts {
    pub key: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub synonyms: Option<String>,
    pub enum_values: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TraitFacts {
    pub name: String,
    /// The upstream `Alias.traits.Y` this trait composes; absent for a trait the behavior defines.
    pub reference: Option<String>,
    pub capabilities: Vec<String>,
    pub knobs: Vec<KnobFacts>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EntityFacts {
    pub name: String,
    pub field_names: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OrbitalFacts {
    pub organism: String,
    pub orbital: String,
    pub entity: Option<EntityFacts>,
    pub traits: Vec<TraitFacts>,
    /// Knobs declared on the orbital itself, not forwarded through a trait.
    pub knobs: Vec<KnobFacts>,
}

/// The registry fields the organism text reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegistryEntry {
    pub name: String,
    pub description: Option<String>,
    pub capabilities: Option<String>,
    pub default_entity_name: Option<String>,
    pub connectable_events: Vec<String>,
    pub exposure: Option<String>,
}

impl From<&IndexEntry> for RegistryEntry {
    fn from(e: &IndexEntry) -> Self {
        Self {
            name: e.name.clone(),
            description: Some(e.description.clone()),
            capabilities: e.capabilities.clone(),
            default_entity_name: Some(e.default_entity_name.clone()),
            connectable_events: e.connectable_events.clone(),
            exposure: e.exposure.clone(),
        }
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

/// CamelCase to space-separated words ("NoteCreate" to "Note Create"): a space
/// between `[a-z0-9]` and `[A-Z]`, then between the last two capitals of a
/// capital run that a lowercase letter follows ("HTMLParser" to "HTML Parser").
pub(crate) fn decamel(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut first = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            let p = chars[i - 1];
            if p.is_ascii_lowercase() || p.is_ascii_digit() {
                first.push(' ');
            }
        }
        first.push(c);
    }
    let chars: Vec<char> = first.chars().collect();
    let mut out = String::with_capacity(first.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if i >= 1
            && chars[i - 1].is_ascii_uppercase()
            && c.is_ascii_lowercase()
            && i >= 2
            && chars[i - 2].is_ascii_uppercase()
        {
            // the space goes before chars[i - 1], already pushed
            let last = out.pop();
            out.push(' ');
            out.extend(last);
        }
        out.push(c);
    }
    out
}

fn entity_clause(entity: Option<&EntityFacts>) -> Option<String> {
    let entity = entity?;
    let fields = entity
        .field_names
        .iter()
        .take(SUBJECT_FIELD_LIMIT)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    Some(if fields.is_empty() {
        format!("for {} records", entity.name)
    } else {
        format!(
            "for {} records — {} fields: {fields}",
            entity.name, entity.name
        )
    })
}

fn knob_subject_line(sig: &OrbitalFacts, tr: &TraitFacts) -> String {
    let mut pieces = vec![format!("the {}", decamel(&tr.name))];
    if let Some(ref_trait) = tr
        .reference
        .as_deref()
        .and_then(|r| r.rsplit('.').next())
        .filter(|s| !s.is_empty())
    {
        pieces.push(format!("(a {})", decamel(ref_trait).to_lowercase()));
    }
    pieces.extend(entity_clause(sig.entity.as_ref()));
    format!("Subject: {}", pieces.join(" "))
}

fn orbital_knob_subject_line(sig: &OrbitalFacts) -> String {
    let mut pieces = vec![format!("the {} orbital", decamel(&sig.orbital))];
    pieces.extend(entity_clause(sig.entity.as_ref()));
    format!("Subject: {}", pieces.join(" "))
}

fn knob_parts(head: String, subject: String, knob: &KnobFacts) -> String {
    let mut parts = vec![head];
    parts.extend(non_empty(&knob.label).map(str::to_string));
    parts.extend(non_empty(&knob.description).map(str::to_string));
    parts.push(subject);
    if !knob.enum_values.is_empty() {
        parts.push(format!("values: {}", knob.enum_values.join(", ")));
    }
    if let Some(s) = non_empty(&knob.synonyms) {
        parts.push(format!("synonyms: {s}"));
    }
    parts.join("\n")
}

pub(crate) fn build_knob_text(sig: &OrbitalFacts, tr: &TraitFacts, knob: &KnobFacts) -> String {
    knob_parts(
        format!("{} {} {}.{}", sig.organism, sig.orbital, tr.name, knob.key),
        knob_subject_line(sig, tr),
        knob,
    )
}

pub(crate) fn build_orbital_knob_text(sig: &OrbitalFacts, knob: &KnobFacts) -> String {
    knob_parts(
        format!(
            "{} {} {ORBITAL_KNOB_SENTINEL}.{}",
            sig.organism, sig.orbital, knob.key
        ),
        orbital_knob_subject_line(sig),
        knob,
    )
}

pub(crate) fn build_capability_text(entry: &RegistryEntry) -> Option<String> {
    non_empty(&entry.capabilities).map(|c| format!("Capabilities (organism): {c}"))
}

pub(crate) fn build_entry_text(entry: &RegistryEntry, traits: &[&TraitFacts]) -> String {
    let mut parts = vec![entry.name.clone()];
    parts.extend(non_empty(&entry.description).map(str::to_string));
    parts.extend(build_capability_text(entry));
    parts.extend(
        entry
            .default_entity_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(|n| format!("Entity: {n}")),
    );
    if !entry.connectable_events.is_empty() {
        parts.push(format!("Events: {}", entry.connectable_events.join(" ")));
    }
    let capabilities: BTreeSet<&str> = traits
        .iter()
        .flat_map(|t| t.capabilities.iter().map(String::as_str))
        .collect();
    if !capabilities.is_empty() {
        parts.push(format!(
            "Capabilities: {}",
            capabilities.into_iter().collect::<Vec<_>>().join(" ")
        ));
    }
    parts.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
    Organism,
    Capability,
    Knob,
}

impl TextKind {
    pub(crate) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "organism" => Ok(Self::Organism),
            "capability" => Ok(Self::Capability),
            "knob" => Ok(Self::Knob),
            other => anyhow::bail!("unknown --kind `{other}` (one of organism, capability, knob)"),
        }
    }
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct TextEntry {
    pub key: String,
    pub text: String,
    pub hash: String,
}

fn entry_of(key: String, text: String) -> TextEntry {
    let hash = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    TextEntry { key, text, hash }
}

/// One package's texts in embed order: organisms, capabilities, then knobs, each in registry order.
pub(crate) fn texts_of(
    registry: &[(String, RegistryEntry)],
    facts: &[Vec<OrbitalFacts>],
    kind: Option<TextKind>,
) -> Vec<TextEntry> {
    let want = |k: TextKind| kind.is_none() || kind == Some(k);
    let mut out = Vec::new();
    if want(TextKind::Organism) {
        for ((key, entry), orbitals) in registry.iter().zip(facts) {
            let traits: Vec<&TraitFacts> = orbitals.iter().flat_map(|o| &o.traits).collect();
            out.push(entry_of(key.clone(), build_entry_text(entry, &traits)));
        }
    }
    if want(TextKind::Capability) {
        for (key, entry) in registry {
            if let Some(text) = build_capability_text(entry) {
                out.push(entry_of(format!("{key}{CAPABILITY_KEY_SUFFIX}"), text));
            }
        }
    }
    if want(TextKind::Knob) {
        for ((_, entry), orbitals) in registry.iter().zip(facts) {
            if entry.exposure.as_deref() == Some("internal") {
                continue;
            }
            for sig in orbitals {
                for tr in &sig.traits {
                    for knob in &tr.knobs {
                        out.push(entry_of(
                            format!("{}/{}/{}/{}", sig.organism, sig.orbital, tr.name, knob.key),
                            build_knob_text(sig, tr, knob),
                        ));
                    }
                }
                for knob in &sig.knobs {
                    out.push(entry_of(
                        format!(
                            "{}/{}/{ORBITAL_KNOB_SENTINEL}/{}",
                            sig.organism, sig.orbital, knob.key
                        ),
                        build_orbital_knob_text(sig, knob),
                    ));
                }
            }
        }
    }
    out
}

/// The organism text and the `#capabilities` text of one index entry, from the entry alone: the
/// capabilities its traits resolve to ride in the entry as `trait_capabilities`, so no behavior is
/// described. Equal to `texts_of`'s organism and capability texts for the same entry.
pub(crate) fn index_texts(entry: &IndexEntry) -> (String, Option<String>) {
    let registry = RegistryEntry::from(entry);
    let traits = TraitFacts {
        capabilities: entry.trait_capabilities.clone(),
        ..Default::default()
    };
    (
        build_entry_text(&registry, &[&traits]),
        build_capability_text(&registry),
    )
}

const DESCRIBE_WORKERS: usize = 8;

/// Describe every behavior of `registry` that `kind` needs facts for, in parallel.
pub(crate) fn describe_all(
    prefix: &str,
    registry: &[(String, RegistryEntry)],
    kind: Option<TextKind>,
) -> anyhow::Result<Vec<Vec<OrbitalFacts>>> {
    if kind == Some(TextKind::Capability) {
        return Ok(vec![Vec::new(); registry.len()]);
    }
    let skip_internal = kind == Some(TextKind::Knob);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<anyhow::Result<Vec<OrbitalFacts>>>>> = registry
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..DESCRIBE_WORKERS.min(registry.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some((name, entry)) = registry.get(i) else {
                    break;
                };
                let facts = if skip_internal && entry.exposure.as_deref() == Some("internal") {
                    Ok(Vec::new())
                } else {
                    text_facts(&format!("{prefix}/{name}"), name)
                };
                if let Ok(mut slot) = slots[i].lock() {
                    *slot = Some(facts);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .map_err(|_| anyhow::anyhow!("a describe worker panicked"))?
                .ok_or_else(|| anyhow::anyhow!("a behavior was never described"))?
        })
        .collect()
}

/// `orb behaviors texts`: every embedding text under the scopes, as a JSON array.
pub(crate) fn run_texts(paths: Vec<String>, kind: Option<String>) -> anyhow::Result<()> {
    let all = texts(&paths, kind.as_deref())?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(1 << 20, stdout.lock());
    serde_json::to_writer(&mut out, &all)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Every embedding text under the scopes, in embed order: what `orb behaviors texts` prints.
pub(crate) fn texts(paths: &[String], kind: Option<&str>) -> anyhow::Result<Vec<TextEntry>> {
    let kind = kind.map(TextKind::parse).transpose()?;
    let mut all: Vec<TextEntry> = Vec::new();
    for package in scoped_behaviors(paths)? {
        let registry: Vec<(String, RegistryEntry)> = package
            .behaviors
            .iter()
            .map(|e| (e.name.clone(), RegistryEntry::from(e)))
            .collect();
        let facts = describe_all(&package.prefix, &registry, kind)?;
        all.extend(texts_of(&registry, &facts, kind));
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knob(
        key: &str,
        label: &str,
        description: &str,
        values: &[&str],
        synonyms: &str,
    ) -> KnobFacts {
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        KnobFacts {
            key: key.to_string(),
            label: opt(label),
            description: opt(description),
            synonyms: opt(synonyms),
            enum_values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn entity(name: &str, fields: &[&str]) -> Option<EntityFacts> {
        Some(EntityFacts {
            name: name.to_string(),
            field_names: fields.iter().map(|f| f.to_string()).collect(),
        })
    }

    #[test]
    fn decamel_splits_words_digits_and_acronyms() {
        assert_eq!(decamel("NoteCreate"), "Note Create");
        assert_eq!(decamel("Learn3DSimTransport"), "Learn3 D Sim Transport");
        assert_eq!(decamel("AgentProposalOrbital"), "Agent Proposal Orbital");
        assert_eq!(decamel("HTMLParser"), "HTML Parser");
        assert_eq!(decamel("ABCdEFg"), "AB Cd E Fg");
        assert_eq!(decamel("already spaced"), "already spaced");
        assert_eq!(decamel("UI"), "UI");
        assert_eq!(decamel(""), "");
    }

    #[test]
    fn entry_text_joins_every_declared_part_in_order() {
        let entry = RegistryEntry {
            name: "std-agent-fix-loop".into(),
            description: Some("Agent fix loop.".into()),
            capabilities: Some("program repair, evaluate a program".into()),
            default_entity_name: Some("FixLoop".into()),
            connectable_events: vec!["VALIDATE_PROGRAM".into(), "RESET".into()],
            ..Default::default()
        };
        let own = TraitFacts {
            capabilities: vec!["b".into(), "a".into()],
            ..Default::default()
        };
        let other = TraitFacts {
            capabilities: vec!["a".into(), "c".into()],
            ..Default::default()
        };
        assert_eq!(
            build_entry_text(&entry, &[&own, &other]),
            "std-agent-fix-loop\nAgent fix loop.\nCapabilities (organism): program repair, evaluate a program\nEntity: FixLoop\nEvents: VALIDATE_PROGRAM RESET\nCapabilities: a b c"
        );
    }

    #[test]
    fn entry_text_of_a_bare_atom_is_its_name() {
        let entry = RegistryEntry {
            name: "std-x".into(),
            description: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(build_entry_text(&entry, &[]), "std-x");
        assert_eq!(build_capability_text(&entry), None);
    }

    #[test]
    fn capability_text_carries_only_the_organism_phrases() {
        let entry = RegistryEntry {
            name: "std-agent-fix-loop".into(),
            capabilities: Some("program repair, fix validation errors".into()),
            ..Default::default()
        };
        assert_eq!(
            build_capability_text(&entry).as_deref(),
            Some("Capabilities (organism): program repair, fix validation errors")
        );
    }

    #[test]
    fn trait_knob_text_with_values_synonyms_and_a_wrapped_atom() {
        let sig = OrbitalFacts {
            organism: "std-approval-gate".into(),
            orbital: "ApprovalGateOrbital".into(),
            entity: entity(
                "ApprovalRequest",
                &[
                    "gatedEvent",
                    "subjectLabel",
                    "subjectId",
                    "requestedBy",
                    "requestedByName",
                    "status",
                    "reviewer",
                    "requestedAt",
                    "decidedAt",
                    "escalatedAt",
                    "extra",
                ],
            ),
            ..Default::default()
        };
        let tr = TraitFacts {
            name: "LoadingSpinner".into(),
            reference: Some("Spinner.traits.SpinnerRender".into()),
            ..Default::default()
        };
        let k = knob(
            "ariaCurrent",
            "Aria-current",
            "Marks the current item in a set: the current page, step, location, date or time.",
            &["page", "step", "location", "date", "time", "true", "false"],
            "aria-current",
        );
        assert_eq!(
            build_knob_text(&sig, &tr, &k),
            "std-approval-gate ApprovalGateOrbital LoadingSpinner.ariaCurrent\nAria-current\nMarks the current item in a set: the current page, step, location, date or time.\nSubject: the Loading Spinner (a spinner render) for ApprovalRequest records — ApprovalRequest fields: gatedEvent, subjectLabel, subjectId, requestedBy, requestedByName, status, reviewer, requestedAt, decidedAt, escalatedAt\nvalues: page, step, location, date, time, true, false\nsynonyms: aria-current"
        );
    }

    #[test]
    fn trait_knob_text_without_a_ref_or_fields() {
        let sig = OrbitalFacts {
            organism: "std-agent-suggestions".into(),
            orbital: "AgentSuggestionsOrbital".into(),
            entity: entity("AgentSuggestionsHost", &[]),
            ..Default::default()
        };
        let tr = TraitFacts {
            name: "AgentSuggestionProposer".into(),
            ..Default::default()
        };
        let k = knob(
            "allowedActions",
            "Which actions may a suggestion carry?",
            "When listed, any other action is refused. Empty accepts any.",
            &[],
            "allowed actions, commands, events",
        );
        assert_eq!(
            build_knob_text(&sig, &tr, &k),
            "std-agent-suggestions AgentSuggestionsOrbital AgentSuggestionProposer.allowedActions\nWhich actions may a suggestion carry?\nWhen listed, any other action is refused. Empty accepts any.\nSubject: the Agent Suggestion Proposer for AgentSuggestionsHost records\nsynonyms: allowed actions, commands, events"
        );
    }

    #[test]
    fn trait_knob_text_decamels_digits_and_omits_an_entityless_subject() {
        let sig = OrbitalFacts {
            organism: "std-learn-sim-3d".into(),
            orbital: "LearnSim3DOrbital".into(),
            entity: entity(
                "Learn3DSimScene",
                &[
                    "active",
                    "title",
                    "running",
                    "speedMultiplier",
                    "steps",
                    "simTime",
                ],
            ),
            ..Default::default()
        };
        let tr = TraitFacts {
            name: "Learn3DSimTransport".into(),
            ..Default::default()
        };
        let text = build_knob_text(&sig, &tr, &knob("dt", "", "", &[], ""));
        assert_eq!(
            text,
            "std-learn-sim-3d LearnSim3DOrbital Learn3DSimTransport.dt\nSubject: the Learn3 D Sim Transport for Learn3DSimScene records — Learn3DSimScene fields: active, title, running, speedMultiplier, steps, simTime"
        );
        let bare = OrbitalFacts {
            organism: "o".into(),
            orbital: "P".into(),
            ..Default::default()
        };
        assert_eq!(
            build_knob_text(&bare, &tr, &knob("k", "", "", &[], "")),
            "o P Learn3DSimTransport.k\nSubject: the Learn3 D Sim Transport"
        );
    }

    #[test]
    fn orbital_knob_text_uses_the_sentinel_and_names_the_orbital() {
        let sig = OrbitalFacts {
            organism: "std-agent-proposal".into(),
            orbital: "AgentProposalOrbital".into(),
            entity: entity(
                "AgentProposalRun",
                &[
                    "input", "context", "request", "feedback", "summary", "items", "error",
                ],
            ),
            ..Default::default()
        };
        let k = knob(
            "tools",
            "What may the agent do?",
            "The declared inputs it may fire and the entities it may read, as the signed-in person. An importing app restates the list with PROPOSE addressed by its local orbital name, which the agent can never propose without.",
            &["AgentToolScope", "AgentToolInput", "AgentToolRead"],
            "allowlist, abilities, permissions",
        );
        assert_eq!(
            build_orbital_knob_text(&sig, &k),
            "std-agent-proposal AgentProposalOrbital $orbital.tools\nWhat may the agent do?\nThe declared inputs it may fire and the entities it may read, as the signed-in person. An importing app restates the list with PROPOSE addressed by its local orbital name, which the agent can never propose without.\nSubject: the Agent Proposal Orbital orbital for AgentProposalRun records — AgentProposalRun fields: input, context, request, feedback, summary, items, error\nvalues: AgentToolScope, AgentToolInput, AgentToolRead\nsynonyms: allowlist, abilities, permissions"
        );
    }

    #[test]
    fn texts_come_organisms_then_capabilities_then_knobs_and_skip_internal_knobs() {
        let registry = vec![
            (
                "a".to_string(),
                RegistryEntry {
                    name: "a".into(),
                    capabilities: Some("cap".into()),
                    ..Default::default()
                },
            ),
            (
                "b".to_string(),
                RegistryEntry {
                    name: "b".into(),
                    exposure: Some("internal".into()),
                    ..Default::default()
                },
            ),
        ];
        let sig = |organism: &str| OrbitalFacts {
            organism: organism.into(),
            orbital: "O".into(),
            traits: vec![TraitFacts {
                name: "T".into(),
                knobs: vec![knob("k", "", "", &[], "")],
                ..Default::default()
            }],
            knobs: vec![knob("own", "", "", &[], "")],
            ..Default::default()
        };
        let facts = vec![vec![sig("a")], vec![sig("b")]];
        let keys: Vec<String> = texts_of(&registry, &facts, None)
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys,
            ["a", "b", "a#capabilities", "a/O/T/k", "a/O/$orbital/own"]
        );
        let only_caps = texts_of(&registry, &facts, Some(TextKind::Capability));
        assert_eq!(only_caps.len(), 1);
        assert_eq!(only_caps[0].text, "Capabilities (organism): cap");
    }

    #[test]
    fn hash_is_the_lowercase_hex_sha256_of_the_utf8_text() {
        assert_eq!(
            entry_of("k".into(), "abc".into()).hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(entry_of("k".into(), "—".into()).hash.len(), 64);
    }

    fn index_entry(name: &str) -> IndexEntry {
        IndexEntry {
            name: name.into(),
            dirs: vec!["app".into(), "atoms".into()],
            level: "atom".into(),
            topic: "app".into(),
            exposure: None,
            description: String::new(),
            capabilities: None,
            pattern: None,
            default_entity_name: "Item".into(),
            connectable_events: Vec::new(),
            traits: Vec::new(),
            trait_capabilities: Vec::new(),
        }
    }

    #[test]
    fn registry_entry_reads_the_fields_the_texts_use() {
        let e = RegistryEntry::from(&IndexEntry {
            description: "d".into(),
            capabilities: Some("c1, c2".into()),
            default_entity_name: "E".into(),
            connectable_events: vec!["X".into(), "Y".into()],
            exposure: Some("internal".into()),
            ..index_entry("n")
        });
        assert_eq!(e.name, "n");
        assert_eq!(e.description.as_deref(), Some("d"));
        assert_eq!(e.capabilities.as_deref(), Some("c1, c2"));
        assert_eq!(e.default_entity_name.as_deref(), Some("E"));
        assert_eq!(e.connectable_events, ["X", "Y"]);
        assert_eq!(e.exposure.as_deref(), Some("internal"));
    }

    #[test]
    fn index_texts_equal_the_described_organism_and_capability_texts() {
        let entry = IndexEntry {
            description: "An x.".into(),
            capabilities: Some("do x, do y".into()),
            default_entity_name: "X".into(),
            connectable_events: vec!["GO".into(), "STOP".into()],
            traits: vec!["A".into(), "B".into(), "C".into()],
            trait_capabilities: vec!["a".into(), "b".into(), "c".into()],
            ..index_entry("std-x")
        };
        let (organism, capability) = index_texts(&entry);
        let registry = vec![("std-x".to_string(), RegistryEntry::from(&entry))];
        let facts = vec![vec![OrbitalFacts {
            traits: vec![
                TraitFacts {
                    name: "A".into(),
                    capabilities: vec!["a".into(), "b".into()],
                    ..Default::default()
                },
                TraitFacts {
                    name: "B".into(),
                    capabilities: vec!["a".into(), "c".into()],
                    ..Default::default()
                },
                TraitFacts {
                    name: "C".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }]];
        let want = texts_of(&registry, &facts, None);
        assert_eq!(want[0].text, organism);
        assert_eq!(want[1].key, "std-x#capabilities");
        assert_eq!(Some(want[1].text.clone()), capability);
        // Control: an entry whose traits resolve to no capabilities carries none in its text.
        let names_only = IndexEntry {
            traits: vec!["A".into()],
            default_entity_name: String::new(),
            ..index_entry("std-z")
        };
        assert_eq!(index_texts(&names_only), ("std-z".to_string(), None));
        // Control: an entry with no capabilities has no capability text.
        assert_eq!(index_texts(&index_entry("std-y")).1, None);
    }
}
