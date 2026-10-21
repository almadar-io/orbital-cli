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
use std::time::Instant;

use orbital_core::{
    BindingContext, EventPayload, SExpression, StateMachineCore, TraitDefinition, Value,
};
use orbital_verify::planner::{initial_state, inline_traits};
use serde::Serialize;

use crate::utils::parse_schema_auto;

/// One fired transition — mirrors `harness/types.ts` `StateTransition`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecTransition {
    from: String,
    to: String,
    event: String,
    /// Always 0 — there is no wall clock here to compare against the
    /// TypeScript side's `Date.now()`, and no parity comparison keys on it.
    timestamp: i64,
}

/// One executed effect S-expression — mirrors `NormalizedEffect`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecEffect {
    operator: String,
    args: Vec<serde_json::Value>,
    original: serde_json::Value,
}

fn to_exec_effect(expr: &SExpression) -> ExecEffect {
    match expr.operator() {
        Some(op) => ExecEffect {
            operator: op.to_string(),
            args: expr
                .arguments()
                .unwrap_or(&[])
                .iter()
                .map(SExpression::to_canonical_json)
                .collect(),
            original: expr.to_canonical_json(),
        },
        // Mirrors `normalizeEffect`'s fallback for a non-call-shaped effect.
        None => ExecEffect {
            operator: "unknown".to_string(),
            args: vec![],
            original: expr.to_canonical_json(),
        },
    }
}

/// Evaluation context echoed back on a guard evaluation — mirrors `EvalContext`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecGuardContext {
    entity: serde_json::Value,
    payload: serde_json::Value,
    state: String,
}

/// One guard evaluation — mirrors `harness/types.ts` `GuardResult`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecGuardEvaluation {
    guard: serde_json::Value,
    context: ExecGuardContext,
    result: bool,
    path: String,
}

/// Execution DTO — field-for-field mirror of
/// `packages/almadar-parity/src/harness/types.ts:60` `ExecutionResult`. Lives
/// here (never on `orbital-core`) because parity JSON is a CLI-only concern:
/// `StateMachineCore::process_event`'s own `TransitionResult`/`GuardResult`
/// deliberately do not derive `Serialize`.
///
/// Two asymmetries are DELIBERATELY preserved to keep parity comparisons
/// apples-to-apples with `ts-harness.ts` `executeWithRuntime` (same file,
/// ~L279-335): (1) a guard evaluation is recorded only when SOME transition
/// for the fired event actually matched the trait's current `from` state —
/// an event nobody listens for from that state records no guard, even when
/// a different `from` state declares a guarded transition on the same event
/// name; (2) the guard EXPRESSION shown in that evaluation is looked up by
/// event name alone, ignoring `from` (`ts-harness.ts` line ~319 finds the
/// transition definition via `.find(t => t.event === normalizedEvent)`) — on
/// a trait where two different `from` states share one event name and only
/// one of them is guarded, the displayed `guard` can differ from the guard
/// that actually fired even though `result` is always the real evaluated
/// boolean from the transition that matched `from`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionResultDto {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    initial_state: String,
    final_state: String,
    transitions: Vec<ExecTransition>,
    effects: Vec<ExecEffect>,
    guard_evaluations: Vec<ExecGuardEvaluation>,
    implementation: &'static str,
    execution_time_ms: u128,
    schema_path: String,
}

impl ExecutionResultDto {
    fn failure(schema_path: &str, execution_time_ms: u128, error: String) -> Self {
        Self {
            success: false,
            error: Some(error),
            initial_state: String::new(),
            final_state: String::new(),
            transitions: vec![],
            effects: vec![],
            guard_evaluations: vec![],
            implementation: "rust",
            execution_time_ms,
            schema_path: schema_path.to_string(),
        }
    }
}

/// `orb execute <FILE> --event E [--payload JSON] [--entity JSON] [--trait NAME] [--json]`
pub(crate) fn execute_schema_cli(
    path: &Path,
    event: &str,
    payload: Option<&str>,
    entity: Option<&str>,
    trait_filter: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let start = Instant::now();
    let schema_path = path.display().to_string();

    let dto = match run_execute(path, event, payload, entity, trait_filter) {
        Ok(mut dto) => {
            dto.execution_time_ms = start.elapsed().as_millis();
            dto
        }
        // Parse/resolve/argument failures are a DATA outcome, not a process
        // failure — the caller (`rust-harness.ts`, this crate's own tests)
        // always gets a well-formed DTO on stdout to parse, mirroring how
        // `ts-harness.ts execute()` catches and returns `success:false`
        // rather than throwing.
        Err(e) => {
            ExecutionResultDto::failure(&schema_path, start.elapsed().as_millis(), e.to_string())
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&dto)?);
    } else {
        print_human(&dto);
    }

    Ok(())
}

fn print_human(dto: &ExecutionResultDto) {
    println!("Execute: {}", dto.schema_path);
    if !dto.success {
        println!(
            "  FAILED: {}",
            dto.error.as_deref().unwrap_or("unknown error")
        );
        return;
    }
    println!("  {} -> {}", dto.initial_state, dto.final_state);
    println!("  transitions: {}", dto.transitions.len());
    for t in &dto.transitions {
        println!("    {} --{}--> {}", t.from, t.event, t.to);
    }
    println!("  effects: {}", dto.effects.len());
    for e in &dto.effects {
        println!("    ({} {})", e.operator, e.args.len());
    }
    println!("  guard evaluations: {}", dto.guard_evaluations.len());
    for g in &dto.guard_evaluations {
        println!("    {} -> {}", g.path, g.result);
    }
}

/// The value of an optional `--payload`/`--entity` CLI argument, parsed as
/// JSON. Absent (matching `ts-harness.ts`'s `payload ?? {}` /
/// `options?.entityState ?? {}`) defaults to an empty object.
fn parse_json_arg(raw: Option<&str>) -> anyhow::Result<serde_json::Value> {
    match raw {
        Some(s) => {
            serde_json::from_str(s).map_err(|e| anyhow::anyhow!("Invalid JSON: {}: {}", s, e))
        }
        None => Ok(serde_json::json!({})),
    }
}

fn to_core_value(json: &serde_json::Value) -> anyhow::Result<Value> {
    serde_json::from_value(json.clone())
        .map_err(|e| anyhow::anyhow!("Failed to convert JSON to evaluator value: {}", e))
}

/// The guard expression the TypeScript harness would display for this event,
/// looked up by event name alone (ignoring `from`) — see the asymmetry note
/// on [`ExecutionResultDto`].
fn guard_for_event(td: &TraitDefinition, event: &str) -> serde_json::Value {
    td.state_machine
        .transitions
        .iter()
        .find(|t| t.event == event)
        .and_then(|t| t.guard.as_ref())
        .map(SExpression::to_canonical_json)
        .unwrap_or(serde_json::Value::Array(vec![]))
}

/// `orb execute <FILE> --event E --trait T --cascade`: the event through the real kernel dispatch
/// (listens fan-out, executed effects, the dispatch roots). The Rust leg of the cascade parity runner
/// (`tools/parity-cascade`); prints one JSON object whether it succeeds or not.
pub(crate) fn execute_cascade_cli(
    path: &Path,
    event: &str,
    payload: Option<&str>,
    trait_name: &str,
) -> anyhow::Result<()> {
    let report = parse_json_arg(payload)
        .map(|payload| serde_json::json!([{ "trait": trait_name, "event": event, "payload": payload }]))
        .and_then(|steps| run_cascade(path, &steps))
        .map(|mut report| {
            // One step: its outcomes at the top level, as before `--steps`.
            if let Some(first) = report["steps"][0]["outcomes"].as_array().cloned() {
                report["outcomes"] = serde_json::Value::Array(first);
            }
            report
        });
    print_cascade_report(report)
}

/// `orb execute <FILE> --steps '[{trait, event, payload}, …]'`: each step through the same kernel
/// in order, so what one step leaves behind (a held delivery, a state) is what the next one meets.
pub(crate) fn execute_cascade_steps_cli(path: &Path, steps: &str) -> anyhow::Result<()> {
    print_cascade_report(parse_json_arg(Some(steps)).and_then(|steps| run_cascade(path, &steps)))
}

fn print_cascade_report(report: anyhow::Result<serde_json::Value>) -> anyhow::Result<()> {
    let report =
        report.unwrap_or_else(|e| serde_json::json!({ "success": false, "error": e.to_string() }));
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
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
    })
}

/// The program at `path`, parsed and resolved the way a cascade runs it.
pub(crate) fn resolve_program(path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    use orbital_compiler::{phases::lazy::resolve_program_with_path, CompilerOptions};
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let schema = parse_schema_auto(&content, &path.display().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let (resolved, _lazy) =
        resolve_program_with_path(schema, &CompilerOptions::from_env(), Some(path))
            .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    Ok(resolved)
}

pub(crate) fn run_cascade(path: &Path, steps: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    use orbital_core::runtime::{EntitySource, InMemoryEntityCache, RuntimeKernel};

    let steps = steps
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("--steps must be a JSON array"))?;
    let resolved = resolve_program(path)?;

    let kernel = RuntimeKernel::new(
        orbital_core::RuntimeEnvironment::Server,
        std::sync::Arc::new(InMemoryEntityCache::new()) as std::sync::Arc<dyn EntitySource>,
        Default::default(),
    );
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
    kernel
        .register_schema(resolved)
        .map_err(|e| anyhow::anyhow!("Failed to register {}: {e}", path.display()))?;
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
        if let Some(from) = step["from"].as_str() {
            let entity = kernel.snapshot_trait(trait_name, None).entity;
            kernel.restore_trait(
                trait_name,
                None,
                orbital_core::TraitSnapshot {
                    state: from.to_string(),
                    entity,
                },
            );
        }
        let payload = match &step["payload"] {
            serde_json::Value::Null => orbital_core::Value::Null,
            p => to_core_value(p)?,
        };
        let outcomes = kernel
            .dispatch(trait_name, None, event, payload)
            .map_err(|e| anyhow::anyhow!("step {i} dispatch failed: {e}"))?;
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
        reports.push(serde_json::json!({ "outcomes": outcomes, "states": states, "entities": entities }));
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

fn run_execute(
    path: &Path,
    event: &str,
    payload_arg: Option<&str>,
    entity_arg: Option<&str>,
    trait_filter: Option<&str>,
) -> anyhow::Result<ExecutionResultDto> {
    use orbital_compiler::{phases::lazy::resolve_program_with_path, CompilerOptions};

    let schema_path = path.display().to_string();
    let options = CompilerOptions::from_env();

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let schema = parse_schema_auto(&content, &schema_path)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let (resolved, _lazy) = resolve_program_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;

    let payload_json = parse_json_arg(payload_arg)?;
    let entity_json = parse_json_arg(entity_arg)?;
    let payload_value = to_core_value(&payload_json)?;
    let entity_value = to_core_value(&entity_json)?;

    let mut traits = inline_traits(&resolved);
    if let Some(name) = trait_filter {
        traits.retain(|t| t.name == name);
    }

    if traits.is_empty() {
        return Ok(ExecutionResultDto {
            success: false,
            error: Some("No inline traits with state machines found in schema".to_string()),
            initial_state: String::new(),
            final_state: String::new(),
            transitions: vec![],
            effects: vec![],
            guard_evaluations: vec![],
            implementation: "rust",
            execution_time_ms: 0,
            schema_path: schema_path.clone(),
        });
    }

    let core = StateMachineCore::new();
    let mut first_initial_state = String::new();
    let mut last_final_state = String::new();
    let mut all_transitions = Vec::new();
    let mut all_effects = Vec::new();
    let mut all_guards = Vec::new();

    for td in &traits {
        let current_state = initial_state(td).unwrap_or_else(|| "idle".to_string());
        if first_initial_state.is_empty() {
            first_initial_state = current_state.clone();
        }

        let declared = td
            .state_machine
            .events
            .iter()
            .flatten()
            .find(|declared| declared.key == event)
            .and_then(|declared| declared.payload_schema.as_deref());
        if let Some(schema) = declared {
            let failures = orbital_core::runtime::payload_validation::validate_event_payload(event, &payload_value, schema);
            if !failures.is_empty() {
                anyhow::bail!("{}", orbital_core::runtime::payload_validation::format_payload_failures(&failures));
            }
        }

        let event_payload = EventPayload::new(event, payload_value.clone());
        let mut ctx = BindingContext::new();
        ctx.set_entity(entity_value.clone());

        let result = core.process_event(&td.state_machine, &current_state, &event_payload, &ctx)?;

        let post_state = if result.executed {
            let to = result
                .new_state
                .clone()
                .ok_or_else(|| anyhow::anyhow!("executed transition carries no new_state"))?;
            all_transitions.push(ExecTransition {
                from: current_state.clone(),
                to: to.clone(),
                event: event.to_string(),
                timestamp: 0,
            });
            for effect in result.effects() {
                all_effects.push(to_exec_effect(effect));
            }
            to
        } else {
            current_state.clone()
        };

        if let Some(guard_result) = &result.guard_result {
            all_guards.push(ExecGuardEvaluation {
                guard: guard_for_event(td, event),
                context: ExecGuardContext {
                    entity: entity_json.clone(),
                    payload: payload_json.clone(),
                    state: post_state.clone(),
                },
                result: guard_result.passed,
                path: format!("traits[{}].transitions[{}]", td.name, event),
            });
        }

        last_final_state = post_state;
    }

    Ok(ExecutionResultDto {
        success: true,
        error: None,
        initial_state: first_initial_state,
        final_state: last_final_state,
        transitions: all_transitions,
        effects: all_effects,
        guard_evaluations: all_guards,
        implementation: "rust",
        execution_time_ms: 0,
        schema_path,
    })
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
            guards: vec![ArmGuard {
                arm: 1,
                guard: GuardResult::passed(guard, Value::Bool(true)),
            }],
            effect_runs: runs,
            consumed_emits: Vec::new(),
            failure: None,
            callsite_payload: None,
            cascade_dropped: Vec::new(),
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
