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
    SubstrateFailure { message: message.into(), errors: Vec::new() }
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
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn program_arg(op: &str, value: Option<&Value>) -> Result<serde_json::Value, SubstrateFailure> {
    let json = value.map(to_json).unwrap_or(serde_json::Value::Null);
    match &json {
        serde_json::Value::Object(map) if map.get("name").is_some_and(serde_json::Value::is_string) && map.contains_key("orbitals") => Ok(json),
        _ => Err(failure(format!("{op}: not a program"))),
    }
}

/// The issues of a validation report, as the `{ code, message, path?, line?, column? }` findings both hosts answer.
fn issues(report: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    report.get(key).and_then(serde_json::Value::as_array).cloned().unwrap_or_default()
}

impl ProgramHost {
    fn read(&self, text: &str) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let path = scratch.write("program.lolo", text)?;
        match crate::emit::orb_json(text, &path.display().to_string()) {
            Ok(json) => from_json(serde_json::from_str(&json).map_err(|e| failure(e.to_string()))?),
            Err(lowering) => {
                let report = crate::commands::validate::validation_report(&path, None, None, true, false)
                    .map_err(|e| failure(format!("program/read: {e}")))?;
                let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
                let found: Vec<serde_json::Value> = issues(&report, "errors").into_iter().chain(issues(&report, "warnings")).collect();
                if found.is_empty() {
                    return Err(failure(format!("program/read: {lowering}")));
                }
                let messages: Vec<&str> = found.iter().filter_map(|i| i.get("message").and_then(serde_json::Value::as_str)).collect();
                Err(SubstrateFailure {
                    message: format!("program/read: {} validation issue(s): {}", found.len(), messages.join("; ")),
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

    fn validate(&self, program: serde_json::Value, options: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let mut final_program = false;
        let mut prior: Option<serde_json::Value> = None;
        match options.map(to_json) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("final", serde_json::Value::Bool(b)) => final_program = b,
                        ("final", _) => return Err(failure("program/validate: final must be a boolean")),
                        ("prior", p) => prior = Some(program_arg("program/validate prior", from_json(p).ok().as_ref())?),
                        (other, _) => return Err(failure(format!("program/validate: unsupported option \"{other}\""))),
                    }
                }
            }
            Some(_) => return Err(failure("program/validate: options must be an object")),
        }
        let name = program.get("name").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
        let issue = |code: &str, message: String| serde_json::json!({ "code": code, "message": message });
        if !program_name_ok(&name) {
            let report = serde_json::json!({ "valid": false, "errors": [issue("PROGRAM_NAME_INVALID", format!("program name \"{name}\" is not a file name"))], "warnings": [] });
            return from_json(report);
        }
        let scratch = Scratch::new("almadar-validate")?;
        let input = scratch.write(&format!("{name}.orb"), &program.to_string())?;
        let prior_path = prior.map(|p| scratch.write("prior.orb", &p.to_string())).transpose()?;
        let stamped = match crate::commands::stamp::stamped_json(&input, prior_path.as_deref(), None) {
            Ok(json) => scratch.write(&format!("{name}.stamped.orb"), &json)?,
            Err(e) => {
                let report = serde_json::json!({ "valid": false, "errors": [issue("PROGRAM_STAMP_FAILED", e.to_string())], "warnings": [] });
                return from_json(report);
            }
        };
        let report = crate::commands::validate::validation_report(&stamped, None, None, true, final_program)
            .map_err(|e| failure(format!("program/validate: {e}")))?;
        let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
        let (errors, warnings) = (issues(&report, "errors"), issues(&report, "warnings"));
        let valid = report.get("valid").and_then(serde_json::Value::as_bool).unwrap_or(false) && errors.is_empty() && warnings.is_empty();
        from_json(serde_json::json!({ "valid": valid, "errors": errors, "warnings": warnings }))
    }

    fn describe_target(op: &str, value: Option<&Value>) -> Result<crate::commands::behaviors_catalog::DescribeTarget, SubstrateFailure> {
        use crate::commands::behaviors_catalog::DescribeTarget;
        let json = value.map(to_json).unwrap_or(serde_json::Value::Null);
        let behavior = json.get("behavior").and_then(serde_json::Value::as_str).map(str::to_string);
        match (behavior, json.get("trait"), json.get("orbital").and_then(serde_json::Value::as_str)) {
            (Some(_), Some(_), _) => Ok(DescribeTarget::Value(json)),
            (Some(behavior), None, Some(orbital)) => Ok(DescribeTarget::Spec(format!("{behavior}.orbitals.{orbital}"))),
            (Some(behavior), None, None) => Ok(DescribeTarget::Spec(behavior)),
            (None, _, _) => Err(failure(format!("{op}: expected {{ behavior }}, an orbital value or a trait value"))),
        }
    }

    fn behavior_of(op: &str, value: Option<&Value>) -> Result<String, SubstrateFailure> {
        value
            .map(to_json)
            .and_then(|v| v.get("behavior").and_then(serde_json::Value::as_str).map(str::to_string))
            .ok_or_else(|| failure(format!("{op}: expected a value naming its behavior")))
    }

    fn catalog(&self, scope: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let json = scope.map(to_json).unwrap_or(serde_json::Value::Null);
        let strings = |key: &str| -> Option<Vec<String>> {
            json.get(key).and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        };
        let Some(paths) = strings("paths") else {
            return Err(failure("behavior/catalog: paths: expected a list of registry folders"));
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
            other => Err(failure(format!(
                "{other}: not answered by the native program host yet (program/compose, program/eval, program/step and behavior/dependencies run on the JS host; G-CROSS-168 W2)"
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
        let program = host.run("program/read", &[Value::String(NOTE.to_string())]).expect("reads");
        let as_json = json(&program);
        assert_eq!(as_json["name"], "note-pad");
        let expected: serde_json::Value = serde_json::from_str(&crate::emit::orb_json(NOTE, "note.lolo").unwrap()).unwrap();
        assert_eq!(as_json, json(&from_json(expected).unwrap()), "the value `orb emit orb` prints, as a kernel value");
        let printed = host.run("program/print", &[program]).expect("prints");
        let Value::String(text) = printed else { panic!("text") };
        assert!(text.contains("note-pad") && text.contains("NoteView"), "{text}");
    }

    #[test]
    fn validate_reports_the_validators_findings_and_a_bad_name_before_any_run() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let program = host.run("program/read", &[Value::String(NOTE.to_string())]).unwrap();
        let report = json(&host.run("program/validate", &[program.clone()]).expect("validates"));
        assert!(report["errors"].is_array() && report["warnings"].is_array(), "{report}");
        let mut renamed = json(&program);
        renamed["name"] = serde_json::json!("not a file name");
        let refused = json(&host.run("program/validate", &[from_json(renamed).unwrap()]).unwrap());
        assert_eq!(refused["valid"], false);
        assert_eq!(refused["errors"][0]["code"], "PROGRAM_NAME_INVALID");
    }

    #[test]
    fn the_behavior_ops_answer_what_the_behaviors_commands_print() {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let host = ProgramHost;
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let source = json(&host.run("behavior/source", &[value.clone()]).expect("source"));
        let printed = crate::commands::behaviors_catalog::source("almadar-std/std-retry").unwrap();
        assert_eq!(source, json(&from_json(printed).unwrap()));
        let described = json(&host.run("behavior/describe", &[value.clone()]).expect("describe"));
        assert_eq!(described["behavior"], "almadar-std/std-retry");
        let resolved = json(&host.run("behavior/resolve", &[value]).expect("resolve"));
        assert_eq!(resolved["name"], "std-retry");
        let missing = from_json(serde_json::json!({ "behavior": "almadar-std/no-such-behavior" })).unwrap();
        assert!(host.run("behavior/source", &[missing]).is_err(), "control: an unknown behavior fails");
    }

    #[test]
    fn control_a_bad_argument_and_an_op_not_hosted_here_fail_with_a_message() {
        let host = ProgramHost;
        assert!(host.run("program/read", &[Value::Number(1.0)]).unwrap_err().message.contains("expected .lolo text"));
        assert!(host.run("program/print", &[Value::Null]).unwrap_err().message.contains("not a program"));
        assert!(host.run("program/compose", &[]).unwrap_err().message.contains("G-CROSS-168"));
    }
}
