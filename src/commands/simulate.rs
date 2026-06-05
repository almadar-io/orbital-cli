//! `orb validate --simulate` — render simulation tracing reachable states.

use crate::utils::parse_schema_auto;
use std::path::PathBuf;

/// Simulate render output — trace every reachable state and report what render-ui effects fire.
/// This is the compile-time equivalent of running the app and inspecting what renders.
pub(crate) fn simulate_render(
    path: &PathBuf,
    _patterns_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use orbital_core::schema::{SExpression, TraitReference};
    use std::collections::{HashSet, VecDeque};

    let content_raw = std::fs::read_to_string(path)?;
    let schema = parse_schema_auto(&content_raw)?;

    println!("\n🔍 Render Simulation");
    println!("{}", "=".repeat(60));

    for orbital in &schema.orbitals {
        // Get entity name
        let _entity_name = match &orbital.entity {
            orbital_core::EntityRef::Inline(e) => e.name.as_str(),
            orbital_core::EntityRef::Reference(r) => r.as_str(),
            // Phase 3.5.G.A.5: `Extends` is rewritten to `Inline` by the
            // inline phase before this CLI command sees the schema. If one
            // somehow leaks through, fall through with the upstream
            // `extends` reference, mirroring the bare `Reference` arm.
            orbital_core::EntityRef::Extends(e) => e.reference.as_str(),
        };

        // Get entity field names for checking
        let _entity_fields: Vec<&str> = match &orbital.entity {
            orbital_core::EntityRef::Inline(e) => {
                e.fields.iter().filter_map(|f| f.name.as_deref()).collect()
            }
            _ => vec![],
        };

        // Process pages
        if let Some(pages) = &orbital.pages {
            for page_ref in pages {
                let page = match page_ref.as_inline() {
                    Some(p) => p,
                    None => continue,
                };

                println!("\nPage: {} ({})", page.name, page.path);

                // Collect trait definitions referenced by this page
                let page_trait_names: HashSet<String> = page
                    .traits
                    .as_ref()
                    .map(|t| {
                        t.iter()
                            .filter_map(|tr| match tr {
                                TraitReference::Inline(td) => Some(td.name.clone()),
                                TraitReference::Reference {
                                    reference, name, ..
                                } => reference.clone().or_else(|| name.clone()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Process each trait on this page
                for trait_ref in &orbital.traits {
                    let trait_def = match trait_ref {
                        TraitReference::Inline(td) => td,
                        _ => continue,
                    };

                    if !page_trait_names.contains(&trait_def.name) {
                        continue;
                    }

                    // Find initial state
                    let initial_state = trait_def
                        .state_machine
                        .states
                        .iter()
                        .find(|s| s.is_initial == Some(true))
                        .map(|s| s.name.as_str())
                        .unwrap_or("(unknown)");

                    // BFS through reachable states
                    let mut visited: HashSet<&str> = HashSet::new();
                    let mut queue: VecDeque<&str> = VecDeque::new();
                    queue.push_back(initial_state);
                    visited.insert(initial_state);

                    while let Some(state) = queue.pop_front() {
                        let is_initial = state == initial_state;
                        println!(
                            "  State: {}{}",
                            state,
                            if is_initial { " (initial)" } else { "" }
                        );

                        // Find transitions FROM this state
                        for transition in &trait_def.state_machine.transitions {
                            let from_matches = match &transition.from {
                                orbital_core::schema::StringOrArray::Single(s) => s == state,
                                orbital_core::schema::StringOrArray::Multiple(v) => {
                                    v.iter().any(|s| s == state)
                                }
                            };

                            if !from_matches {
                                continue;
                            }

                            let has_guard = transition.guard.is_some();
                            println!(
                                "    {} transition{}:",
                                transition.event,
                                if has_guard { " (guarded)" } else { "" }
                            );

                            // Collect effects
                            let mut has_fetch = false;
                            let mut fetch_entities: Vec<String> = Vec::new();
                            let mut render_effects: Vec<(String, String, Vec<String>)> = Vec::new(); // (slot, pattern_type, props)

                            if let Some(effects) = &transition.effects {
                                for effect in effects {
                                    if let SExpression::Array(arr) = effect {
                                        if arr.is_empty() {
                                            continue;
                                        }
                                        let op = match &arr[0] {
                                            SExpression::String(s) => s.as_str(),
                                            _ => continue,
                                        };

                                        match op {
                                            "fetch" => {
                                                has_fetch = true;
                                                if let Some(SExpression::String(e)) = arr.get(1) {
                                                    fetch_entities.push(e.clone());
                                                }
                                            }
                                            "render-ui" | "render_ui" => {
                                                let slot = match arr.get(1) {
                                                    Some(SExpression::String(s)) => s.clone(),
                                                    _ => "?".to_string(),
                                                };
                                                let (pattern_type, props) = match arr.get(2) {
                                                    Some(SExpression::Object(obj)) => {
                                                        let pt = match obj.get("type") {
                                                            Some(SExpression::String(t)) => {
                                                                t.clone()
                                                            }
                                                            _ => "?".to_string(),
                                                        };
                                                        let ps: Vec<String> = obj
                                                            .keys()
                                                            .filter(|k| *k != "type")
                                                            .cloned()
                                                            .collect();
                                                        (pt, ps)
                                                    }
                                                    Some(SExpression::Null) => {
                                                        ("null (clear)".to_string(), vec![])
                                                    }
                                                    _ => ("?".to_string(), vec![]),
                                                };
                                                render_effects.push((slot, pattern_type, props));
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            // Report fetches
                            if has_fetch {
                                for fe in &fetch_entities {
                                    println!("      📦 fetch \"{}\"", fe);
                                }
                            }

                            // Report render-ui
                            for (slot, pattern_type, props) in &render_effects {
                                let props_str = if props.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", props.join(", "))
                                };
                                let status = if pattern_type == "null (clear)" {
                                    "🗑️"
                                } else {
                                    "✅"
                                };
                                println!(
                                    "      {} Slot \"{}\" ← [{}]{}",
                                    status, slot, pattern_type, props_str
                                );

                                // Check for entity prop without fetch
                                if !has_fetch && props.contains(&"entity".to_string()) {
                                    println!(
                                        "         ⚠️  WARNING: entity prop present but no fetch in this transition"
                                    );
                                }
                            }

                            if render_effects.is_empty() && !has_fetch {
                                println!("      (no render-ui effects)");
                            }

                            // Enqueue target state
                            let to = transition.to.as_str();
                            if !visited.contains(to) {
                                visited.insert(to);
                                queue.push_back(to);
                            }
                        }
                    }

                    // Report unreachable states
                    let all_states: HashSet<&str> = trait_def
                        .state_machine
                        .states
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    let unreachable: Vec<&&str> = all_states.difference(&visited).collect();
                    if !unreachable.is_empty() {
                        println!("  ⚠️  Unreachable states: {:?}", unreachable);
                    }
                }
            }
        }
    }

    println!("\n{}", "=".repeat(60));
    println!("Simulation complete.\n");

    Ok(())
}
