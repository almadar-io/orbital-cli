//! The native host of the language trio's server effects (`program/read|print|validate`, `behavior/catalog|describe|
//! source|resolve`): the kernel's `SubstrateHandler` answered by `orb`'s own functions, in process. Its JS twin is
//! `runProgramEffect` in `@almadar/runtime` (`host/program`), which runs these same functions through the `orb`
//! commands (`emit orb`, `emit lolo`, `stamp` + `validate --json --deny-warnings`, `behaviors catalog|describe|source
//! --json`, `resolve`); each answer here is the value those commands print, staged the same way.

use orbital_core::effects::executor::{SubstrateFailure, SubstrateHandler};
use orbital_core::Value;
use std::path::PathBuf;

/// The program host of the project `orb` runs in: behavior specifiers and relative imports resolve from the process's
/// working directory, as the `orb` commands resolve them.
pub(crate) struct ProgramHost;

fn failure(message: impl Into<String>) -> SubstrateFailure {
    SubstrateFailure {
        message: message.into(),
        errors: Vec::new(),
    }
}

fn to_json(value: &Value) -> serde_json::Value {
    orbital_core::json_value::value_to_json(value)
}

fn from_json(value: serde_json::Value) -> Result<Value, SubstrateFailure> {
    serde_json::from_value(value).map_err(|e| failure(e.to_string()))
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(prefix: &str) -> Result<Self, SubstrateFailure> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| failure(e.to_string()))?;
        Ok(Self(dir))
    }

    fn write(&self, name: &str, content: &str) -> Result<PathBuf, SubstrateFailure> {
        let path = self.0.join(name);
        std::fs::write(&path, content).map_err(|e| failure(e.to_string()))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn program_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A program argument: an object with a `name` and `orbitals`, every orbital value in it standing for its import
/// orbital (as `@almadar/core` `parseProgram` reads a program held as data).
fn program_arg(op: &str, value: Option<&Value>) -> Result<serde_json::Value, SubstrateFailure> {
    let json = value.map(to_json).unwrap_or(serde_json::Value::Null);
    match &json {
        serde_json::Value::Object(map)
            if map.get("name").is_some_and(serde_json::Value::is_string)
                && map.contains_key("orbitals") =>
        {
            orbital_core::schema::behavior_value::program_with_orbital_imports(&json)
                .map_err(|e| failure(format!("{op}: {e}")))
        }
        _ => Err(failure(format!("{op}: not a program"))),
    }
}

/// A program stamped (ids minted against `prior`) and validated: the findings, and the stamped program text when
/// stamping succeeded.
struct Checked {
    valid: bool,
    errors: Vec<serde_json::Value>,
    warnings: Vec<serde_json::Value>,
    stamped: Option<String>,
}

fn stamp_and_validate(
    op: &str,
    program: &serde_json::Value,
    prior: Option<&serde_json::Value>,
    final_program: bool,
) -> Result<Checked, SubstrateFailure> {
    let name = program
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let issue =
        |code: &str, message: String| serde_json::json!({ "code": code, "message": message });
    let refused = |error: serde_json::Value| Checked {
        valid: false,
        errors: vec![error],
        warnings: Vec::new(),
        stamped: None,
    };
    if !program_name_ok(&name) {
        return Ok(refused(issue(
            "PROGRAM_NAME_INVALID",
            format!("program name \"{name}\" is not a file name"),
        )));
    }
    let scratch = Scratch::new("almadar-validate")?;
    let input = scratch.write(&format!("{name}.orb"), &program.to_string())?;
    let prior_path = prior
        .map(|p| scratch.write("prior.orb", &p.to_string()))
        .transpose()?;
    let stamped_text =
        match crate::commands::stamp::stamped_json(&input, prior_path.as_deref(), None) {
            Ok(json) => json,
            Err(e) => return Ok(refused(issue("PROGRAM_STAMP_FAILED", e.to_string()))),
        };
    let stamped = scratch.write(&format!("{name}.stamped.orb"), &stamped_text)?;
    let report =
        crate::commands::validate::validation_report(&stamped, None, None, true, final_program)
            .map_err(|e| failure(format!("{op}: {e}")))?;
    let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
    let (errors, warnings) = (issues(&report, "errors"), issues(&report, "warnings"));
    let valid = report
        .get("valid")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && errors.is_empty()
        && warnings.is_empty();
    Ok(Checked {
        valid,
        errors,
        warnings,
        stamped: Some(stamped_text),
    })
}

/// The issues of a validation report, as the `{ code, message, path?, line?, column? }` findings both hosts answer.
fn issues(report: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    report
        .get(key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

impl ProgramHost {
    fn read(&self, text: &str) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let path = scratch.write("program.lolo", text)?;
        match crate::emit::orb_json(text, &path.display().to_string()) {
            Ok(json) => from_json(serde_json::from_str(&json).map_err(|e| failure(e.to_string()))?),
            Err(lowering) => {
                let report =
                    crate::commands::validate::validation_report(&path, None, None, true, false)
                        .map_err(|e| failure(format!("program/read: {e}")))?;
                let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
                let found: Vec<serde_json::Value> = issues(&report, "errors")
                    .into_iter()
                    .chain(issues(&report, "warnings"))
                    .collect();
                if found.is_empty() {
                    return Err(failure(format!("program/read: {lowering}")));
                }
                let messages: Vec<&str> = found
                    .iter()
                    .filter_map(|i| i.get("message").and_then(serde_json::Value::as_str))
                    .collect();
                Err(SubstrateFailure {
                    message: format!(
                        "program/read: {} validation issue(s): {}",
                        found.len(),
                        messages.join("; ")
                    ),
                    errors: found.into_iter().map(from_json).collect::<Result<_, _>>()?,
                })
            }
        }
    }

    fn print(&self, program: serde_json::Value) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let text = program.to_string();
        let path = scratch.write("program.orb", &text)?;
        crate::emit::lolo_text(&text, &path.display().to_string())
            .map(Value::String)
            .map_err(|e| failure(format!("program/print: {e}")))
    }

    fn validate(
        &self,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let mut final_program = false;
        let mut prior: Option<serde_json::Value> = None;
        match options.map(to_json) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("final", serde_json::Value::Bool(b)) => final_program = b,
                        ("final", _) => {
                            return Err(failure("program/validate: final must be a boolean"))
                        }
                        ("prior", p) => {
                            prior = Some(program_arg(
                                "program/validate prior",
                                from_json(p).ok().as_ref(),
                            )?)
                        }
                        (other, _) => {
                            return Err(failure(format!(
                                "program/validate: unsupported option \"{other}\""
                            )))
                        }
                    }
                }
            }
            Some(_) => return Err(failure("program/validate: options must be an object")),
        }
        let checked =
            stamp_and_validate("program/validate", &program, prior.as_ref(), final_program)?;
        from_json(
            serde_json::json!({ "valid": checked.valid, "errors": checked.errors, "warnings": checked.warnings }),
        )
    }

    fn describe_target(
        op: &str,
        value: Option<&Value>,
    ) -> Result<crate::commands::behaviors_catalog::DescribeTarget, SubstrateFailure> {
        use crate::commands::behaviors_catalog::DescribeTarget;
        let json = value.map(to_json).unwrap_or(serde_json::Value::Null);
        let behavior = json
            .get("behavior")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match (
            behavior,
            json.get("trait"),
            json.get("orbital").and_then(serde_json::Value::as_str),
        ) {
            (Some(_), Some(_), _) => Ok(DescribeTarget::Value(json)),
            (Some(behavior), None, Some(orbital)) => Ok(DescribeTarget::Spec(format!(
                "{behavior}.orbitals.{orbital}"
            ))),
            (Some(behavior), None, None) => Ok(DescribeTarget::Spec(behavior)),
            (None, _, _) => Err(failure(format!(
                "{op}: expected {{ behavior }}, an orbital value or a trait value"
            ))),
        }
    }

    fn behavior_of(op: &str, value: Option<&Value>) -> Result<String, SubstrateFailure> {
        value
            .map(to_json)
            .and_then(|v| {
                v.get("behavior")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| failure(format!("{op}: expected a value naming its behavior")))
    }

    fn catalog(&self, scope: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let json = scope.map(to_json).unwrap_or(serde_json::Value::Null);
        let strings = |key: &str| -> Option<Vec<String>> {
            json.get(key)
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
        };
        let Some(paths) = strings("paths") else {
            return Err(failure(
                "behavior/catalog: paths: expected a list of registry folders",
            ));
        };
        let exposure = strings("exposure").unwrap_or_default();
        let entries = crate::commands::behaviors_catalog::catalog(&paths, &exposure)
            .map_err(|e| failure(format!("behavior/catalog: {e}")))?;
        from_json(serde_json::to_value(entries).map_err(|e| failure(e.to_string()))?)
    }

    fn resolve_behavior(&self, behavior: &str) -> Result<Value, SubstrateFailure> {
        let path = crate::commands::behaviors_catalog::behavior_source_path(behavior)
            .map_err(|e| failure(format!("behavior/resolve: {e}")))?;
        let resolved = crate::commands::dev::resolve_to_temp(&path)
            .map_err(|e| failure(format!("behavior/resolve: {e}")))?;
        let text = std::fs::read_to_string(&resolved).map_err(|e| failure(e.to_string()))?;
        from_json(serde_json::from_str(&text).map_err(|e| failure(e.to_string()))?)
    }
}

/// `importsOf`: each `uses` of the program's orbitals as `(behavior, traits composed from it)`, the behavior being
/// the path's trailing name (the loader's rule).
fn imported_behaviors(program: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for orbital in program
        .get("orbitals")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        for u in orbital
            .get("uses")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(from) = u.get("from").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let behavior = from.rsplit('/').next().unwrap_or(from).to_string();
            if !out.contains(&behavior) {
                out.push(behavior);
            }
        }
    }
    out
}

/// `(tier, prefix, pattern)` of every installed behavior, by name: the first package that ships it.
fn installed_behavior_index(
) -> Result<std::collections::HashMap<String, (String, String, Option<String>)>, SubstrateFailure> {
    let cwd = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
    let packages = super::npm_project::discover_from(&cwd)
        .map_err(|e| failure(format!("behavior/dependencies: {e}")))?;
    let mut out = std::collections::HashMap::new();
    for pkg in packages {
        let tier = match pkg.name.as_str() {
            "@almadar/std" => "std",
            "@almadar-io/behaviors" => "io",
            _ => "unknown",
        };
        let entries = crate::commands::behaviors_catalog::catalog_with(
            &[pkg.prefix.clone()],
            &[],
            crate::commands::behaviors_catalog::Vectors::Omit,
            None,
        )
        .map_err(|e| failure(format!("behavior/dependencies: {e}")))?;
        for entry in serde_json::to_value(entries)
            .map_err(|e| failure(e.to_string()))?
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(behavior) = entry.get("behavior").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let name = behavior
                .strip_prefix(&format!("{}/", pkg.prefix))
                .unwrap_or(behavior)
                .to_string();
            let pattern = entry
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            out.entry(name)
                .or_insert((tier.to_string(), pkg.prefix.clone(), pattern));
        }
    }
    Ok(out)
}

impl ProgramHost {
    /// `behavior/dependencies`: each orbital's direct imports, then every behavior they reach, once, with its layer
    /// (`primitive` for a UI pattern factory, else its package's tier) and its own imports.
    fn dependencies(&self, program: serde_json::Value) -> Result<Value, SubstrateFailure> {
        let index = installed_behavior_index()?;
        let mut orbitals = serde_json::Map::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for orbital in program
            .get("orbitals")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = orbital
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let own = imported_behaviors(&serde_json::json!({ "orbitals": [orbital] }));
            queue.extend(own.iter().cloned());
            orbitals.insert(name, serde_json::json!(own));
        }
        let mut behaviors = serde_json::Map::new();
        while let Some(name) = queue.pop_front() {
            if behaviors.contains_key(&name) {
                continue;
            }
            let entry = index.get(&name);
            let imports = match entry {
                Some((_, prefix, _)) => imported_behaviors(
                    &crate::commands::behaviors_catalog::source(&format!("{prefix}/{name}"))
                        .map_err(|e| failure(format!("behavior/source: {e}")))?,
                ),
                None => Vec::new(),
            };
            let layer = match entry {
                Some((_, _, Some(_))) => "primitive".to_string(),
                Some((tier, _, None)) => tier.clone(),
                None => "unknown".to_string(),
            };
            queue.extend(imports.iter().cloned());
            behaviors.insert(
                name,
                serde_json::json!({ "layer": layer, "imports": imports }),
            );
        }
        from_json(serde_json::json!({ "orbitals": orbitals, "behaviors": behaviors }))
    }

    /// `program/eval`: the program stamped and validated at the 0/0 bar, then written into the project's
    /// `orbitals/` folder; the answer is the written behavior and its traits as values.
    fn eval(
        &self,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let project = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        self.eval_in(&project, program, options)
    }

    fn eval_in(
        &self,
        project: &std::path::Path,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let (mut prior, mut final_program) = (None, false);
        match options.map(to_json) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("final", serde_json::Value::Bool(b)) => final_program = b,
                        ("final", _) => return Err(failure("program/eval: final must be a boolean")),
                        ("prior", p) => prior = Some(program_arg("program/eval prior", from_json(p).ok().as_ref())?),
                        ("workspace", serde_json::Value::String(id)) if !id.is_empty() => {
                            return Err(SubstrateFailure {
                                message: "program/eval: 1 validation issue(s): no workspace host is installed on this host".to_string(),
                                errors: vec![from_json(serde_json::json!({ "code": "PROGRAM_NO_WORKSPACE_HOST", "message": "no workspace host is installed on this host" }))?],
                            })
                        }
                        ("workspace", _) => return Err(failure("program/eval: workspace must be a non-empty id")),
                        (other, _) => return Err(failure(format!("program/eval: unsupported option \"{other}\""))),
                    }
                }
            }
            Some(_) => return Err(failure("program/eval: options must be an object")),
        }
        let checked = stamp_and_validate("program/eval", &program, prior.as_ref(), final_program)?;
        let issues: Vec<serde_json::Value> = checked
            .errors
            .iter()
            .chain(checked.warnings.iter())
            .cloned()
            .collect();
        let stamped = match checked.stamped {
            Some(text) if checked.valid && issues.is_empty() => text,
            _ => {
                let messages: Vec<String> = issues
                    .iter()
                    .filter_map(|i| {
                        i.get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect();
                return Err(SubstrateFailure {
                    message: format!(
                        "program/eval: {} validation issue(s): {}",
                        issues.len(),
                        messages.join("; ")
                    ),
                    errors: issues
                        .into_iter()
                        .map(from_json)
                        .collect::<Result<_, _>>()?,
                });
            }
        };
        let written: serde_json::Value = serde_json::from_str(&stamped)
            .map_err(|e| failure(format!("program/eval: stamped program does not parse: {e}")))?;
        let name = written
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target = project.join("orbitals");
        std::fs::create_dir_all(&target).map_err(|e| failure(e.to_string()))?;
        std::fs::write(
            target.join(format!("{name}.orb")),
            serde_json::to_string_pretty(&written).map_err(|e| failure(e.to_string()))?,
        )
        .map_err(|e| failure(e.to_string()))?;
        let behavior = format!("./orbitals/{name}");
        let traits: Vec<serde_json::Value> = written
            .get("orbitals")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|o| {
                o.get("traits")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|t| {
                t.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(|n| serde_json::json!({ "behavior": behavior, "trait": n }))
            })
            .collect();
        from_json(
            serde_json::json!({ "behavior": behavior, "traits": traits, "value": { "behavior": behavior } }),
        )
    }

    /// `program/step`: each request `{orbital, trait, from, event, payload}` played against the program in one
    /// kernel run, from its own `from` state; each answered as asked with the circuit it ran.
    fn step(
        &self,
        program: serde_json::Value,
        requests: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let op = "program/step";
        let requests = requests
            .map(to_json)
            .and_then(|r| r.as_array().cloned())
            .ok_or_else(|| failure(format!("{op}: expected a list of requests")))?;
        for r in &requests {
            for key in ["orbital", "trait", "from", "event"] {
                if !r.get(key).is_some_and(serde_json::Value::is_string) {
                    return Err(failure(format!("{op}: each request needs a {key}")));
                }
            }
        }
        let steps: Vec<serde_json::Value> = requests
            .iter()
            .map(|r| serde_json::json!({ "trait": r["trait"], "event": r["event"], "payload": r.get("payload").cloned().unwrap_or(serde_json::json!({})), "from": r["from"] }))
            .collect();
        let scratch = Scratch::new("almadar-step")?;
        let path = scratch.write("program.orb", &program.to_string())?;
        let report = crate::commands::execute::run_cascade(&path, &serde_json::Value::Array(steps))
            .map_err(|e| failure(format!("{op}: {e}")))?;
        if report.get("success") != Some(&serde_json::Value::Bool(true)) {
            return Err(failure(format!(
                "{op}: {}",
                report
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the run failed")
            )));
        }
        let mut played = Vec::new();
        for (i, request) in requests.iter().enumerate() {
            let trait_name = request["trait"].as_str().unwrap_or_default();
            let outcome = report["steps"][i]["outcomes"]
                .as_array()
                .and_then(|os| {
                    os.iter()
                        .find(|o| {
                            o.get("trait").and_then(serde_json::Value::as_str) == Some(trait_name)
                        })
                        .cloned()
                })
                .ok_or_else(|| failure(format!("{op}: trait \"{trait_name}\" did not run")))?;
            let fired = outcome.get("firedArm").filter(|v| !v.is_null()).cloned();
            let guards = outcome
                .get("guards")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let fired_guarded = fired
                .as_ref()
                .is_some_and(|f| guards.iter().any(|g| g.get("arm") == Some(f)));
            let guard = if fired_guarded {
                "pass"
            } else if fired.is_none() && !guards.is_empty() {
                "fail"
            } else {
                "none"
            };
            let mut result = serde_json::json!({
                "trait": outcome["trait"],
                "event": request["event"],
                "transitionFired": outcome["executed"],
                "guard": guard,
                "state": { "before": outcome["previousState"], "after": outcome["newState"] },
                "effects": outcome.get("effects").cloned().unwrap_or(serde_json::json!([])),
                "emitted": outcome.get("emits").and_then(serde_json::Value::as_array).into_iter().flatten().map(|e| {
                    let mut out = serde_json::json!({ "type": e["event"], "timestamp": i });
                    if let Some(p) = e.get("payload").filter(|p| !p.is_null()) { out["payload"] = p.clone(); }
                    out
                }).collect::<Vec<_>>(),
                "guards": guards.iter().map(|g| {
                    let mut out = serde_json::json!({ "arm": g["arm"], "guard": g["guard"], "passed": g["passed"], "trace": [] });
                    if let Some(e) = g.get("error") { out["error"] = e.clone(); }
                    out
                }).collect::<Vec<_>>(),
                "ensures": outcome.get("ensures").cloned().unwrap_or(serde_json::json!([])),
            });
            if let Some(f) = fired {
                result["firedArm"] = f;
            }
            let mut step = request.clone();
            step["result"] = result;
            played.push(step);
        }
        from_json(serde_json::Value::Array(played))
    }
}

const LAYOUT_STRATEGIES: [&str; 5] = ["sidebar", "tabs", "dashboard", "wizard-flow", "single"];

impl ProgramHost {
    /// `program/compose`: the programs, in order, as one app program (`orbital_compiler::composition::compose_programs`).
    fn compose(&self, args: &[Value]) -> Result<Value, SubstrateFailure> {
        let op = "program/compose";
        let Some(Value::Array(list)) = args.first() else {
            return Err(failure("program/compose: expected a list of programs"));
        };
        let programs = list
            .iter()
            .map(|p| program_arg(op, Some(p)))
            .collect::<Result<Vec<_>, _>>()?;
        let options = args
            .get(1)
            .map(to_json)
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| failure("program/compose: options must be an object with an appName"))?;
        let (mut app_name, mut layout, mut theme) = (None, None, None);
        for (key, value) in options.as_object().into_iter().flatten() {
            match (key.as_str(), value.as_str()) {
                ("appName", Some(v)) if !v.is_empty() => app_name = Some(v.to_string()),
                ("theme", Some(v)) => theme = (!v.is_empty()).then(|| v.to_string()),
                ("layout", v) => match v.filter(|l| LAYOUT_STRATEGIES.contains(l)) {
                    Some(l) => layout = Some(l.to_string()),
                    None => {
                        return Err(failure(format!(
                            "program/compose: layout must be one of {}",
                            LAYOUT_STRATEGIES.join(", ")
                        )))
                    }
                },
                _ => {
                    return Err(failure(format!(
                        "program/compose: unsupported option \"{key}\""
                    )))
                }
            }
        }
        let app_name = app_name
            .ok_or_else(|| failure("program/compose: options must name the app (appName)"))?;
        let now = orbital_core::runtime::mock_random::iso_from_ms(
            orbital_core::time_util::now_millis() as f64,
        );
        let load = |from: &str| {
            crate::commands::behaviors_catalog::source(from)
                .map_err(|e| format!("behavior/source: {e}"))
        };
        let schema = orbital_compiler::composition::compose_programs(
            &programs,
            &app_name,
            layout.as_deref(),
            theme.as_deref(),
            &now,
            &load,
        )
        .map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(schema)
    }
}

impl SubstrateHandler for ProgramHost {
    fn run(&self, op: &str, args: &[Value]) -> Result<Value, SubstrateFailure> {
        match op {
            "program/read" => match args.first() {
                Some(Value::String(text)) => self.read(text),
                _ => Err(failure("program/read: expected .lolo text")),
            },
            "program/print" => self.print(program_arg(op, args.first())?),
            "program/validate" => self.validate(program_arg(op, args.first())?, args.get(1)),
            "behavior/catalog" => self.catalog(args.first()),
            "behavior/describe" => {
                let target = Self::describe_target(op, args.first())?;
                let described = crate::commands::behaviors_catalog::describe(target)
                    .map_err(|e| failure(format!("{op}: {e}")))?;
                from_json(described)
            }
            "behavior/source" => {
                let behavior = Self::behavior_of(op, args.first())?;
                let source = crate::commands::behaviors_catalog::source(&behavior)
                    .map_err(|e| failure(format!("{op}: {e}")))?;
                from_json(source)
            }
            "behavior/resolve" => self.resolve_behavior(&Self::behavior_of(op, args.first())?),
            "program/compose" => self.compose(args),
            "program/eval" => self.eval(program_arg(op, args.first())?, args.get(1)),
            "program/step" => self.step(program_arg(op, args.first())?, args.get(1)),
            "behavior/dependencies" => self.dependencies(program_arg(op, args.first())?),
            other => Err(failure(format!(
                "{other}: not an op the native program host answers"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = r#"app note-pad [
  version: "1.0.0",
  description: "One note."
]

orbital NoteOrbital {
  entity Note [runtime] {
    id : string!
    text : string = ""
  }

  trait NoteView -> Note [interaction] {
    initial: idle

    state idle {
      INIT -> idle
        (set @entity.text "hello")
        (render-ui main { type: "typography", content: @entity.text })
    }
  }

  page "/" -> NoteView
}
"#;

    fn json(value: &Value) -> serde_json::Value {
        to_json(value)
    }

    #[test]
    fn read_lowers_text_to_the_program_emit_orb_prints_and_print_writes_it_back() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string())])
            .expect("reads");
        let as_json = json(&program);
        assert_eq!(as_json["name"], "note-pad");
        let expected: serde_json::Value =
            serde_json::from_str(&crate::emit::orb_json(NOTE, "note.lolo").unwrap()).unwrap();
        assert_eq!(
            as_json,
            json(&from_json(expected).unwrap()),
            "the value `orb emit orb` prints, as a kernel value"
        );
        let printed = host.run("program/print", &[program]).expect("prints");
        let Value::String(text) = printed else {
            panic!("text")
        };
        assert!(
            text.contains("note-pad") && text.contains("NoteView"),
            "{text}"
        );
    }

    #[test]
    fn validate_reports_the_validators_findings_and_a_bad_name_before_any_run() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string())])
            .unwrap();
        let report = json(
            &host
                .run("program/validate", &[program.clone()])
                .expect("validates"),
        );
        assert!(
            report["errors"].is_array() && report["warnings"].is_array(),
            "{report}"
        );
        let mut renamed = json(&program);
        renamed["name"] = serde_json::json!("not a file name");
        let refused = json(
            &host
                .run("program/validate", &[from_json(renamed).unwrap()])
                .unwrap(),
        );
        assert_eq!(refused["valid"], false);
        assert_eq!(refused["errors"][0]["code"], "PROGRAM_NAME_INVALID");
    }

    #[test]
    fn the_behavior_ops_answer_what_the_behaviors_commands_print() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let source = json(
            &host
                .run("behavior/source", &[value.clone()])
                .expect("source"),
        );
        let printed = crate::commands::behaviors_catalog::source("almadar-std/std-retry").unwrap();
        assert_eq!(source, json(&from_json(printed).unwrap()));
        let described = json(
            &host
                .run("behavior/describe", &[value.clone()])
                .expect("describe"),
        );
        assert_eq!(described["behavior"], "almadar-std/std-retry");
        let resolved = json(&host.run("behavior/resolve", &[value]).expect("resolve"));
        assert_eq!(resolved["name"], "std-retry");
        let missing =
            from_json(serde_json::json!({ "behavior": "almadar-std/no-such-behavior" })).unwrap();
        assert!(
            host.run("behavior/source", &[missing]).is_err(),
            "control: an unknown behavior fails"
        );
    }

    #[test]
    fn program_compose_assembles_an_orbital_value_into_an_app_carrying_its_hooks() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "Drop", "orbitals": [{ "behavior": "almadar-std/std-file-drop", "orbital": "FileDropOrbital" }] })).unwrap();
        let options =
            from_json(serde_json::json!({ "appName": "DropApp", "theme": "ocean" })).unwrap();
        let schema = json(
            &host
                .run(
                    "program/compose",
                    &[Value::Array(vec![program.clone()]), options],
                )
                .expect("compose"),
        );
        assert_eq!(schema["name"], "DropApp");
        assert_eq!(schema["theme"], "ocean");
        assert_eq!(
            schema["inbound"],
            serde_json::json!(["file-drop"]),
            "the imported behavior's hook joins the app"
        );
        assert_eq!(
            schema["orbitals"][0]["reference"]["ref"],
            "Source.orbitals.FileDropOrbital"
        );
        let bad = from_json(serde_json::json!({ "appName": "X", "layout": "grid" })).unwrap();
        assert!(
            host.run("program/compose", &[Value::Array(vec![program]), bad])
                .unwrap_err()
                .message
                .contains("layout must be one of"),
            "control: an unknown layout is refused"
        );
    }

    #[test]
    fn eval_writes_a_valid_program_into_the_project_and_refuses_an_invalid_one() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = json(
            &host
                .run("program/read", &[Value::String(NOTE.to_string())])
                .unwrap(),
        );
        let project = std::env::temp_dir().join(format!("orb-eval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let out = json(
            &host
                .eval_in(&project, program.clone(), None)
                .expect("a valid program evaluates"),
        );
        assert_eq!(out["behavior"], "./orbitals/note-pad");
        assert_eq!(
            out["traits"],
            serde_json::json!([{ "behavior": "./orbitals/note-pad", "trait": "NoteView" }])
        );
        assert!(
            project.join("orbitals/note-pad.orb").is_file(),
            "the program is written into the project"
        );
        let mut broken = program.clone();
        broken["orbitals"][0]["traits"][0]["stateMachine"]["states"] = serde_json::json!([]);
        let refused = host.eval_in(&project, broken, None).unwrap_err();
        assert!(
            refused.message.starts_with("program/eval: ") && !refused.errors.is_empty(),
            "control: an invalid program is refused with its findings: {}",
            refused.message
        );
    }

    #[test]
    fn step_plays_a_transition_and_dependencies_list_what_the_program_imports() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string())])
            .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "NoteOrbital", "trait": "NoteView", "from": "idle", "event": "INIT", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program.clone(), requests])
                .expect("steps"),
        );
        assert_eq!(played[0]["result"]["transitionFired"], true);
        assert_eq!(
            played[0]["result"]["state"],
            serde_json::json!({ "before": "idle", "after": "idle" })
        );
        let bad = from_json(serde_json::json!([{ "trait": "NoteView" }])).unwrap();
        assert!(
            host.run("program/step", &[program, bad])
                .unwrap_err()
                .message
                .contains("needs a orbital"),
            "control: a request without its orbital is refused"
        );
        let importer = from_json(serde_json::json!({ "name": "Uses", "orbitals": [{ "name": "R", "uses": [{ "from": "std/behaviors/std-retry", "as": "Retry" }], "traits": [], "pages": [] }] })).unwrap();
        let graph = json(
            &host
                .run("behavior/dependencies", &[importer])
                .expect("graph"),
        );
        assert_eq!(graph["orbitals"]["R"], serde_json::json!(["std-retry"]));
        assert_eq!(graph["behaviors"]["std-retry"]["layer"], "std");
    }

    #[test]
    fn step_reports_each_ensures_judged_by_the_kernel_and_whether_a_failure_was_routed() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "CounterApp", "orbitals": [{ "name": "Counting",
            "entity": { "name": "Counter", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "count", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "Count", "linkedEntity": "Counter", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }],
                "transitions": [
                    { "from": "ready", "to": "ready", "event": "INC",
                      "effects": [["set", "@entity.count", ["+", "@entity.count", 1]]],
                      "ensures": [["=", "@entity.count", ["+", "@fromEntity.count", 1]], ["=", "@entity.count", 99]] },
                    { "from": "ready", "to": "ready", "event": "GATED", "guard": ["=", 1, 2],
                      "ensures": [["=", "@entity.count", 0]] }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([
            { "orbital": "Counting", "trait": "Count", "from": "ready", "event": "INC", "payload": {} },
            { "orbital": "Counting", "trait": "Count", "from": "ready", "event": "GATED", "payload": {} }
        ]))
        .unwrap();
        let played = json(&host.run("program/step", &[program, requests]).expect("steps"));
        let inc = &played[0]["result"];
        assert_eq!(inc["effects"][0]["routed"], false, "an executed set routes nothing");
        assert_eq!(inc["ensures"][0]["verdict"], "held", "{inc}");
        assert_eq!(inc["ensures"][1]["verdict"], "broken");
        assert_eq!(inc["ensures"][1]["proposition"], serde_json::json!(["=", "@entity.count", 99]));
        assert_eq!(inc["ensures"][1]["counterexample"]["entity"]["count"], 1);
        assert_eq!(inc["ensures"][1]["counterexample"]["fromEntity"]["count"], 0);
        let gated = &played[1]["result"];
        assert_eq!(gated["transitionFired"], false);
        assert_eq!(gated["ensures"][0]["verdict"], "inconclusive", "{gated}");
        assert_eq!(gated["ensures"][0]["reason"], "the transition did not fire");
        assert!(
            serde_json::from_value::<orbital_core::program_machine::PlayedStep>(played[0].clone()).is_ok(),
            "the host answers the typed PlayedStep wire"
        );
    }

    #[test]
    fn control_a_bad_argument_and_an_op_not_hosted_here_fail_with_a_message() {
        let host = ProgramHost;
        assert!(host
            .run("program/read", &[Value::Number(1.0)])
            .unwrap_err()
            .message
            .contains("expected .lolo text"));
        assert!(host
            .run("program/print", &[Value::Null])
            .unwrap_err()
            .message
            .contains("not a program"));
        assert!(host
            .run("program/nonsense", &[])
            .unwrap_err()
            .message
            .contains("not an op the native program host answers"));
        assert!(host
            .run("program/compose", &[])
            .unwrap_err()
            .message
            .contains("expected a list of programs"));
    }
}
