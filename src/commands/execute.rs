//! `orb execute` — pure headless state-machine execution, JSON-DTO output.
//!
//! Mirrors the TypeScript parity harness's pure `processEvent` walk
//! (`packages/almadar-parity/src/harness/ts-harness.ts` `executeWithRuntime`)
//! field-for-field against `ExecutionResult`
//! (`packages/almadar-parity/src/harness/types.ts:60`) — the two are the
//! `rust-harness.ts`/`ts-harness.ts` pair `packages/almadar-parity` diffs to
//! measure Rust/TypeScript parity. Effects are NOT executed here, only
//! reported — this walks the same pure kernel
//! (`orbital_core::StateMachineCore::process_event`) the exhaustive test
//! generator uses, never the effect executor.

use std::path::Path;

use orbital_core::{TraitDefinition, Value};

use crate::utils::parse_schema_auto;

fn to_core_value(json: &serde_json::Value) -> anyhow::Result<Value> {
    serde_json::from_value(json.clone())
        .map_err(|e| anyhow::anyhow!("Failed to convert JSON to evaluator value: {}", e))
}

/// One trait's outcome of a dispatched step: the transition, the emits, the arm that fired,
/// every guard tried and how each authored effect of the fired arm ran.
fn outcome_report(
    trait_name: &str,
    r: &orbital_core::runtime::ProcessEventResult,
) -> serde_json::Value {
    use orbital_core::effects::Effect;
    let emits: Vec<serde_json::Value> = r
        .effect_results
        .iter()
        .filter(|er| !er.is_failed())
        .filter_map(|er| match &er.effect {
            Effect::Emit { event, payload } => Some(serde_json::json!({
                "event": event,
                "payload": serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
            })),
            _ => None,
        })
        .collect();
    let guards: Vec<serde_json::Value> = r
        .guards
        .iter()
        .map(|g| {
            let mut entry = serde_json::json!({
                "arm": g.arm,
                "guard": serde_json::to_value(&*g.guard.guard).unwrap_or(serde_json::Value::Null),
                "passed": g.guard.passed,
            });
            if let Some(error) = &g.guard.error {
                entry["error"] = serde_json::Value::String(error.clone());
            }
            entry
        })
        .collect();
    let effects: Vec<serde_json::Value> = r
        .effect_runs
        .iter()
        .map(|run| {
            serde_json::to_value(orbital_core::program_machine::played_effect(run))
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    serde_json::json!({
        "trait": trait_name,
        "executed": r.executed,
        "previousState": r.previous_state,
        "newState": r.new_state,
        "emits": emits,
        "firedArm": r.fired_arm,
        "guards": guards,
        "effects": effects,
        "failure": r.failure,
    })
}

/// The program text with each step's `config` knob values written over its trait's declared defaults; `None` when
/// no step overrides a knob. The compiler folds `@config` into the program at resolve time, so an
/// override is the program's for the whole call: two steps giving one trait's knob different values are refused.
fn with_step_config(path: &Path, steps: &[serde_json::Value]) -> anyhow::Result<Option<String>> {
    let mut knobs: std::collections::BTreeMap<(String, String), serde_json::Value> = Default::default();
    for (i, step) in steps.iter().enumerate() {
        match &step["config"] {
            serde_json::Value::Null => {}
            serde_json::Value::Object(values) => {
                let trait_name = step["trait"].as_str().unwrap_or_default().to_string();
                for (knob, value) in values {
                    match knobs.insert((trait_name.clone(), knob.clone()), value.clone()) {
                        Some(earlier) if &earlier != value => anyhow::bail!(
                            "step {i}: config.{knob} of {trait_name} is {value}, but an earlier step set it to {earlier}; \
                             one call takes one value per knob (the compiler folds @config at resolve time)"
                        ),
                        _ => {}
                    }
                }
            }
            _ => anyhow::bail!("step {i}: `config` must be an object of knob values"),
        }
    }
    if knobs.is_empty() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;
    let mut program: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("step config: {} is not .orb JSON: {e}", path.display()))?;
    for ((trait_name, knob), value) in knobs {
        let declared = program["orbitals"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .filter_map(|o| o["traits"].as_array_mut())
            .flatten()
            .find(|t| t["name"] == trait_name.as_str() && t.get("stateMachine").is_some())
            .and_then(|t| t.get_mut("config"))
            .and_then(|c| c.get_mut(&knob))
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("step config: {trait_name} declares no knob `{knob}`"))?;
        declared.insert("default".to_string(), value);
    }
    Ok(Some(serde_json::to_string(&program)?))
}

/// The temp directory a live call's resolved program was written to, removed when the call ends.
struct ResolvedTempDir(std::path::PathBuf);

impl Drop for ResolvedTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A live step waits for its open keyed calls (a spawned process, a stream) to end. With the request's `settleMs`,
/// a call still open after it is cancelled through `cancel-call`; without it the step waits, never cutting one off.
fn settle_open_calls(host: &orbital_shell_rust::host::Host, settle_ms: Option<f64>) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    match settle_ms {
        Some(ms) => {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms.max(0.0) as u64));
                let _ = stop.send(());
            });
            orbital_shell_rust::host::run::hold(host, &stopped);
        }
        None => {
            orbital_shell_rust::host::run::hold(host, &stopped);
            drop(stop);
        }
    }
}

/// A step's host answer: a keyed os/* or http ask is left pending (its answer event is a later step's to play,
/// G-ORB-459); every other ask is answered as a kernel with no host answers it.
fn step_answer(
    kernel: &orbital_core::runtime::RuntimeKernel,
    request: &orbital_core::runtime::resumable::IoRequest,
) -> orbital_core::runtime::resumable::IoAnswer {
    use orbital_core::runtime::resumable::{BlockingHost, IoAnswer, IoRequest};
    match request {
        IoRequest::Os { key: Some(_), .. }
        | IoRequest::Http { key: Some(_), .. }
        | IoRequest::HttpStream { key: Some(_), .. }
        | IoRequest::Substrate { key: Some(_), site: Some(_), .. } => IoAnswer::ServiceDeferred,
        other => BlockingHost { source: kernel.entity_source().as_ref(), service: None, substrate: None }.answer(other),
    }
}

/// The program at `path`, parsed and resolved the way a cascade runs it.
pub(crate) fn resolve_program(path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    resolve_parsed(parse_program_text(&content, path)?, path)
}

fn parse_program_text(content: &str, path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    parse_schema_auto(content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))
}

/// `schema`, read from `path`, resolved the way a cascade runs it.
fn resolve_parsed(schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    use orbital_compiler::{phases::lazy::resolve_program_with_path, CompilerOptions};
    let (resolved, _lazy) =
        resolve_program_with_path(schema, &CompilerOptions::from_env(), Some(path))
            .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    Ok(resolved)
}

/// [`run_cascade`]; `live` runs the steps on the native program host (`orbital_shell_rust::host`), so their os/* and
/// http asks are answered by this machine instead of left pending (G-ORB-459).
pub(crate) fn run_cascade_with(path: &Path, steps: &serde_json::Value, live: bool) -> anyhow::Result<serde_json::Value> {
    use orbital_core::runtime::{EntitySource, InMemoryEntityCache, RuntimeKernel};

    let steps = steps
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("--steps must be a JSON array"))?;
    let program = match with_step_config(path, steps)? {
        Some(configured) => parse_program_text(&configured, path)?,
        None => parse_program_text(
            &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?,
            path,
        )?,
    };
    let resolved = resolve_parsed(program.clone(), path)?;

    let pinned_now: std::sync::Arc<std::sync::Mutex<Option<f64>>> = Default::default();
    let clock_pin = std::sync::Arc::clone(&pinned_now);
    let clock: orbital_core::runtime::DispatchClock = std::sync::Arc::new(move || {
        clock_pin
            .lock()
            .ok()
            .and_then(|pin| *pin)
            .unwrap_or_else(orbital_core::time_util::now_millis_f64)
    });
    let trait_defs: std::collections::BTreeMap<String, TraitDefinition> =
        resolved
            .orbitals
            .iter()
            .flat_map(|o| o.traits.iter())
            .filter_map(|t| match t {
                orbital_core::schema::types::TraitReference::Inline(td) => Some((td.name.clone(), td.clone())),
                _ => None,
            })
            .collect();
    let (live_host, _live_files) = if live {
        let program = crate::commands::dev::resolve_schema_to_temp(program, path)?;
        let files = program.parent().map(|dir| ResolvedTempDir(dir.to_path_buf()));
        let config = orbital_shell_rust::host::HostConfig {
            substrate: Some(orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(
                crate::commands::program_host::ProgramHost,
            ))),
            clock: Some(clock.clone()),
            ..orbital_shell_rust::host::HostConfig::default()
        };
        let host = orbital_shell_rust::host::boot::boot_file_with(&program, false, &std::env::vars().collect(), false, config)?;
        (Some(host), files)
    } else {
        (None, None)
    };
    let replay_kernel;
    let kernel: &RuntimeKernel = match &live_host {
        Some(host) => host.kernel(),
        None => {
            replay_kernel = RuntimeKernel::new(
                orbital_core::RuntimeEnvironment::Server,
                std::sync::Arc::new(InMemoryEntityCache::new()) as std::sync::Arc<dyn EntitySource>,
                orbital_core::KernelConfig { clock: Some(clock), ..Default::default() },
            );
            replay_kernel
                .register_schema(resolved)
                .map_err(|e| anyhow::anyhow!("Failed to register {}: {e}", path.display()))?;
            &replay_kernel
        }
    };
    kernel.set_record_entity_after(true);

    let known = kernel.trait_snapshot(Some("singleton"));
    let mut reports = Vec::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        let field = |key: &str| {
            step[key]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("step {i}: `{key}` must be a string"))
        };
        let (trait_name, event) = (field("trait")?, field("event")?);
        if !known.contains_key(trait_name) {
            anyhow::bail!("step {i}: {} has no trait \"{trait_name}\"", path.display());
        }
        let seed = match &step["entity"] {
            serde_json::Value::Null => None,
            serde_json::Value::Object(fields) => Some(fields),
            _ => anyhow::bail!("step {i}: `entity` must be an object of fields"),
        };
        let viewer = match &step["viewer"] {
            serde_json::Value::Null => None,
            user @ serde_json::Value::Object(_) => Some(to_core_value(user)?),
            _ => anyhow::bail!("step {i}: `viewer` must be a user object or null"),
        };
        let settle_ms = match &step["settleMs"] {
            serde_json::Value::Null => None,
            serde_json::Value::Number(n) if live => n.as_f64(),
            serde_json::Value::Number(_) => anyhow::bail!("step {i}: `settleMs` applies to a live call only"),
            _ => anyhow::bail!("step {i}: `settleMs` must be a number of milliseconds"),
        };
        let now = match &step["now"] {
            serde_json::Value::Null => None,
            serde_json::Value::Number(n) => n.as_f64(),
            _ => anyhow::bail!("step {i}: `now` must be a number (ms since the epoch)"),
        };
        if step["from"].is_string() || seed.is_some() {
            let current = kernel.snapshot_trait(trait_name, None);
            let entity = match seed {
                None => current.entity,
                Some(fields) => {
                    let mut row = match current.entity {
                        Value::Object(row) => row,
                        _ => Default::default(),
                    };
                    for (key, value) in fields {
                        row.insert(key.clone(), to_core_value(value)?);
                    }
                    Value::Object(row)
                }
            };
            kernel.restore_trait(
                trait_name,
                None,
                orbital_core::TraitSnapshot {
                    state: step["from"].as_str().map(str::to_string).unwrap_or_default(),
                    entity,
                },
            );
        }
        *pinned_now
            .lock()
            .map_err(|_| anyhow::anyhow!("step {i}: the clock pin is poisoned"))? = now;
        let payload = match &step["payload"] {
            serde_json::Value::Null => orbital_core::Value::Null,
            p => to_core_value(p)?,
        };
        let viewing = viewer.map(|user| kernel.enter_viewer(user));
        let outcomes = match &live_host {
            Some(host) => {
                let played = host.dispatch(trait_name, "singleton", event, payload).map_err(|e| e.to_string());
                settle_open_calls(host, settle_ms);
                played
            }
            None => kernel
                .start_dispatch(trait_name, None, event, payload, None, None)
                .and_then(|progress| {
                    orbital_core::runtime::resumable::drive_blocking(kernel, progress, &|request| step_answer(kernel, request))
                })
                .map_err(|e| e.to_string()),
        };
        drop(viewing);
        *pinned_now
            .lock()
            .map_err(|_| anyhow::anyhow!("step {i}: the clock pin is poisoned"))? = None;
        let outcomes = outcomes.map_err(|e| anyhow::anyhow!("step {i} dispatch failed: {e}"))?;
        let entity_after = serde_json::to_value(kernel.snapshot_trait(trait_name, None).entity)
            .unwrap_or(serde_json::Value::Null);
        // The moved traits' rows after the step, so a caller can read what a step wrote
        // (an id it generated, a list it changed) and feed it to the next step.
        let entities: serde_json::Map<String, serde_json::Value> = outcomes
            .iter()
            .filter(|(_, r)| r.executed)
            .map(|(t, _)| {
                let row = kernel.snapshot_trait(t, None).entity;
                (t.to_string(), serde_json::to_value(&row).unwrap_or(serde_json::Value::Null))
            })
            .collect();
        let outcomes: Vec<serde_json::Value> = outcomes
            .iter()
            .map(|(t, r)| {
                let mut report = outcome_report(t, r);
                report["orbital"] = kernel
                    .orbital_of_trait(t)
                    .map_or(serde_json::Value::Null, serde_json::Value::String);
                report["event"] = serde_json::Value::String(r.event.clone().unwrap_or_else(|| event.to_string()));
                let ensures = trait_defs
                    .get(t.as_str())
                    .map(|td| {
                        orbital_core::program_machine::played_ensures(
                            td,
                            r.event.as_deref().unwrap_or(event),
                            r,
                        )
                    })
                    .unwrap_or_default();
                report["ensures"] =
                    serde_json::to_value(ensures).unwrap_or(serde_json::Value::Array(Vec::new()));
                report
            })
            .collect();
        let states: serde_json::Map<String, serde_json::Value> = kernel
            .trait_snapshot(Some("singleton"))
            .into_iter()
            .map(|(name, (state, _))| (name, serde_json::Value::String(state)))
            .collect();
        reports.push(serde_json::json!({ "outcomes": outcomes, "states": states, "entities": entities, "entityAfter": entity_after }));
    }
    let dispatch_modes: serde_json::Map<String, serde_json::Value> = kernel
        .trait_snapshot(Some("singleton"))
        .into_iter()
        .map(|(name, (_, mode))| {
            (
                name,
                serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    Ok(serde_json::json!({ "success": true, "steps": reports, "dispatchModes": dispatch_modes }))
}

#[cfg(test)]
mod outcome_report_tests {
    use super::*;
    use orbital_core::runtime::{EffectRun, EffectRunStatus, ProcessEventResult};
    use orbital_core::state_machine::{ArmGuard, GuardResult};

    fn result(fired_arm: Option<usize>, runs: Vec<EffectRun>) -> ProcessEventResult {
        let guard: SExpression = serde_json::from_str(r#"["=", "@payload.key", "v"]"#).unwrap();
        ProcessEventResult {
            executed: fired_arm.is_some(),
            previous_state: "NORMAL".into(),
            new_state: if fired_arm.is_some() {
                "VISUAL".into()
            } else {
                "NORMAL".into()
            },
            effects_executed: 0,
            effect_results: Vec::new(),
            entity_data: orbital_core::runtime::shared_entity::SharedEntity::null(),
            linked_entity: "Doc".into(),
            event: fired_arm.map(|_| "KEY".to_string()),
            payload: Value::Null,
            entry: None,
            fired_arm,
            transition_index: None,
            entity_id: "singleton".into(),
            state_entity_id: "singleton".into(),
            guards: vec![ArmGuard {
                arm: 1,
                guard: GuardResult::passed(guard, Value::Bool(true)),
            }],
            effect_runs: runs,
            consumed_emits: Vec::new(),
            failure: None,
            callsite_payload: None,
            cascade_dropped: Vec::new(),
            cascade_cut: None,
        }
    }

    #[test]
    fn a_fired_outcome_reports_its_arm_guards_and_authored_effects() {
        let set: SExpression =
            serde_json::from_str(r#"["set", "@entity.mode", "visual"]"#).unwrap();
        let report = outcome_report(
            "Modes",
            &result(
                Some(1),
                vec![EffectRun {
                    effect: set,
                    status: EffectRunStatus::Executed,
                    error: None,
                }],
            ),
        );
        assert_eq!(report["firedArm"], serde_json::json!(1));
        assert_eq!(
            report["guards"],
            serde_json::json!([{ "arm": 1, "guard": ["=", "@payload.key", "v"], "passed": true }])
        );
        assert_eq!(
            report["effects"],
            serde_json::json!([{ "type": "set", "args": ["@entity.mode", "visual"], "status": "executed", "routed": false }])
        );
        assert_eq!(report["previousState"], serde_json::json!("NORMAL"));
    }

    #[test]
    fn control_a_blocked_outcome_has_a_null_arm_and_no_effects() {
        let report = outcome_report("Modes", &result(None, Vec::new()));
        assert_eq!(report["firedArm"], serde_json::Value::Null);
        assert_eq!(report["effects"], serde_json::json!([]));
    }

    #[test]
    fn edge_a_failed_effect_carries_its_error() {
        let persist: SExpression =
            serde_json::from_str(r#"["persist", "create", "Doc", "@entity"]"#).unwrap();
        let report = outcome_report(
            "Modes",
            &result(
                Some(1),
                vec![EffectRun {
                    effect: persist,
                    status: EffectRunStatus::Failed,
                    error: Some("denied".into()),
                }],
            ),
        );
        assert_eq!(report["effects"][0]["status"], serde_json::json!("failed"));
        assert_eq!(report["effects"][0]["error"], serde_json::json!("denied"));
    }
}

#[cfg(test)]
mod cascade_from_tests {
    use super::*;

    const MODES: &str = r#"{"name":"EditorApp","orbitals":[{"name":"Editor","entity":{"name":"Doc","persistence":"runtime","fields":[{"name":"id","type":"string"}]},"pages":[],"traits":[{"name":"Modes","linkedEntity":"Doc","stateMachine":{"states":[{"name":"NORMAL","isInitial":true},{"name":"INSERT"},{"name":"VISUAL"}],"transitions":[{"from":"NORMAL","to":"INSERT","event":"KEY","guard":["=","@payload.key","i"]},{"from":"NORMAL","to":"VISUAL","event":"KEY","guard":["=","@payload.key","v"]}]}}]}]}"#;

    /// A program file of its own per call: tests run in parallel threads of one process.
    fn program(content: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("orb-cascade-from-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("modes.orb");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn run(steps: serde_json::Value) -> serde_json::Value {
        run_cascade(&program(MODES), &steps).unwrap()
    }

    const COUNTER: &str = r#"{"name":"CounterApp","orbitals":[{"name":"Counting","entity":{"name":"Counter","persistence":"runtime","fields":[{"name":"id","type":"string"},{"name":"count","type":"number","default":0}]},"pages":[],"traits":[{"name":"Count","linkedEntity":"Counter","stateMachine":{"states":[{"name":"ready","isInitial":true}],"transitions":[{"from":"ready","to":"ready","event":"INC","effects":[["set","@entity.count",["+","@entity.count",1]]]}]}}]}]}"#;

    /// `play` shows what a step wrote, so a value it generated can drive the next step.
    #[test]
    fn a_step_reports_the_row_each_moved_trait_holds_after_it() {
        let report = run_cascade(
            &program(COUNTER),
            &serde_json::json!([{ "trait": "Count", "event": "INC" }, { "trait": "Count", "event": "INC" }]),
        )
        .unwrap();
        assert_eq!(report["steps"][0]["entities"]["Count"]["count"], serde_json::json!(1.0), "{report}");
        assert_eq!(report["steps"][1]["entities"]["Count"]["count"], serde_json::json!(2.0), "{report}");
    }

    #[test]
    fn control_a_step_that_moves_nothing_reports_no_rows() {
        let report = run(serde_json::json!([{ "trait": "Modes", "event": "KEY", "payload": { "key": "x" } }]));
        assert_eq!(report["steps"][0]["entities"], serde_json::json!({}), "{report}");
    }

    /// G-ORB-477: one `Answer` trait whose `GO` emits `RESPOND` with the given payload.
    fn answer(payload: serde_json::Value) -> serde_json::Value {
        let program_json = serde_json::json!({"name":"AnswerApp","orbitals":[{"name":"Answering","entity":{"name":"Req","persistence":"runtime","fields":[{"name":"id","type":"string"}]},"pages":[],"traits":[{"name":"Answer","linkedEntity":"Req","stateMachine":{"states":[{"name":"ready","isInitial":true}],"transitions":[{"from":"ready","to":"ready","event":"GO","effects":[["emit","RESPOND",payload]]}]}}]}]});
        run_cascade(&program(&program_json.to_string()), &serde_json::json!([{ "trait": "Answer", "event": "GO" }])).unwrap()
    }

    #[test]
    fn an_emit_whose_payload_fails_to_evaluate_fails_the_step_and_emits_nothing() {
        let report = answer(serde_json::json!({ "headers": ["array/append", ["object/get", null, "h"], { "name": "x" }] }));
        let outcome = &report["steps"][0]["outcomes"][0];
        assert_eq!(outcome["emits"], serde_json::json!([]), "{report}");
        assert!(outcome["failure"].as_str().is_some_and(|f| !f.is_empty()), "{report}");
        assert_eq!(outcome["effects"][0]["status"], serde_json::json!("failed"), "{report}");
    }

    #[test]
    fn control_an_emit_whose_payload_evaluates_is_reported_resolved() {
        let report = answer(serde_json::json!({ "status": ["if", true, 403, 200] }));
        let outcome = &report["steps"][0]["outcomes"][0];
        assert_eq!(outcome["emits"][0]["payload"], serde_json::json!({ "status": 403.0 }), "{report}");
        assert_eq!(outcome["failure"], serde_json::Value::Null, "{report}");
    }

    fn dispatching(options: serde_json::Value) -> serde_json::Value {
        let program_json = serde_json::json!({"name":"AskApp","orbitals":[{"name":"Asking","entity":{"name":"Req","persistence":"runtime","fields":[{"name":"id","type":"string"}]},"pages":[],"traits":[{"name":"Ask","linkedEntity":"Req","stateMachine":{"states":[{"name":"ready","isInitial":true}],"transitions":[{"from":"ready","to":"ready","event":"GO","effects":[["program/dispatch","INNER",{},{"viewer":null},options]]}]}}]}]});
        run_cascade(&program(&program_json.to_string()), &serde_json::json!([{ "trait": "Ask", "event": "GO" }])).unwrap()
    }

    /// A replayed step leaves a keyed program op pending as it leaves keyed os and http asks: its completion is the
    /// next step the caller feeds, so no route fires in this one.
    #[test]
    fn a_keyed_program_ask_is_left_pending_in_a_replayed_step() {
        let report = dispatching(serde_json::json!({ "key": "k1", "emit": { "success": "GOT", "failure": "LOST" } }));
        let outcome = &report["steps"][0]["outcomes"][0];
        assert_eq!(outcome["emits"], serde_json::json!([]), "{report}");
        let control = dispatching(serde_json::json!({ "emit": { "success": "GOT", "failure": "LOST" } }));
        let fired: Vec<&str> = control["steps"][0]["outcomes"][0]["emits"].as_array().unwrap().iter().filter_map(|e| e["event"].as_str()).collect();
        assert_eq!(fired, vec!["LOST"], "control: an unkeyed program op is still answered in place: {control}");
    }

    #[test]
    fn a_step_naming_a_trait_the_program_lacks_is_refused() {
        let error = run_cascade(
            &program(MODES),
            &serde_json::json!([{ "trait": "Nope", "event": "KEY" }]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("\"Nope\""), "{error}");
    }

    #[test]
    fn edge_an_empty_program_file_is_refused() {
        assert!(run_cascade(
            &program(""),
            &serde_json::json!([{ "trait": "Modes", "event": "KEY" }])
        )
        .is_err());
    }

    #[test]
    fn a_step_with_from_plays_from_that_state() {
        let report = run(serde_json::json!([
            { "trait": "Modes", "event": "KEY", "payload": { "key": "i" } },
            { "trait": "Modes", "event": "KEY", "payload": { "key": "v" }, "from": "NORMAL" }
        ]));
        let second = &report["steps"][1]["outcomes"][0];
        assert_eq!(second["previousState"], serde_json::json!("NORMAL"));
        assert_eq!(second["newState"], serde_json::json!("VISUAL"));
        assert_eq!(second["firedArm"], serde_json::json!(1));
    }

    #[test]
    fn control_without_from_the_step_plays_where_the_last_one_left_the_trait() {
        let report = run(serde_json::json!([
            { "trait": "Modes", "event": "KEY", "payload": { "key": "i" } },
            { "trait": "Modes", "event": "KEY", "payload": { "key": "v" } }
        ]));
        let second = &report["steps"][1]["outcomes"][0];
        assert_eq!(second["previousState"], serde_json::json!("INSERT"));
        assert_eq!(second["executed"], serde_json::json!(false));
    }
}
