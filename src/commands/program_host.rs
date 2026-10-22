//! The native host of the language trio's server effects (`program/read|print|validate`, `behavior/catalog|describe|
//! source|resolve`): the kernel's `SubstrateHandler` answered by `orb`'s own functions, in process. Its JS twin is
//! `runProgramEffect` in `@almadar/runtime` (`host/program`), which runs these same functions through the `orb`
//! commands (`emit orb`, `emit lolo`, `stamp` + `validate --json --deny-warnings`, `behaviors catalog|describe|source
//! --json`, `resolve`); each answer here is the value those commands print, staged the same way.

use crate::commands::behaviors_vectors::{Embedder, ProviderEmbedder};
use orbital_core::effects::executor::{SubstrateFailure, SubstrateHandler};
use orbital_core::Value;
use std::path::{Path, PathBuf};

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

/// A boolean option of an op's options argument; absent or not a boolean is `None`.
fn option_flag(options: Option<&Value>, key: &str) -> Option<bool> {
    options
        .map(to_json)
        .and_then(|o| o.get(key).and_then(serde_json::Value::as_bool))
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

    /// `behavior/catalog`: `options.vectors: false` omits the vectors, so neither the store nor `embedder` is read.
    fn catalog(
        scope: Option<&Value>,
        options: Option<&Value>,
        embedder: Option<&dyn Embedder>,
        user_dir: Option<&Path>,
    ) -> Result<Value, SubstrateFailure> {
        use crate::commands::behaviors_catalog::{catalog_with, Vectors};
        let vectors = match option_flag(options, "vectors") {
            Some(false) => Vectors::Omit,
            _ => Vectors::Attach(embedder),
        };
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
        let entries = catalog_with(&paths, &exposure, vectors, user_dir)
            .map_err(|e| failure(format!("behavior/catalog: {e}")))?;
        from_json(serde_json::to_value(entries).map_err(|e| failure(e.to_string()))?)
    }

    /// `behavior/describe`: `options.knobVectors: true` attaches a trait's knob vectors as `orb behaviors describe
    /// --knob-vectors` does, a miss embedded by `embedder`.
    fn describe(
        op: &str,
        value: Option<&Value>,
        options: Option<&Value>,
        embedder: Option<&dyn Embedder>,
        user_dir: Option<&Path>,
    ) -> Result<Value, SubstrateFailure> {
        use crate::commands::behaviors_catalog::{describe_with, Vectors};
        let target = Self::describe_target(op, value)?;
        let vectors = match option_flag(options, "knobVectors") {
            Some(true) => Vectors::Attach(embedder),
            _ => Vectors::Omit,
        };
        let described = describe_with(target, None, vectors, user_dir)
            .map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(described)
    }

    /// `behavior/texts`: the embedding texts under `{ paths, kind? }`, as `orb behaviors texts --json` prints them.
    fn texts(scope: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let json = scope.map(to_json).unwrap_or(serde_json::Value::Null);
        let Some(paths) = json
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()
            })
            .flatten()
        else {
            return Err(failure(
                "behavior/texts: paths: expected a list of registry folders",
            ));
        };
        let kind = match json.get("kind") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(k)) => Some(k.as_str()),
            Some(_) => return Err(failure("behavior/texts: kind: expected a string")),
        };
        let texts = crate::commands::behaviors_texts::texts(&paths, kind)
            .map_err(|e| failure(format!("behavior/texts: {e}")))?;
        from_json(serde_json::to_value(texts).map_err(|e| failure(e.to_string()))?)
    }

    /// `behavior/packages`: each installed behavior package's `{ root, prefix, vectors }`, `vectors` being the
    /// `orb.vectors` snapshot the lock resolves (the cached sidecar when locked), `null` when none is declared.
    fn packages() -> Result<Value, SubstrateFailure> {
        let cwd = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        let packages = super::npm_project::discover_from(&cwd)
            .map_err(|e| failure(format!("behavior/packages: {e}")))?;
        let mut out = Vec::new();
        for pkg in &packages {
            let vectors = crate::commands::sidecar_fetch::resolve_vectors(
                pkg,
                &crate::commands::sidecar_fetch::UreqFetcher,
            )
            .map_err(|e| failure(format!("behavior/packages: {e}")))?;
            out.push(serde_json::json!({
                "root": pkg.root.to_string_lossy(),
                "prefix": pkg.prefix,
                "vectors": vectors.map(|v| v.to_string_lossy().into_owned()),
            }));
        }
        from_json(serde_json::Value::Array(out))
    }

    /// `behavior/source`: the behavior as a program; with `options.file: true`, its source file as `{ path, source }`
    /// (the file text), what `orb mcp`'s `read_behavior` answers.
    fn source(
        op: &str,
        value: Option<&Value>,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let behavior = Self::behavior_of(op, value)?;
        if option_flag(options, "file") == Some(true) {
            let path = crate::commands::behaviors_catalog::behavior_source_path(&behavior)
                .map_err(|e| failure(format!("{op}: {e}")))?;
            let source = std::fs::read_to_string(&path)
                .map_err(|e| failure(format!("{op}: {}: {e}", path.display())))?;
            return from_json(serde_json::json!({ "path": path, "source": source }));
        }
        let source = crate::commands::behaviors_catalog::source(&behavior)
            .map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(source)
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

/// `importsOf`: each `uses` of the program's orbitals as `(behavior, specifier)`, once per behavior, the behavior
/// being the specifier's trailing name (the loader's rule).
fn imported_behaviors(program: &serde_json::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
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
            if !out.iter().any(|(b, _)| *b == behavior) {
                out.push((behavior, from.to_string()));
            }
        }
    }
    out
}

fn behavior_names(imports: &[(String, String)]) -> Vec<String> {
    imports.iter().map(|(b, _)| b.clone()).collect()
}

impl ProgramHost {
    /// `behavior/dependencies`: each orbital's direct imports, then every behavior they reach, once, with its layer
    /// (`primitive` for a UI pattern factory, else its package's tier) and its own imports. Each import is resolved
    /// by the loader the program's `uses` go through, so only the behaviors reached are read.
    fn dependencies(&self, program: serde_json::Value) -> Result<Value, SubstrateFailure> {
        let op = "behavior/dependencies";
        let cwd = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        let packages = super::npm_project::discover_from(&cwd)
            .map_err(|e| failure(format!("{op}: {e}")))?;
        let mut loader = orbital_compiler::loader::ExternalOrbitalLoader::new(
            orbital_compiler::loader::LoaderConfig {
                base_path: cwd,
                packages: packages.clone(),
                ..Default::default()
            },
        );
        let mut orbitals = serde_json::Map::new();
        let mut queue: std::collections::VecDeque<(String, String, Option<PathBuf>)> =
            std::collections::VecDeque::new();
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
            queue.extend(own.iter().map(|(b, spec)| (b.clone(), spec.clone(), None)));
            orbitals.insert(name, serde_json::json!(behavior_names(&own)));
        }
        let mut behaviors = serde_json::Map::new();
        while let Some((name, spec, from)) = queue.pop_front() {
            if behaviors.contains_key(&name) {
                continue;
            }
            let imported = crate::commands::behaviors_catalog::resolve_import(
                &mut loader,
                &packages,
                &spec,
                from.as_deref(),
            )
            .map_err(|e| failure(format!("{op}: {e}")))?;
            let (layer, imports) = match imported {
                Some(b) => {
                    let layer = match (&b.pattern, b.package.as_ref().map(|p| p.name.as_str())) {
                        (Some(_), _) => "primitive",
                        (None, Some("@almadar/std")) => "std",
                        (None, Some("@almadar-io/behaviors")) => "io",
                        (None, _) => "unknown",
                    };
                    let imports = imported_behaviors(&b.program);
                    queue.extend(imports.iter().map(|(n, s)| (n.clone(), s.clone(), Some(b.path.clone()))));
                    (layer, behavior_names(&imports))
                }
                None => ("unknown", Vec::new()),
            };
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
    /// kernel run, from its own `from` state; each answered as asked with the circuit it ran. `options.render`
    /// adds each request's rendered slots, walked by the render probe against the desktop renderer.
    fn step(
        &self,
        program: serde_json::Value,
        requests: Option<&Value>,
        options: Option<&Value>,
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
            if r.get("entity").is_some_and(|e| !e.is_object()) {
                return Err(failure(format!(
                    "{op}: a request's entity must be an object of fields"
                )));
            }
            if r.get("viewer")
                .is_some_and(|v| !v.is_object() && !v.is_null())
            {
                return Err(failure(format!(
                    "{op}: a request's viewer must be a user object or null"
                )));
            }
            if r.get("settleMs").is_some_and(|n| !n.is_number()) {
                return Err(failure(format!(
                    "{op}: a request's settleMs must be a number of milliseconds"
                )));
            }
            if r.get("config").is_some_and(|c| !c.is_object()) {
                return Err(failure(format!(
                    "{op}: a request's config must be an object of knob values"
                )));
            }
            if r.get("now").is_some_and(|n| !n.is_number()) {
                return Err(failure(format!(
                    "{op}: a request's now must be a number (ms since the epoch)"
                )));
            }
        }
        let steps: Vec<serde_json::Value> = requests
            .iter()
            .map(|r| {
                let mut step = serde_json::json!({ "trait": r["trait"], "event": r["event"], "payload": r.get("payload").cloned().unwrap_or(serde_json::json!({})), "from": r["from"] });
                for key in ["entity", "config", "now", "settleMs", "viewer"] {
                    if let Some(v) = r.get(key) {
                        step[key] = v.clone();
                    }
                }
                step
            })
            .collect();
        let scratch = Scratch::new("almadar-step")?;
        let path = scratch.write("program.orb", &program.to_string())?;
        let live = match options.map(to_json).as_ref().and_then(|o| o.get("live")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.live must be a boolean, not {other}"
                )))
            }
        };
        let report = crate::commands::execute::run_cascade_with(
            &path,
            &serde_json::Value::Array(steps),
            live,
        )
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
        let render = match options.map(to_json).as_ref().and_then(|o| o.get("render")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.render must be a boolean, not {other}"
                )))
            }
        };
        let renderer = if render {
            let schema = crate::commands::execute::resolve_program(&path)
                .map_err(|e| failure(format!("{op}: {e}")))?;
            let target = crate::utils::desktop::target()
                .map_err(|e| failure(format!("{op}: options.render: {e}")))?;
            Some((schema, target))
        } else {
            None
        };
        let emitted_of = |o: &serde_json::Value, i: usize| -> Vec<serde_json::Value> {
            o.get("emits")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .map(|e| {
                    let mut out = serde_json::json!({ "type": e["event"], "timestamp": i });
                    if let Some(p) = e.get("payload").filter(|p| !p.is_null()) {
                        out["payload"] = p.clone();
                    }
                    out
                })
                .collect()
        };
        let mut played = Vec::new();
        for (i, request) in requests.iter().enumerate() {
            let trait_name = request["trait"].as_str().unwrap_or_default();
            let moved = report["steps"][i]["outcomes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let outcome = moved
                .iter()
                .find(|o| o.get("trait").and_then(serde_json::Value::as_str) == Some(trait_name))
                .cloned()
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
                "failure": outcome.get("failure").cloned().unwrap_or(serde_json::Value::Null),
                "emitted": emitted_of(&outcome, i),
                "guards": guards.iter().map(|g| {
                    let mut out = serde_json::json!({ "arm": g["arm"], "guard": g["guard"], "passed": g["passed"], "trace": [] });
                    if let Some(e) = g.get("error") { out["error"] = e.clone(); }
                    out
                }).collect::<Vec<_>>(),
                "ensures": outcome.get("ensures").cloned().unwrap_or(serde_json::json!([])),
                "entityAfter": report["steps"][i]["entityAfter"],
                "outcomes": moved.iter().map(|o| serde_json::json!({
                    "orbital": o["orbital"],
                    "trait": o["trait"],
                    "event": o["event"],
                    "from": o["previousState"],
                    "to": o["newState"],
                    "arm": o["firedArm"],
                    "fired": o["executed"],
                    "effects": o.get("effects").cloned().unwrap_or(serde_json::json!([])),
                    "emitted": emitted_of(o, i),
                })).collect::<Vec<_>>(),
            });
            if let Some(f) = fired {
                result["firedArm"] = f;
            }
            if let Some((schema, target)) = &renderer {
                let payload: orbital_core::Value = serde_json::from_value(
                    request
                        .get("payload")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                )
                .map_err(|e| failure(format!("{op}: payload: {e}")))?;
                let slots = orbital_core::client::render_probe::played_render_facts(
                    schema,
                    trait_name,
                    request["event"].as_str().unwrap_or_default(),
                    payload,
                    target,
                    |s, _| orbital_core::client::render_probe::seeded_kernel(s),
                )
                .map_err(|e| failure(format!("{op}: render: {e}")))?;
                result["rendered"] =
                    serde_json::to_value(slots).map_err(|e| failure(e.to_string()))?;
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
        let (mut app_name, mut layout, mut theme, mut overrides) = (None, None, None, None);
        for (key, value) in options.as_object().into_iter().flatten() {
            match (key.as_str(), value.as_str()) {
                ("overrides", _) => overrides = Some(value.clone()),
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
            overrides.as_ref(),
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
            "behavior/catalog" => {
                let embedder = ProviderEmbedder::from_env();
                let embedder = embedder.as_ref().map(|e| e as &dyn Embedder);
                Self::catalog(args.first(), args.get(1), embedder, None)
            }
            "behavior/describe" => {
                let embedder = ProviderEmbedder::from_env();
                let embedder = embedder.as_ref().map(|e| e as &dyn Embedder);
                Self::describe(op, args.first(), args.get(1), embedder, None)
            }
            "behavior/texts" => Self::texts(args.first()),
            "behavior/packages" => Self::packages(),
            "behavior/source" => Self::source(op, args.first(), args.get(1)),
            "behavior/resolve" => self.resolve_behavior(&Self::behavior_of(op, args.first())?),
            "program/compose" => self.compose(args),
            "program/eval" => self.eval(program_arg(op, args.first())?, args.get(1)),
            "program/step" => self.step(program_arg(op, args.first())?, args.get(1), args.get(2)),
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
    fn source_with_the_file_option_answers_the_path_and_file_text_as_read_behavior_does() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let file = from_json(serde_json::json!({ "file": true })).unwrap();
        let answered =
            json(&ProgramHost::source("behavior/source", Some(&value), Some(&file)).expect("file"));
        let path =
            crate::commands::behaviors_catalog::behavior_source_path("almadar-std/std-retry")
                .unwrap();
        assert_eq!(
            answered,
            serde_json::json!({ "path": path, "source": std::fs::read_to_string(&path).unwrap() })
        );
        assert!(answered["path"]
            .as_str()
            .unwrap()
            .ends_with("std-retry.lolo"));
        let program =
            json(&ProgramHost::source("behavior/source", Some(&value), None).expect("program"));
        assert_eq!(
            program,
            json(
                &from_json(
                    crate::commands::behaviors_catalog::source("almadar-std/std-retry").unwrap()
                )
                .unwrap()
            ),
            "control: without the option the answer stays the program"
        );
        assert!(program.get("path").is_none());
    }

    /// An embedder that answers locally and counts the texts it embedded.
    struct CountingEmbedder(std::sync::Mutex<usize>);

    impl Embedder for CountingEmbedder {
        fn model(&self) -> &str {
            "test-model"
        }
        fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            *self.0.lock().unwrap() += texts.len();
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32, 1.0, -1.0, 0.5])
                .collect())
        }
    }

    #[test]
    fn describe_attaches_knob_vectors_only_with_the_knob_vectors_option_as_the_verb_does() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let store = Scratch::new("almadar-knob-vectors").unwrap();
        let fake = CountingEmbedder(std::sync::Mutex::new(0));
        let value =
            from_json(serde_json::json!({ "behavior": "almadar-std/std-retry", "trait": "Retry" }))
                .unwrap();
        let with = from_json(serde_json::json!({ "knobVectors": true })).unwrap();
        let attached = json(
            &ProgramHost::describe(
                "behavior/describe",
                Some(&value),
                Some(&with),
                Some(&fake),
                Some(&store.0),
            )
            .expect("describe with knob vectors"),
        );
        let knobs = attached["knobs"].as_array().unwrap();
        assert_eq!(knobs.len(), 4, "{attached}");
        assert!(knobs
            .iter()
            .all(|k| k["vector"].as_array().is_some_and(|v| v.len() == 4)));
        assert_eq!(attached["model"], "test-model");
        let verb = crate::commands::behaviors_catalog::describe_with(
            crate::commands::behaviors_catalog::DescribeTarget::Spec(
                "almadar-std/std-retry.traits.Retry".into(),
            ),
            None,
            crate::commands::behaviors_catalog::Vectors::Attach(Some(&fake)),
            Some(&store.0),
        )
        .unwrap();
        assert_eq!(
            attached,
            json(&from_json(verb).unwrap()),
            "the op answers what `describe --knob-vectors` prints"
        );

        let embedded = *fake.0.lock().unwrap();
        let without = json(
            &ProgramHost::describe(
                "behavior/describe",
                Some(&value),
                None,
                Some(&fake),
                Some(&store.0),
            )
            .expect("describe"),
        );
        assert!(without["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k.get("vector").is_none()));
        assert_eq!(without["model"], "");
        let off = from_json(serde_json::json!({ "knobVectors": false })).unwrap();
        let declined = json(
            &ProgramHost::describe(
                "behavior/describe",
                Some(&value),
                Some(&off),
                Some(&fake),
                Some(&store.0),
            )
            .expect("describe knobVectors false"),
        );
        assert_eq!(declined, without);
        assert_eq!(
            *fake.0.lock().unwrap(),
            embedded,
            "control: no knob vectors, no embedding"
        );
    }

    #[test]
    fn catalog_with_vectors_false_reads_no_store_and_no_provider() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let store = Scratch::new("almadar-catalog-vectors").unwrap();
        let fake = CountingEmbedder(std::sync::Mutex::new(0));
        let scope =
            from_json(serde_json::json!({ "paths": ["almadar-std/ml/organisms"] })).unwrap();
        let off = from_json(serde_json::json!({ "vectors": false })).unwrap();
        let bare = json(
            &ProgramHost::catalog(Some(&scope), Some(&off), Some(&fake), Some(&store.0))
                .expect("catalog without vectors"),
        );
        let entries = bare.as_array().unwrap();
        assert!(!entries.is_empty());
        assert!(entries
            .iter()
            .all(|e| e["vectors"] == serde_json::json!({}) && e["model"] == ""));
        assert_eq!(*fake.0.lock().unwrap(), 0, "the provider is never asked");
        assert_eq!(
            std::fs::read_dir(&store.0).unwrap().count(),
            0,
            "the user store is never opened"
        );

        let full = json(
            &ProgramHost::catalog(Some(&scope), None, Some(&fake), Some(&store.0))
                .expect("catalog"),
        );
        assert!(
            full.as_array()
                .unwrap()
                .iter()
                .all(|e| e["model"] == "test-model"
                    && e["vectors"].as_object().is_some_and(|v| !v.is_empty())),
            "control: by default the vectors are attached"
        );
    }

    #[test]
    fn texts_answers_what_behaviors_texts_prints_in_embed_order() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let paths = vec!["almadar-std/ml/organisms".to_string()];
        let scope = from_json(serde_json::json!({ "paths": paths })).unwrap();
        let all = json(&ProgramHost::texts(Some(&scope)).expect("texts"));
        let printed = crate::commands::behaviors_texts::texts(&paths, None).unwrap();
        assert_eq!(
            all,
            json(&from_json(serde_json::to_value(&printed).unwrap()).unwrap())
        );
        let keys: Vec<&str> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap())
            .collect();
        let organisms = keys
            .iter()
            .take_while(|k| !k.contains('#') && !k.contains('/'))
            .count();
        assert!(organisms > 0, "{keys:?}");
        let capabilities = keys[organisms..]
            .iter()
            .take_while(|k| k.ends_with("#capabilities"))
            .count();
        assert!(
            keys[organisms + capabilities..]
                .iter()
                .all(|k| k.contains('/')),
            "organism texts, then capability texts, then knob texts: {keys:?}"
        );

        let only = from_json(serde_json::json!({ "paths": paths, "kind": "capability" })).unwrap();
        let capability = json(&ProgramHost::texts(Some(&only)).expect("capability texts"));
        assert_eq!(capability.as_array().unwrap().len(), capabilities);
        let bad = from_json(serde_json::json!({ "paths": paths, "kind": "nope" })).unwrap();
        assert!(
            ProgramHost::texts(Some(&bad)).is_err(),
            "control: an unknown kind fails"
        );
        let none = from_json(serde_json::json!({ "kind": "knob" })).unwrap();
        assert!(
            ProgramHost::texts(Some(&none)).is_err(),
            "control: no paths fails"
        );
    }

    #[test]
    fn packages_lists_each_installed_package_with_its_resolved_vectors() {
        let listed = json(&ProgramHost::packages().expect("packages"));
        let cwd = std::env::current_dir().unwrap();
        let installed = crate::commands::npm_project::discover_from(&cwd).unwrap();
        let entries = listed.as_array().unwrap();
        assert_eq!(entries.len(), installed.len());
        assert!(!entries.is_empty());
        for (entry, pkg) in entries.iter().zip(&installed) {
            assert_eq!(entry["prefix"], pkg.prefix.as_str());
            assert_eq!(entry["root"], pkg.root.to_string_lossy().as_ref());
            let vectors = crate::commands::sidecar_fetch::resolve_vectors(
                pkg,
                &crate::commands::sidecar_fetch::UreqFetcher,
            )
            .unwrap();
            match vectors {
                Some(path) => assert_eq!(entry["vectors"], path.to_string_lossy().as_ref()),
                None => assert!(
                    entry["vectors"].is_null(),
                    "control: no orb.vectors is null"
                ),
            }
        }
        assert!(entries.iter().any(|e| e["prefix"] == "almadar-std"));
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
        let alone = from_json(serde_json::json!({ "name": "Alone", "orbitals": [{ "name": "A", "traits": [], "pages": [] }] })).unwrap();
        let graph = json(&host.run("behavior/dependencies", &[alone]).expect("graph"));
        assert_eq!(
            graph,
            serde_json::json!({ "orbitals": { "A": [] }, "behaviors": {} }),
            "control: a program with no uses lists none"
        );
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
        let played = json(
            &host
                .run("program/step", &[program.clone(), requests.clone()])
                .expect("steps"),
        );
        let inc = &played[0]["result"];
        assert_eq!(
            inc["effects"][0]["routed"], false,
            "an executed set routes nothing"
        );
        assert_eq!(inc["ensures"][0]["verdict"], "held", "{inc}");
        assert_eq!(inc["ensures"][1]["verdict"], "broken");
        assert_eq!(
            inc["ensures"][1]["proposition"],
            serde_json::json!(["=", "@entity.count", 99])
        );
        assert_eq!(inc["ensures"][1]["counterexample"]["entity"]["count"], 1);
        assert_eq!(
            inc["ensures"][1]["counterexample"]["fromEntity"]["count"],
            0
        );
        let gated = &played[1]["result"];
        assert_eq!(gated["transitionFired"], false);
        assert_eq!(gated["ensures"][0]["verdict"], "inconclusive", "{gated}");
        assert_eq!(gated["ensures"][0]["reason"], "the transition did not fire");
        assert!(
            serde_json::from_value::<orbital_core::program_machine::PlayedStep>(played[0].clone())
                .is_ok(),
            "the host answers the typed PlayedStep wire"
        );
        assert!(
            inc.get("rendered").is_none(),
            "control: no render facts unless options.render asks"
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    requests,
                    from_json(serde_json::json!({ "render": "yes" })).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("options.render must be a boolean"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn step_seeds_the_entity_and_pins_now_for_that_request_only() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "GateApp", "orbitals": [{ "name": "Gating",
            "entity": { "name": "Gate", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "owner", "type": "string", "default": "" }] },
            "pages": [],
            "traits": [{ "name": "G", "linkedEntity": "Gate", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }, { "name": "open" }],
                "transitions": [
                    { "from": "ready", "to": "open", "event": "OPEN", "guard": ["=", "@entity.owner", "ann"] },
                    { "from": "ready", "to": "open", "event": "LATE", "guard": [">", "@now", 1000] }
                ] } }] }] }))
        .unwrap();
        let step = |extra: serde_json::Value| {
            let mut r = serde_json::json!({ "orbital": "Gating", "trait": "G", "from": "ready", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            r
        };
        let requests = from_json(serde_json::json!([
            step(serde_json::json!({ "event": "OPEN", "entity": { "owner": "ann" } })),
            step(serde_json::json!({ "event": "OPEN", "entity": { "owner": "bob" } })),
            step(serde_json::json!({ "event": "LATE", "now": 0 })),
            step(serde_json::json!({ "event": "LATE" }))
        ]))
        .unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "a seeded owner admits the guard: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "fail",
            "control: another owner does not"
        );
        assert_eq!(
            played[2]["result"]["guard"], "fail",
            "a pinned @now of 0 is not past 1000: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the real clock is"
        );
        assert_eq!(
            played[0]["entity"],
            serde_json::json!({ "owner": "ann" }),
            "the request is answered as asked"
        );
    }

    #[test]
    fn step_reports_each_cascaded_arm_with_its_own_effects() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RelayApp",
            "osGrants": { "fs": [], "exec": [], "net": [], "env": ["HOME"], "stdio": [], "sys": [], "desktop": [] },
            "orbitals": [{ "name": "Auth",
            "entity": { "name": "Key", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "ApiKeys", "linkedEntity": "Key", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "ISSUE", "effects": [["emit", "ISSUED", {}]] }] } },
                { "name": "ServerBearer", "linkedEntity": "Key",
                  "listens": [{ "event": "ISSUED", "triggers": "ARM", "source": { "kind": "trait", "trait": "ApiKeys" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "armed" }],
                    "transitions": [
                        { "from": "idle", "to": "armed", "event": "ARM", "effects": [["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                        { "from": "armed", "to": "armed", "event": "GOT" }, { "from": "armed", "to": "idle", "event": "NOGO" }
                    ] } }
            ] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Auth", "trait": "ApiKeys", "from": "idle", "event": "ISSUE", "payload": {} }])).unwrap();
        let played = json(&host.run("program/step", &[program, requests]).expect("steps"));
        let r = &played[0]["result"];
        assert_eq!(r["outcomes"][1]["effects"][0]["type"], "os/getenv", "the cascaded arm reports its own effect: {r}");
        assert_eq!(r["outcomes"][0]["effects"], r["effects"], "control: the requested trait's entry carries the top-level effects");
        assert_eq!(r["effects"].as_array().map(Vec::len), Some(1), "the top-level effects stay the requested arm's only: {r}");
    }

    #[test]
    fn step_lists_every_trait_the_dispatch_cascade_moved_in_order() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RelayApp", "orbitals": [{ "name": "Auth",
            "entity": { "name": "Key", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "ApiKeys", "linkedEntity": "Key", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "ISSUE", "effects": [["emit", "ISSUED", {}]] }] } },
                { "name": "ServerBearer", "linkedEntity": "Key",
                  "listens": [{ "event": "ISSUED", "triggers": "ARM", "source": { "kind": "trait", "trait": "ApiKeys" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "armed" }],
                    "transitions": [{ "from": "idle", "to": "armed", "event": "ARM" }] } }
            ] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Auth", "trait": "ApiKeys", "from": "idle", "event": "ISSUE", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let outcomes = &played[0]["result"]["outcomes"];
        assert_eq!(outcomes.as_array().map(Vec::len), Some(2), "{outcomes}");
        assert_eq!(outcomes[0]["orbital"], "Auth");
        assert_eq!(outcomes[0]["trait"], "ApiKeys");
        assert_eq!(outcomes[0]["emitted"][0]["type"], "ISSUED");
        assert_eq!(outcomes[0]["event"], "ISSUE");
        assert_eq!(outcomes[0]["arm"], 0);
        assert_eq!(outcomes[1]["trait"], "ServerBearer");
        assert_eq!(
            outcomes[1]["event"], "ARM",
            "the listener's own event: {outcomes}"
        );
        assert_eq!(outcomes[1]["fired"], true);
        assert_eq!(outcomes[1]["arm"], 0);
        assert_eq!(
            (&outcomes[1]["from"], &outcomes[1]["to"]),
            (&serde_json::json!("idle"), &serde_json::json!("armed"))
        );
    }

    #[test]
    fn control_a_single_trait_step_has_one_outcome_equal_to_its_top_level_fields() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EditorApp", "orbitals": [{ "name": "Editor",
            "entity": { "name": "Doc", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "Modes", "linkedEntity": "Doc", "stateMachine": {
                "states": [{ "name": "NORMAL", "isInitial": true }, { "name": "INSERT" }],
                "transitions": [{ "from": "NORMAL", "to": "INSERT", "event": "KEY", "guard": ["=", "@payload.key", "i"] }] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Editor", "trait": "Modes", "from": "NORMAL", "event": "KEY", "payload": { "key": "i" } }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(
            r["outcomes"],
            serde_json::json!([{ "orbital": "Editor", "trait": r["trait"], "event": r["event"], "from": r["state"]["before"], "to": r["state"]["after"], "arm": r["firedArm"], "fired": r["transitionFired"], "effects": r["effects"], "emitted": r["emitted"] }])
        );
    }

    #[test]
    fn step_carries_an_array_of_objects_and_bytes_to_the_next_request() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "CarryApp", "orbitals": [{ "name": "Carry",
            "entity": { "name": "Box", "persistence": "runtime", "fields": [
                { "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 },
                { "name": "body", "type": "bytes" },
                { "name": "items", "type": "array", "default": [], "items": { "type": "object", "properties": { "name": { "name": "name", "type": "string" } } } }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Box", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "done" }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "PUT", "effects": [
                        ["set", "@entity.items", ["array/append", "@entity.items", { "name": "x" }]],
                        ["set", "@entity.body", ["bytes/from-string", "hi"]],
                        ["set", "@entity.n", 1]] },
                    { "from": "idle", "to": "done", "event": "HAS_ITEM", "guard": [">", ["array/len", "@entity.items"], 0] },
                    { "from": "idle", "to": "done", "event": "HAS_BODY", "guard": ["=", ["bytes/to-string", "@entity.body"], "hi"] },
                    { "from": "idle", "to": "done", "event": "HAS_N", "guard": ["=", "@entity.n", 1] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Carry", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([
                            req("PUT"),
                            req("HAS_ITEM"),
                            req("HAS_BODY"),
                            req("HAS_N")
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[1]["result"]["guard"], "pass",
            "the appended object is there: {}",
            played[1]
        );
        assert_eq!(
            played[2]["result"]["guard"], "pass",
            "the bytes read back: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the scalar carries"
        );
    }

    #[test]
    fn step_seeds_nested_arrays_of_objects_and_keeps_the_unseeded_siblings() {
        let host = ProgramHost;
        let obj =
            |props: serde_json::Value| serde_json::json!({ "type": "object", "properties": props });
        let program = from_json(serde_json::json!({ "name": "JudgeApp", "orbitals": [{ "name": "Judge",
            "entity": { "name": "Run", "persistence": "runtime", "fields": [
                { "name": "id", "type": "string" },
                { "name": "played", "type": "array", "default": [], "items": obj(serde_json::json!({ "id": { "name": "id", "type": "string" }, "targets": { "name": "targets", "type": "array", "items": obj(serde_json::json!({ "t": { "name": "t", "type": "string" } })) } })) },
                { "name": "judged", "type": "array", "default": [], "items": obj(serde_json::json!({ "id": { "name": "id", "type": "string" }, "verdicts": { "name": "verdicts", "type": "array", "items": obj(serde_json::json!({ "v": { "name": "v", "type": "string" } })) } })) }] },
            "pages": [],
            "traits": [{ "name": "J", "linkedEntity": "Run", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "done" }],
                "transitions": [
                    { "from": "idle", "to": "done", "event": "TARGETS", "guard": ["=", ["object/get", ["array/nth", ["object/get", ["array/nth", "@entity.played", 0], "targets"], 0], "t"], "a"] },
                    { "from": "idle", "to": "done", "event": "VERDICTS", "guard": ["=", ["object/get", ["array/nth", ["object/get", ["array/nth", "@entity.judged", 0], "verdicts"], 0], "v"], "ok"] },
                    { "from": "idle", "to": "done", "event": "NO_JUDGED", "guard": ["=", ["array/len", "@entity.judged"], 0] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str, entity: serde_json::Value| serde_json::json!({ "orbital": "Judge", "trait": "J", "from": "idle", "event": event, "payload": {}, "entity": entity });
        let full = serde_json::json!({ "played": [{ "id": "p", "targets": [{ "t": "a" }] }], "judged": [{ "id": "j", "verdicts": [{ "v": "ok" }] }] });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([
                            req("TARGETS", full.clone()),
                            req("VERDICTS", full),
                            req("TARGETS", serde_json::json!({ "judged": [] })),
                            req("NO_JUDGED", serde_json::json!({ "judged": [] }))
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "nested targets read back: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "pass",
            "nested verdicts read back: {}",
            played[1]
        );
        assert_eq!(
            played[2]["result"]["guard"], "pass",
            "seeding judged keeps played: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the seeded field itself is replaced"
        );
    }

    #[test]
    fn step_deletes_a_row_an_earlier_request_of_the_same_call_created() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "PApp", "orbitals": [{ "name": "P",
            "entity": { "name": "Ticket", "persistence": "persistent", "collection": "tickets", "fields": [{ "name": "id", "type": "string" }, { "name": "v", "type": "string", "default": "" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Ticket", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "MAKE", "effects": [["persist", "create", "Ticket", { "id": "tk1", "v": "a" }, { "emit": { "success": "MADE", "failure": "MAKE_FAILED" } }]] },
                    { "from": "idle", "to": "idle", "event": "DROP", "effects": [["persist", "delete", "Ticket", "tk1", { "emit": { "success": "DROPPED", "failure": "DROP_FAILED" } }]] },
                    { "from": "idle", "to": "idle", "event": "MADE" }, { "from": "idle", "to": "idle", "event": "MAKE_FAILED" },
                    { "from": "idle", "to": "idle", "event": "DROPPED" }, { "from": "idle", "to": "idle", "event": "DROP_FAILED" }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "P", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let both = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req("MAKE"), req("DROP")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            both[1]["result"]["emitted"][0]["type"], "DROPPED",
            "{}",
            both[1]
        );
        let alone = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("DROP")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            alone[0]["result"]["emitted"][0]["type"], "DROP_FAILED",
            "control: no row, not found"
        );
    }

    #[test]
    fn step_reports_the_evaluated_emit_payload() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EmApp", "orbitals": [{ "name": "Em",
            "entity": { "name": "Req", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Req", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [{ "from": "idle", "to": "idle", "event": "GO", "effects": [
                    ["let", [["big", [">", "@payload.n", 10]]],
                        ["emit", "RESPOND", { "status": ["if", "@big", 413, 200], "lit": 7 }]]] }] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Em", "trait": "T", "from": "idle", "event": "GO", "payload": { "n": 3 } }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(r["emitted"][0]["payload"]["status"], 200.0, "{r}");
        assert_eq!(r["emitted"][0]["payload"]["lit"], 7.0, "control: a literal");
        assert_eq!(r["outcomes"][0]["emitted"], r["emitted"]);
    }

    #[test]
    fn step_overrides_a_config_knob_for_the_call() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "KnobApp", "orbitals": [{ "name": "Knob",
            "entity": { "name": "K", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "T", "linkedEntity": "K",
                  "config": { "mode": { "type": "string", "default": "off" } },
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "on" }],
                    "transitions": [
                        { "from": "idle", "to": "on", "event": "GO", "guard": ["=", "@config.mode", "on"], "effects": [["emit", "WENT", {}]] }
                    ] } },
                { "name": "L", "linkedEntity": "K",
                  "config": { "mode": { "type": "string", "default": "off" } },
                  "listens": [{ "event": "WENT", "triggers": "HEARD", "source": { "kind": "trait", "trait": "T" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "heard" }],
                    "transitions": [{ "from": "idle", "to": "heard", "event": "HEARD", "guard": ["=", "@config.mode", "off"] }] } }
            ] }] }))
        .unwrap();
        let req = |config: Option<serde_json::Value>| {
            let mut r = serde_json::json!({ "orbital": "Knob", "trait": "T", "from": "idle", "event": "GO", "payload": {} });
            if let Some(c) = config {
                r["config"] = c;
            }
            r
        };
        let on = || Some(serde_json::json!({ "mode": "on" }));
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req(on()), req(None)])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "the overridden knob admits the guard: {}",
            played[0]
        );
        assert_eq!(
            played[0]["result"]["outcomes"][1]["fired"], true,
            "the override is the requested trait's only, a listener keeps its default: {}",
            played[0]
        );
        let default = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req(None)])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            default[0]["result"]["guard"], "fail",
            "control: a call that overrides nothing reads the default"
        );
        let split = host
            .run(
                "program/step",
                &[
                    program.clone(),
                    from_json(serde_json::json!([
                        req(on()),
                        req(Some(serde_json::json!({ "mode": "off" })))
                    ]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            split.message.contains("one call takes one value per knob"),
            "{}",
            split.message
        );
        let unknown = host
            .run(
                "program/step",
                &[
                    program.clone(),
                    from_json(serde_json::json!([req(Some(
                        serde_json::json!({ "nope": 1 })
                    ))]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            unknown.message.contains("declares no knob `nope`"),
            "{}",
            unknown.message
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    from_json(serde_json::json!([req(Some(serde_json::json!("on")))])).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("config must be an object"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn step_runs_an_os_effect_only_when_the_call_is_live() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "LiveApp",
            "osGrants": { "fs": [], "exec": [], "net": [], "env": ["HOME"], "stdio": [], "sys": [], "desktop": [] },
            "orbitals": [{ "name": "Live",
            "entity": { "name": "L", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "L", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "asking" }, { "name": "got" }],
                "transitions": [
                    { "from": "idle", "to": "asking", "event": "ASK", "effects": [
                        ["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                    { "from": "asking", "to": "got", "event": "GOT" },
                    { "from": "asking", "to": "idle", "event": "NOGO" }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Live", "trait": "T", "from": "idle", "event": "ASK", "payload": {} }])).unwrap();
        let live = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        requests.clone(),
                        from_json(serde_json::json!({ "live": true })).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        let events: Vec<&str> = live[0]["result"]["outcomes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o["event"].as_str())
            .collect();
        assert_eq!(
            events,
            vec!["ASK", "GOT"],
            "live: the host answers getenv and its success route runs: {}",
            live[0]
        );
        let replay = json(
            &host
                .run("program/step", &[program.clone(), requests.clone()])
                .expect("steps"),
        );
        assert_eq!(
            replay[0]["result"]["state"]["after"], "asking",
            "control: without live the ask stays pending"
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    requests,
                    from_json(serde_json::json!({ "live": "yes" })).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("options.live must be a boolean"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn step_live_waits_for_open_calls_unless_settle_ms_cuts_them_off() {
        let host = ProgramHost;
        let program = |seconds: &str| {
            from_json(serde_json::json!({ "name": "SleepApp",
                "osGrants": { "fs": [], "exec": ["sleep"], "net": [], "env": [], "stdio": [], "sys": [], "desktop": [] },
                "orbitals": [{ "name": "Sleep",
                "entity": { "name": "S", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "done", "type": "boolean", "default": false }] },
                "pages": [],
                "traits": [{ "name": "T", "linkedEntity": "S", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "running" }, { "name": "done" }],
                    "transitions": [
                        { "from": "idle", "to": "running", "event": "RUN", "effects": [
                            ["os/spawn", "sleep", [seconds], { "cwd": null, "env": null, "stdin": "null", "stdout": "null", "stderr": "null", "detached": false },
                                { "key": "spawn", "emit": { "success": "SPAWNED", "failure": "FAILED" } }]] },
                        { "from": "running", "to": "running", "event": "SPAWNED", "effects": [
                            ["os/wait", "@payload.pid", { "key": "wait", "emit": { "success": "WAITED", "failure": "FAILED" } }]] },
                        { "from": "running", "to": "done", "event": "WAITED", "effects": [["set", "@entity.done", true]] },
                        { "from": "running", "to": "idle", "event": "FAILED" }
                    ] } }] }] }))
            .unwrap()
        };
        let run = |extra: serde_json::Value| {
            let mut r = serde_json::json!({ "orbital": "Sleep", "trait": "T", "from": "idle", "event": "RUN", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            from_json(serde_json::json!([r])).unwrap()
        };
        let live = from_json(serde_json::json!({ "live": true })).unwrap();
        let started = std::time::Instant::now();
        let cut = json(
            &host
                .run(
                    "program/step",
                    &[
                        program("5"),
                        run(serde_json::json!({ "settleMs": 300 })),
                        live.clone(),
                    ],
                )
                .expect("steps"),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "settleMs cuts the open wait off"
        );
        assert_ne!(
            cut[0]["result"]["entityAfter"]["done"], true,
            "the cut-off call never answered: {}",
            cut[0]
        );
        let waited = json(
            &host
                .run(
                    "program/step",
                    &[program("0.2"), run(serde_json::json!({})), live.clone()],
                )
                .expect("steps"),
        );
        assert_eq!(
            waited[0]["result"]["entityAfter"]["done"], true,
            "control: without settleMs the step waits for the process: {}",
            waited[0]
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program("0"),
                    run(serde_json::json!({ "settleMs": "soon" })),
                    live,
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("settleMs must be a number"),
            "{}",
            bad.message
        );
        let replay = host
            .run(
                "program/step",
                &[program("0"), run(serde_json::json!({ "settleMs": 10 }))],
            )
            .unwrap_err();
        assert!(
            replay
                .message
                .contains("settleMs` applies to a live call only"),
            "control: no cutoff to set without live: {}",
            replay.message
        );
    }

    #[test]
    fn step_dispatches_as_the_request_viewer() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "SeeApp", "orbitals": [{ "name": "See",
            "entity": { "name": "V", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "V", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "WHO", "effects": [["emit", "SEEN", { "id": "@user.id" }]] },
                    { "from": "idle", "to": "idle", "event": "MINE", "guard": ["=", "@user.id", "u1"] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str, viewer: Option<serde_json::Value>| {
            let mut r = serde_json::json!({ "orbital": "See", "trait": "T", "from": "idle", "event": event, "payload": {} });
            if let Some(v) = viewer {
                r["viewer"] = v;
            }
            r
        };
        let u1 = serde_json::json!({ "id": "u1", "email": "u1@example.com" });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([
                            req("MINE", Some(u1.clone())),
                            req("MINE", None),
                            req("WHO", Some(u1)),
                            req("WHO", Some(serde_json::Value::Null))
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "the viewer is @user: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "fail",
            "control: without a viewer @user is nobody: {}",
            played[1]
        );
        assert_eq!(played[2]["result"]["emitted"][0]["payload"]["id"], "u1");
        assert!(
            played[3]["result"]["emitted"][0]["payload"]
                .get("id")
                .is_none_or(serde_json::Value::is_null),
            "control: a null viewer is nobody: {}",
            played[3]
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    from_json(serde_json::json!([req(
                        "WHO",
                        Some(serde_json::json!("u1"))
                    )]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("viewer must be a user object or null"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn step_reports_an_os_effect_without_running_or_failing_it() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "OsApp",
            "osGrants": { "env": ["HOME"] },
            "orbitals": [{ "name": "Os",
            "entity": { "name": "O", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "O", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "asking" }],
                "transitions": [
                    { "from": "idle", "to": "asking", "event": "ASK", "effects": [
                        ["set", "@entity.n", 1],
                        ["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                    { "from": "asking", "to": "idle", "event": "GOT" },
                    { "from": "asking", "to": "idle", "event": "NOGO" }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Os", "trait": "T", "from": "idle", "event": "ASK", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        let os = r["effects"]
            .as_array()
            .and_then(|es| es.iter().find(|e| e["type"] == "os/getenv"))
            .cloned();
        assert!(
            os.as_ref().is_some_and(|e| e["status"] != "failed"),
            "the os ask is reported, not failed: {r}"
        );
        assert_eq!(
            r["effects"][0]["status"], "executed",
            "control: the arm's other effect runs"
        );
        assert_eq!(
            r["state"]["after"], "asking",
            "no answer event re-enters the trait: {r}"
        );
        assert_eq!(r["outcomes"].as_array().map(Vec::len), Some(1), "{r}");
    }

    #[test]
    fn step_reports_the_row_the_requested_trait_holds_after_it() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RowApp", "orbitals": [{ "name": "Row",
            "entity": { "name": "R", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "R", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "BUMP", "effects": [["set", "@entity.n", ["+", "@entity.n", 5]]] },
                    { "from": "idle", "to": "idle", "event": "NOPE", "guard": ["=", 1, 2], "effects": [["set", "@entity.n", 99]] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Row", "trait": "T", "from": "idle", "event": event, "payload": {}, "entity": { "n": 1 } });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("BUMP"), req("NOPE")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["entityAfter"]["n"], 6.0,
            "the set shows in the row: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["entityAfter"]["n"], 1.0,
            "control: a request that fires nothing leaves the seeded row: {}",
            played[1]
        );
    }

    #[test]
    fn step_never_reports_an_emit_whose_payload_failed_to_evaluate() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EmApp", "orbitals": [{ "name": "Em",
            "entity": { "name": "Req", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "items", "type": "array", "default": [], "items": { "type": "object", "properties": { "id": { "name": "id", "type": "number" } } } }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Req", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "GO", "effects": [
                        ["emit", "BAD", { "list": ["array/append", ["object/get", ["array/find", "@entity.items", ["fn", "p", ["=", "@p.id", 9]]], "list"], 1] }]] },
                    { "from": "idle", "to": "idle", "event": "LIT", "effects": [["emit", "GOOD", { "lit": 7 }]] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Em", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("GO"), req("LIT")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(
            r["emitted"],
            serde_json::json!([]),
            "a failed emit is no emit: {r}"
        );
        assert_eq!(r["effects"][0]["status"], "failed", "{r}");
        assert_eq!(
            played[1]["result"]["emitted"][0]["payload"]["lit"], 7.0,
            "control: a literal emit is reported"
        );
    }

    #[test]
    fn control_a_step_entity_that_is_not_an_object_or_a_now_that_is_not_a_number_is_refused() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "GateApp", "orbitals": [{ "name": "Gating",
            "entity": { "name": "Gate", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "G", "linkedEntity": "Gate", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }],
                "transitions": [{ "from": "ready", "to": "ready", "event": "GO" }] } }] }] }))
        .unwrap();
        for (bad, says) in [
            (
                serde_json::json!({ "entity": "ann" }),
                "entity must be an object",
            ),
            (serde_json::json!({ "now": "noon" }), "now must be a number"),
        ] {
            let mut r = serde_json::json!({ "orbital": "Gating", "trait": "G", "from": "ready", "event": "GO", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(bad.as_object().unwrap().clone());
            let err = host
                .run(
                    "program/step",
                    &[program.clone(), from_json(serde_json::json!([r])).unwrap()],
                )
                .unwrap_err();
            assert!(err.message.contains(says), "{}", err.message);
        }
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
