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

pub(super) fn failure(message: impl Into<String>) -> SubstrateFailure {
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

/// The `program/read` modes `options` turns on, in the order they are documented.
fn read_modes(options: Option<&Value>) -> Vec<&'static str> {
    ["stamp", "inspect", "embeds", "resolve", "references"].into_iter().filter(|mode| option_flag(options, mode) == Some(true)).collect()
}

/// A boolean option of an op's options argument; absent or not a boolean is `None`.
fn option_flag(options: Option<&Value>, key: &str) -> Option<bool> {
    options
        .map(to_json)
        .and_then(|o| o.get(key).and_then(serde_json::Value::as_bool))
}

/// An op's options object: typed reads by key, and a refusal of any key the op's form does not take.
struct Options<'a> {
    op: &'a str,
    map: &'a serde_json::Map<String, serde_json::Value>,
}

impl<'a> Options<'a> {
    fn of(op: &'a str, value: &'a serde_json::Value) -> Result<Self, SubstrateFailure> {
        value
            .as_object()
            .map(|map| Self { op, map })
            .ok_or_else(|| failure(format!("{op}: options must be an object")))
    }

    fn only(&self, form: &str, keys: &[&str]) -> Result<(), SubstrateFailure> {
        match self.map.keys().find(|k| !keys.contains(&k.as_str())) {
            Some(other) => Err(failure(format!("{}: unsupported option \"{other}\" with `{form}`", self.op))),
            None => Ok(()),
        }
    }

    fn text(&self, key: &str) -> Result<Option<String>, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(failure(format!("{}: `{key}` must be text", self.op))),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(failure(format!("{}: `{key}` must be a boolean", self.op))),
        }
    }

    fn number(&self, key: &str) -> Result<Option<u64>, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| failure(format!("{}: `{key}` must be a whole number", self.op))),
        }
    }

    fn path(&self, key: &str) -> Result<Option<PathBuf>, SubstrateFailure> {
        Ok(self.text(key)?.map(PathBuf::from))
    }

    fn absolute(&self, key: &str, form: &str) -> Result<PathBuf, SubstrateFailure> {
        self.path(key)?
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure(format!("{}: `{form}` needs the program's absolute `{key}`", self.op)))
    }

    fn choice<T: clap::ValueEnum>(&self, key: &str) -> Result<Option<T>, SubstrateFailure> {
        self.text(key)?
            .map(|name| {
                T::from_str(&name, false)
                    .map_err(|_| failure(format!("{}: `{key}` \"{name}\" is not one of the accepted values", self.op)))
            })
            .transpose()
    }
}

fn has_option(options: Option<&Value>, key: &str) -> bool {
    options
        .map(to_json)
        .is_some_and(|o| o.get(key).is_some_and(|v| !v.is_null()))
}

/// `program/print`'s `{ source, node | nodes }` options: format authored `.lolo` text (comments
/// kept, as `orb format`) instead of the quoted program, whole or at a node path.
struct PrintSource {
    source: String,
    node: Option<String>,
    nodes: Option<String>,
}

impl PrintSource {
    fn of(options: Option<&Value>) -> Result<Option<Self>, SubstrateFailure> {
        let Some(options) = options.map(to_json) else { return Ok(None) };
        let text = |key: &str| -> Result<Option<String>, SubstrateFailure> {
            match options.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(failure(format!("program/print: `{key}` must be text"))),
            }
        };
        let (node, nodes) = (text("node")?, text("nodes")?);
        let Some(source) = text("source")? else {
            return match node.or(nodes) {
                Some(_) => Err(failure("program/print: `node` and `nodes` read the authored `source`, which is missing")),
                None => Ok(None),
            };
        };
        if node.is_some() && nodes.is_some() {
            return Err(failure("program/print: give `node` or `nodes`, not both"));
        }
        Ok(Some(PrintSource { source, node, nodes }))
    }

    fn print(self) -> Result<Value, SubstrateFailure> {
        let at = |path: &str| -> Result<Vec<String>, SubstrateFailure> {
            let program = orbital_lolo::parse_str(&self.source).map_err(|e| failure(format!("program/print: parse error: {e:?}")))?;
            orbital_lolo::format_path(&program, path).map_err(|e| failure(format!("program/print: {e}")))
        };
        match (&self.node, &self.nodes) {
            (Some(path), _) if path.contains("[*]") => Err(failure("program/print: `node` names one construct; use `nodes` for a `[*]` path")),
            (Some(path), _) => Ok(Value::string(at(path)?.remove(0))),
            (None, Some(path)) => from_json(serde_json::Value::from(at(path)?)),
            (None, None) => crate::commands::parse::formatted_text(&self.source, "source")
                .map(Value::string)
                .map_err(|e| failure(format!("program/print: {e}"))),
        }
    }
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
        crate::commands::validate::validation_report(&stamped, None, None, true, final_program, &[])
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
/// The shell's prebuilt web client copied to `<output>/web-client` with the program's runtime files (the client-mode
/// `packages/client/public` and `locales/`) laid over it: the folder `orb serve` serves.
fn copy_web_client(built: Option<&Path>, output: &Path) -> Result<PathBuf, SubstrateFailure> {
    let built = built.ok_or_else(|| {
        failure("program/eval: the typescript shell ships no prebuilt web client: run `orb update` (in a shell checkout, build it with `node tooling/scripts/build-web-client.mjs`)")
    })?;
    if !built.join("index.html").is_file() {
        return Err(failure(format!("program/eval: {} holds no index.html: it is not a built web client", built.display())));
    }
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let public = output.join("packages/client/public");
    if !public.join("client.json").is_file() {
        return Err(failure("program/eval: the client-mode generate wrote no client.json for the web client to read: generate with `mode: client`"));
    }
    let target = output.join("web-client");
    let copied = copy(built, &target)
        .and_then(|()| copy(&public, &target))
        .and_then(|()| if output.join("locales").is_dir() { copy(&output.join("locales"), &target.join("locales")) } else { Ok(()) });
    copied.map_err(|e| failure(format!("program/eval: laying out the web client in {}: {e}", target.display())))?;
    Ok(target)
}

/// `webClient` is the copied prebuilt web client, null when the generate asked for none.
fn generate_answer(report: &crate::commands::compile::CompileReport, web_client: Option<&Path>) -> serde_json::Value {
    serde_json::json!({
        "lines": report.lines,
        "files": report.files,
        "warnings": report.warnings,
        "nextSteps": report.next_steps,
        "webClient": web_client.map(|dir| dir.display().to_string()),
    })
}

fn issues(report: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    report
        .get(key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

impl ProgramHost {
    /// `program/validate { report: true, path, final?, denyWarnings?, findings? }`: the report `orb validate --json` prints
    /// for the file at the absolute `path` (lowering, types, every validator finding with its source position); each of
    /// `findings` (a std lint's) joins the errors or warnings by its severity, positioned the same way.
    fn validation_report(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let op = "program/validate";
        let path = options
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::Path::new)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure(format!("{op}: `report` needs the program's absolute `path`")))?;
        let flag = |key: &str| match options.get(key) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(failure(format!("{op}: `{key}` must be a boolean"))),
        };
        for key in options.as_object().into_iter().flat_map(|o| o.keys()) {
            if !["report", "path", "final", "denyWarnings", "patterns", "services", "findings"].contains(&key.as_str()) {
                return Err(failure(format!("{op}: unsupported option \"{key}\" with `report`")));
            }
        }
        let file = |key: &str| match options.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(p)) => Ok(Some(std::path::PathBuf::from(p))),
            Some(_) => Err(failure(format!("{op}: `{key}` must be a path"))),
        };
        let (patterns, services) = (file("patterns")?, file("services")?);
        let findings: Vec<crate::commands::validate::ProgramFinding> = match options.get("findings") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(list) => serde_json::from_value(list.clone())
                .map_err(|e| failure(format!("{op}: `findings` must be a list of {{ code, path, message, suggestion, severity: \"error\" | \"warning\" }}: {e}")))?,
        };
        let report = crate::commands::validate::validation_report(
            path,
            patterns.as_deref(),
            services.as_deref(),
            flag("denyWarnings")?,
            flag("final")?,
            &findings,
        )
            .map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(serde_json::to_value(report).map_err(|e| failure(e.to_string()))?)
    }

    /// `program/read { resolve: true, path, output? }`: what `orb resolve <path> [--output]` prints
    /// and writes, as `{ program, text }`; the file at the absolute `path` is read, not `text`.
    /// `text: false` answers `{ program, lazy: [{ orbital, program }] }` and writes nothing, so a program with lazy
    /// behaviors or locales is not refused (the lint input reads programs this way).
    fn read_resolved(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let path = options
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::Path::new)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure("program/read: `resolve` needs the program's absolute `path`"))?;
        let output = match options.get("output") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(out)) => Some(std::path::PathBuf::from(out)),
            Some(_) => return Err(failure("program/read: `output` must be a path")),
        };
        match options.get("text") {
            None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(true)) => {}
            Some(serde_json::Value::Bool(false)) if output.is_some() => {
                return Err(failure("program/read: `text: false` writes nothing, so it takes no `output`"))
            }
            Some(serde_json::Value::Bool(false)) => {
                let answer = crate::commands::parse::resolved_value(path)
                    .map_err(|e| failure(format!("program/read: {e}")))?;
                return from_json(answer);
            }
            Some(_) => return Err(failure("program/read: `text` must be a boolean")),
        }
        let text = crate::commands::parse::resolved_text(path, output.as_deref())
            .map_err(|e| failure(format!("program/read: {e}")))?;
        let program: serde_json::Value = serde_json::from_str(&text).map_err(|e| failure(e.to_string()))?;
        from_json(serde_json::json!({ "program": program, "text": text }))
    }

    /// `program/read { stamp: true, path, prior?, imports?, output? }`: what `orb stamp` prints, as `{ program, text }`;
    /// with `output` the stamped text is written to that file (as `--output`) and printed by nobody.
    fn read_stamped(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let options = Options::of("program/read", options)?;
        options.only("stamp", &["stamp", "path", "prior", "imports", "output"])?;
        let path = options.absolute("path", "stamp")?;
        let prior = options.path("prior")?;
        let imports = options.path("imports")?;
        let output = options.path("output")?;
        let text = crate::commands::stamp::stamped_json(&path, prior.as_deref(), imports.as_deref())
            .map_err(|e| failure(format!("program/read: {e}")))?;
        if let Some(out) = &output {
            std::fs::write(out, &text)
                .map_err(|e| failure(format!("program/read: Failed to write {}: {e}", out.display())))?;
        }
        let program: serde_json::Value = serde_json::from_str(&text).map_err(|e| failure(e.to_string()))?;
        from_json(serde_json::json!({ "program": program, "text": text }))
    }

    /// `program/read { references: true, path }`: `{ program, references }`, the program at the absolute `path` as it
    /// reads before resolution and each trait or orbital reference it resolves through, as
    /// `{ kind, alias, name, path, callSite, declaration }` (the reference as written, and the upstream declaration it resolved to).
    fn read_references(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let options = Options::of("program/read", options)?;
        options.only("references", &["references", "path"])?;
        let path = options.absolute("path", "references")?;
        let (program, references) = crate::commands::parse::referenced_program(&path)
            .map_err(|e| failure(format!("program/read: {e}")))?;
        let references = serde_json::to_value(references).map_err(|e| failure(e.to_string()))?;
        from_json(serde_json::json!({ "program": program, "references": references }))
    }

    /// `program/print { target, path, section?, format?, only?, map?, output?, inspectSource?, documentVersion?, unit?,
    /// out?, signKey?, keyId? }`: what `orb emit <target> <path>` prints, as `{ text, stderr }`; the files `orb emit`
    /// writes (`output`, the source map, `out`) are written.
    fn print_target(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        use crate::emit::{EmitFormat, EmitOptions, EmitTarget};
        let options = Options::of("program/print", options)?;
        options.only(
            "target",
            &[
                "target", "path", "section", "format", "only", "map", "output", "inspectSource", "documentVersion",
                "unit", "out", "signKey", "keyId",
            ],
        )?;
        let target: EmitTarget = options
            .choice("target")?
            .ok_or_else(|| failure("program/print: `target` must be orb, lolo, ts, py or manifest"))?;
        let path = options.absolute("path", "target")?;
        let (sign_key, key_id) = (options.path("signKey")?, options.text("keyId")?);
        if sign_key.is_some() != key_id.is_some() {
            return Err(failure("program/print: `signKey` and `keyId` go together"));
        }
        let inspect_source = options.path("inspectSource")?;
        let document_version = options.number("documentVersion")?;
        if document_version.is_some() && inspect_source.is_none() {
            return Err(failure("program/print: `documentVersion` needs `inspectSource`"));
        }
        let rendered = crate::emit::render(EmitOptions {
            target,
            path: Some(path),
            section: options.text("section")?,
            output: options.path("output")?,
            format: options.choice::<EmitFormat>("format")?,
            only: options.text("only")?,
            map: options.flag("map")?,
            inspect_source,
            document_version,
            unit: options.text("unit")?,
            out: options.path("out")?,
            sign_key,
            key_id,
        })
        .map_err(|e| failure(format!("program/print: {e}")))?;
        from_json(serde_json::json!({ "text": rendered.stdout, "stderr": rendered.stderr }))
    }

    /// `program/eval { generate: { path, output, shell, mode?, frontend?, backend?, backendUrl?, patterns?, devLogs?,
    /// host?, listenHost?, port?, emitOrb?, webClient?, source? } }`: what `orb compile` does and prints, as `{ lines, files,
    /// warnings, nextSteps, webClient }`; `webClient: true` also copies the shell's prebuilt web client into the output. The program is validated before anything is generated, as the command does.
    fn eval_generate(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        use crate::commands::compile::{compile_run, CompileRequest};
        let op = "program/eval";
        Options::of(op, options)?.only("generate", &["generate"])?;
        let generate = options.get("generate").unwrap_or(&serde_json::Value::Null);
        let g = Options::of(op, generate)?;
        g.only(
            "generate",
            &[
                "path", "output", "shell", "mode", "frontend", "backend", "backendUrl", "patterns", "devLogs", "host",
                "emitOrb", "webClient", "source",
            ],
        )?;
        let web_client = g.flag("webClient")?;
        let source = match g.path("source")? {
            Some(source) if !source.is_absolute() => {
                return Err(failure(format!("{op}: `generate` needs an absolute `source`, not {}", source.display())))
            }
            source => source,
        };
        let path = g.absolute("path", "generate")?;
        let output = g.absolute("output", "generate")?;
        let shell: crate::ShellType = g
            .choice("shell")?
            .ok_or_else(|| failure(format!("{op}: `generate` needs the `shell` to generate for")))?;
        let (patterns, emit_orb) = (g.path("patterns")?, g.path("emitOrb")?);
        let report = compile_run(CompileRequest {
            path: &path,
            output: &output,
            shell,
            mode: g.choice("mode")?.unwrap_or_default(),
            frontend: g.choice("frontend")?,
            backend: g.choice("backend")?,
            backend_url: g.text("backendUrl")?,
            patterns_path: patterns.as_deref(),
            dev_logs: g.flag("devLogs")?,
            host: g.choice("host")?.unwrap_or_default(),
            emit_orb: emit_orb.as_deref(),
            source: source.as_deref(),
        })
        .map_err(|e| failure(format!("{op}: {e}")))?;
        let web_client = if web_client {
            let shell_program = crate::utils::shell_programs::locate(shell.program()).map_err(|e| failure(format!("{op}: {e}")))?;
            Some(copy_web_client(shell_program.description.web_client.as_deref(), &output)?)
        } else {
            None
        };
        from_json(generate_answer(&report, web_client.as_deref()))
    }

    fn read(&self, text: &str) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let path = scratch.write("program.lolo", text)?;
        match crate::emit::orb_value(text, &path.display().to_string()) {
            Ok(json) => from_json(json),
            Err(lowering) => {
                let report =
                    crate::commands::validate::validation_report(&path, None, None, true, false, &[])
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
            .map(Value::string)
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
        let started = std::time::Instant::now();
        let entries = catalog_with(&paths, &exposure, vectors, user_dir)
            .map_err(|e| failure(format!("behavior/catalog: {e}")))?;
        let listed = started.elapsed().as_millis() as u64;
        let answer = from_json(serde_json::to_value(entries).map_err(|e| failure(e.to_string()))?);
        tracing::debug!(target: orbital_core::logging::ns::CLI, listed_ms = listed, total_ms = started.elapsed().as_millis() as u64, "behavior/catalog answered");
        answer
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
        for (i, pkg) in packages.iter().enumerate() {
            let vectors = crate::commands::sidecar_fetch::resolve_vectors(
                pkg,
                &crate::commands::sidecar_fetch::UreqFetcher,
            )
            .map_err(|e| failure(format!("behavior/packages: {e}")))?;
            let fingerprint = crate::commands::behaviors_index::fingerprint(&packages, i)
                .map_err(|e| failure(format!("behavior/packages: {e}")))?;
            out.push(serde_json::json!({
                "root": pkg.root.to_string_lossy(),
                "prefix": pkg.prefix,
                "vectors": vectors.map(|v| v.to_string_lossy().into_owned()),
                "fingerprint": fingerprint,
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
        let mut program: serde_json::Value = serde_json::from_str(&text).map_err(|e| failure(e.to_string()))?;
        with_client_metadata(&mut program);
        from_json(program)
    }
}

/// A resolved program is mounted by a browser kernel, which has no installed core: it carries the client metadata
/// (`orbital_core::effects::runs_on::client_program_metadata`) beside whatever `_metadata` it already has.
fn with_client_metadata(program: &mut serde_json::Value) {
    let metadata = orbital_core::effects::runs_on::client_program_metadata(program.get("orbitals").unwrap_or(&serde_json::Value::Null));
    let Some(root) = program.as_object_mut() else {
        return;
    };
    let slot = root.entry("_metadata").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(existing) = slot.as_object_mut() {
        existing.extend(metadata);
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
        let raw = match options.map(to_json).as_ref().and_then(|o| o.get("report")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::String(r)) if r == "raw" => true,
            Some(other) => return Err(failure(format!("{op}: options.report must be \"raw\", not {other}"))),
        };
        // A raw request is an `orb execute --steps` step: the kernel dispatch from the held state, no `from` to check.
        let required: &[&str] = if raw { &["trait", "event"] } else { &["orbital", "trait", "from", "event"] };
        for r in &requests {
            for key in required.iter().copied() {
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
                let mut step = serde_json::json!({ "trait": r["trait"], "event": r["event"], "payload": r.get("payload").cloned().unwrap_or(serde_json::json!({})) });
                if let Some(from) = r.get("from") {
                    step["from"] = from.clone();
                }
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
        if raw {
            return from_json(report);
        }
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
                    |s, _| orbital_core::client::render_probe::probe_kernel(s),
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
                Some(Value::String(_)) if read_modes(args.get(1)).len() > 1 => Err(failure(format!(
                    "program/read: unsupported option \"{}\" with `{}`: one of inspect, embeds, resolve, stamp, references per call",
                    read_modes(args.get(1))[1],
                    read_modes(args.get(1))[0]
                ))),
                Some(Value::String(text)) if option_flag(args.get(1), "inspect") == Some(true) => {
                    let options = to_json(&args[1]);
                    super::program_inspect::inspect(text, &options).and_then(from_json)
                }
                Some(Value::String(text)) if option_flag(args.get(1), "embeds") == Some(true) => {
                    let embeds = orbital_lolo::embeds::inline_embeds(text)
                        .map_err(|e| failure(format!("program/read: {e}")))?;
                    from_json(serde_json::to_value(embeds).map_err(|e| failure(e.to_string()))?)
                }
                Some(Value::String(_)) if option_flag(args.get(1), "resolve") == Some(true) => {
                    Self::read_resolved(&to_json(&args[1]))
                }
                Some(Value::String(_)) if option_flag(args.get(1), "references") == Some(true) => {
                    Self::read_references(&to_json(&args[1]))
                }
                Some(Value::String(_)) if option_flag(args.get(1), "stamp") == Some(true) => {
                    Self::read_stamped(&to_json(&args[1]))
                }
                Some(Value::String(text)) => self.read(text),
                _ => Err(failure("program/read: expected .lolo text")),
            },
            "program/print" if has_option(args.get(1), "target") => Self::print_target(&to_json(&args[1])),
            "program/print" => match PrintSource::of(args.get(1))? {
                Some(source) => source.print(),
                None => self.print(program_arg(op, args.first())?),
            },
            "program/validate" if option_flag(args.get(1), "report") == Some(true) => Self::validation_report(&to_json(&args[1])),
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
            "behavior/resolve" if option_flag(args.get(1), "expects") == Some(true) => {
                let spec = Self::behavior_of(op, args.first())?;
                let derived = crate::commands::behaviors_catalog::expectations(&spec).map_err(|e| failure(format!("{op}: {e}")))?;
                from_json(derived)
            }
            "behavior/resolve" if option_flag(args.get(1), "path") == Some(true) => {
                let spec = Self::behavior_of(op, args.first())?;
                match crate::commands::behaviors::which(&spec).map_err(|e| failure(format!("{op}: {e}")))? {
                    Ok((path, owner)) => from_json(serde_json::json!({ "path": path.display().to_string(), "owner": owner })),
                    Err(refusal) => Err(failure(format!("{op}: {refusal}"))),
                }
            }
            "behavior/resolve" => self.resolve_behavior(&Self::behavior_of(op, args.first())?),
            "program/compose" => self.compose(args),
            "program/eval" if has_option(args.get(1), "generate") => Self::eval_generate(&to_json(&args[1])),
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

  trait NoteView -> Note {
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
    fn read_keeps_a_record_literals_author_key_order() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let probe = r#"app order-probe [ version: "1.0.0" ]
orbital O {
  entity E [runtime] {
    id : string!
  }
  trait T -> E {
    initial: a
    state a {
      INIT -> a
        (log (json/stringify { zeta: 1, alpha: 2, mid: 3 }))
    }
  }
}
"#;
        let read = |text: &str| serde_json::to_string(&json(&ProgramHost.run("program/read", &[Value::String(text.to_string().into())]).expect("reads"))).unwrap();
        let text = read(probe);
        let (z, a, m) = (text.find("\"zeta\"").unwrap(), text.find("\"alpha\"").unwrap(), text.find("\"mid\"").unwrap());
        assert!(z < a && a < m, "author order zeta, alpha, mid: {text}");
        let control = read(&probe.replace("{ zeta: 1, alpha: 2, mid: 3 }", "{ alpha: 2, mid: 3, zeta: 1 }"));
        let (z, a, m) = (control.find("\"zeta\"").unwrap(), control.find("\"alpha\"").unwrap(), control.find("\"mid\"").unwrap());
        assert!(a < m && m < z, "control: the other author order is kept too: {control}");
    }

    #[test]
    fn read_lowers_text_to_the_program_emit_orb_prints_and_print_writes_it_back() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
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
    fn read_with_embeds_lists_each_inline_embed_and_control_none_without_one() {
        let source = "app a [version: \"1.0.0\"]\norbital O {\n  uses Button from \"std/behaviors/std-button\"\n  entity E [runtime] {\n    id : string!\n  }\n  trait V -> E {\n    initial: idle\n    state idle {\n      INIT -> idle\n        (render-ui main <Button.traits.ButtonRender label={\"Go\"} />)\n    }\n  }\n}\n";
        let options = from_json(serde_json::json!({ "embeds": true })).unwrap();
        let embeds = json(&ProgramHost.run("program/read", &[Value::string(source), options.clone()]).expect("reads"));
        assert_eq!(embeds.as_array().map(Vec::len), Some(1), "{embeds}");
        assert_eq!(embeds[0]["behavior"], "Button.traits.ButtonRender");
        let none = json(&ProgramHost.run("program/read", &[Value::string(NOTE), options]).expect("reads"));
        assert_eq!(none, serde_json::json!([]));
    }

    #[test]
    fn read_with_resolve_answers_what_orb_resolve_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "resolve": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"));
        let expected = crate::commands::parse::resolved_text(&file, None).expect("orb resolve");
        assert_eq!(answer["text"], serde_json::Value::String(expected.clone()));
        assert_eq!(answer["program"], serde_json::from_str::<serde_json::Value>(&expected).unwrap());
    }

    #[test]
    fn read_with_resolve_and_no_text_answers_a_program_with_locales() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE.replacen("app note-pad [", "app note-pad [\n  locales: [en, ar],", 1)).unwrap();
        let read = |extra: serde_json::Value| {
            let mut options = serde_json::json!({ "resolve": true, "path": file.display().to_string() });
            options.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            ProgramHost.run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
        };
        let answer = json(&read(serde_json::json!({ "text": false })).expect("answers the program"));
        assert_eq!(answer["program"]["name"], "note-pad", "{answer}");
        assert!(answer.get("text").is_none(), "no text is printed: {answer}");
        let refused = read(serde_json::json!({})).unwrap_err();
        assert!(refused.message.contains("declares locales"), "control: the printed form still needs --output: {}", refused.message);
        let both = read(serde_json::json!({ "text": false, "output": "/tmp/x.orb" })).unwrap_err();
        assert!(both.message.contains("takes no `output`"), "{}", both.message);
    }

    #[test]
    fn read_with_resolve_and_no_text_answers_each_lazy_child_by_its_orbital() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("child.lolo"), "app child [ version: \"1.0.0\" ]\norbital ChildOrbital {\n  entity Note [runtime] {\n    id : string!\n  }\n  trait ChildView -> Note {\n    initial: a\n    state a {\n      INIT -> a\n        (render-ui main { type: \"typography\", content: \"child\" })\n    }\n  }\n  page \"/child\" as ChildPage -> ChildView\n}\n").unwrap();
        let main = |lazy: bool| {
            let head = if lazy { "uses lazy Child from \"./child\"\n" } else { "" };
            let tail = if lazy { "orbital Kid = Child.orbitals.ChildOrbital {\n}\n" } else { "" };
            format!("app main [ version: \"1.0.0\" ]\n{head}orbital MainOrbital {{\n  entity Item [runtime] {{\n    id : string!\n  }}\n  trait MainView -> Item {{\n    initial: a\n    state a {{\n      INIT -> a\n        (render-ui main {{ type: \"typography\", content: \"main\" }})\n    }}\n  }}\n  page \"/\" as Home -> MainView\n}}\n{tail}")
        };
        let read = |text: String| {
            let file = dir.path().join("main.lolo");
            std::fs::write(&file, text).unwrap();
            let options = from_json(serde_json::json!({ "resolve": true, "text": false, "path": file.display().to_string() })).unwrap();
            json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"))
        };
        let with = read(main(true));
        let lazy = with["lazy"].as_array().expect("lazy list");
        assert_eq!(lazy.len(), 1, "{with}");
        assert_eq!(lazy[0]["orbital"], "Kid");
        assert!(lazy[0]["program"]["orbitals"].is_array(), "the child program is answered: {}", lazy[0]);
        assert_eq!(read(main(false))["lazy"], serde_json::json!([]), "control: no lazy import, no children");
    }

    const REFERENCED_ATOM: &str = r#"{ "name": "atom", "version": "1.0.0", "orbitals": [ { "name": "AtomOrbital",
      "entity": { "name": "Widget", "persistence": "persistent", "fields": [ { "name": "id", "type": "string", "required": true }, { "name": "n", "type": "number" } ] },
      "traits": [ { "name": "Widget", "linkedEntity": "Widget", "category": "interaction",
        "config": { "k": { "type": "number", "default": 0 } },
        "stateMachine": { "states": [ { "name": "idle", "isInitial": true } ], "events": [ { "key": "LOAD" } ],
          "transitions": [ { "from": "idle", "to": "idle", "event": "LOAD", "effects": [ ["set", "@entity.n", "@config.k"] ] } ] } } ] } ] }"#;

    const REFERENCED_CONSUMER: &str = r#"{ "name": "consumer", "version": "1.0.0", "orbitals": [ { "name": "Composer",
      "uses": [ { "from": "./atom.orb", "as": "Up" } ],
      "entity": { "name": "Rec", "persistence": "runtime", "fields": [ { "name": "id", "type": "string", "required": true } ] },
      "traits": [ { "ref": "Up.traits.Widget", "name": "T", "linkedEntity": "Rec", "config": { "k": { "type": "number", "default": 1 } } } ] } ] }"#;

    #[test]
    fn read_with_references_answers_the_unresolved_program_and_each_reference_with_its_declaration() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("atom.orb"), REFERENCED_ATOM).unwrap();
        let file = dir.path().join("consumer.orb");
        std::fs::write(&file, REFERENCED_CONSUMER).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        assert_eq!(answer["program"]["name"], "consumer", "{answer}");
        assert_eq!(answer["program"]["orbitals"][0]["traits"][0]["ref"], "Up.traits.Widget", "the program is unresolved: {answer}");
        let references = answer["references"].as_array().expect("a list");
        assert_eq!(references.len(), 1, "{answer}");
        let pair = &references[0];
        assert_eq!(pair["kind"], "trait");
        assert_eq!(pair["alias"], "Up");
        assert_eq!(pair["name"], "Widget");
        assert_eq!(pair["path"], "orbitals[name=Composer].traits[0]");
        assert_eq!(pair["callSite"]["config"]["k"]["default"], 1);
        assert_eq!(pair["declaration"]["name"], "Widget");
        assert!(pair["declaration"].to_string().contains("@config.k"), "{}", pair["declaration"]);
    }

    #[test]
    fn control_read_with_references_answers_none_for_a_program_without_uses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("atom.orb");
        std::fs::write(&file, REFERENCED_ATOM).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        assert_eq!(answer["references"], serde_json::json!([]));
    }

    #[test]
    fn control_read_with_references_refuses_a_relative_path_an_unknown_option_and_a_second_mode() {
        let read = |options: serde_json::Value| {
            ProgramHost
                .run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
                .unwrap_err()
                .message
        };
        assert!(read(serde_json::json!({ "references": true, "path": "note.orb" })).contains("absolute `path`"));
        assert!(read(serde_json::json!({ "references": true, "path": "/x/note.orb", "bogus": true })).contains("unsupported option \"bogus\""));
        assert!(read(serde_json::json!({ "references": true, "stamp": true, "path": "/x/note.orb" })).contains("one of inspect, embeds, resolve, stamp, references per call"));
        assert!(read(serde_json::json!({ "references": true, "path": "/nowhere/note.orb" })).contains("Failed to read"));
    }

    #[test]
    fn read_with_resolve_refuses_a_relative_path() {
        let options = from_json(serde_json::json!({ "resolve": true, "path": "note.lolo" })).unwrap();
        let error = ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).unwrap_err();
        assert!(error.message.contains("absolute `path`"), "{}", error.message);
    }

    fn text_of(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap_or_else(|| panic!("`{key}` in {value}")).to_string()
    }

    #[test]
    fn print_with_a_target_answers_what_orb_emit_prints_and_each_target_differs() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let path = file.display().to_string();
        let orb = json(&print_with(serde_json::json!({ "target": "orb", "path": path })).expect("emits orb"));
        let expected = format!("{}\n", crate::emit::orb_json(NOTE, &path).unwrap());
        assert_eq!(text_of(&orb, "text"), expected, "the bytes `orb emit orb` prints");
        assert_eq!(text_of(&orb, "stderr"), "");
        let lolo = json(&print_with(serde_json::json!({ "target": "lolo", "path": path })).expect("emits lolo"));
        let expected = format!("{}\n", crate::emit::lolo_text(NOTE, &path).unwrap());
        assert_eq!(text_of(&lolo, "text"), expected, "the bytes `orb emit lolo` prints");
        assert_ne!(text_of(&lolo, "text"), text_of(&orb, "text"), "control: the target picks the output");
    }

    #[test]
    fn print_with_a_target_and_output_writes_the_file_and_prints_nothing() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let out = dir.path().join("note.orb");
        let answer = json(
            &print_with(serde_json::json!({ "target": "orb", "path": file.display().to_string(), "output": out.display().to_string() }))
                .expect("emits"),
        );
        assert_eq!(text_of(&answer, "text"), "");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), crate::emit::orb_json(NOTE, &file.display().to_string()).unwrap());
    }

    #[test]
    fn control_print_with_a_target_refuses_a_relative_path_an_unknown_option_and_a_lone_key_half() {
        let refusal = |options: serde_json::Value| print_with(options).unwrap_err().message;
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "note.lolo" })).contains("absolute `path`"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/x/note.lolo", "node": "a" })).contains("unsupported option \"node\""));
        assert!(refusal(serde_json::json!({ "target": "java", "path": "/x/note.lolo" })).contains("not one of the accepted values"));
        assert!(refusal(serde_json::json!({ "target": "manifest", "path": "/x/n.lolo", "signKey": "/k" })).contains("go together"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/x/n.lolo", "documentVersion": 2 })).contains("needs `inspectSource`"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/nowhere/missing.lolo" })).contains("Failed to read"));
    }

    #[test]
    fn read_with_stamp_answers_what_orb_stamp_prints_and_writes_output_when_given() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("note.orb");
        std::fs::write(&orb, crate::emit::orb_json(NOTE, "note.lolo").unwrap()).unwrap();
        let options = from_json(serde_json::json!({ "stamp": true, "path": orb.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("stamps"));
        let expected = crate::commands::stamp::stamped_json(&orb, None, None).expect("orb stamp");
        assert_eq!(answer["text"], serde_json::Value::String(expected.clone()));
        assert_eq!(answer["program"], serde_json::from_str::<serde_json::Value>(&expected).unwrap());
        let out = dir.path().join("stamped.orb");
        let options = from_json(serde_json::json!({
            "stamp": true, "path": orb.display().to_string(), "prior": orb.display().to_string(), "output": out.display().to_string()
        }))
        .unwrap();
        let written = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("stamps"));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text_of(&written, "text"), "--output writes the stamped text");
        let with_prior = crate::commands::stamp::stamped_json(&orb, Some(&orb), None).unwrap();
        assert_eq!(text_of(&written, "text"), with_prior, "--prior anchors the ids");
    }

    #[test]
    fn control_read_with_stamp_refuses_a_relative_path_an_unknown_option_and_a_missing_file() {
        let read = |options: serde_json::Value| {
            ProgramHost
                .run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
                .unwrap_err()
                .message
        };
        assert!(read(serde_json::json!({ "stamp": true, "path": "note.orb" })).contains("absolute `path`"));
        assert!(read(serde_json::json!({ "stamp": true, "path": "/x/note.orb", "resolve": true })).contains("unsupported option \"resolve\""));
        assert!(read(serde_json::json!({ "stamp": true, "path": "/nowhere/note.orb" })).contains("Failed to read"));
    }

    fn generate_with(options: serde_json::Value) -> Result<Value, SubstrateFailure> {
        ProgramHost.run("program/eval", &[Value::Null, from_json(options).unwrap()])
    }

    #[test]
    fn a_shells_prebuilt_web_client_is_copied_into_the_output_with_its_nested_assets() {
        let shell = tempfile::tempdir().expect("tempdir");
        let built = shell.path().join("almadar-web-client");
        std::fs::create_dir_all(built.join("assets/fonts")).unwrap();
        std::fs::write(built.join("index.html"), "<html></html>").unwrap();
        std::fs::write(built.join("assets/fonts/a.woff2"), "font").unwrap();
        let out = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(out.path().join("packages/client/public")).unwrap();
        std::fs::write(out.path().join("packages/client/public/client.json"), "{}").unwrap();
        std::fs::write(out.path().join("packages/client/public/program.client.orb"), "{\"name\":\"app\"}").unwrap();
        std::fs::create_dir_all(out.path().join("locales")).unwrap();
        std::fs::write(out.path().join("locales/en.json"), "{\"hi\":\"Hi\"}").unwrap();
        let copied = copy_web_client(Some(&built), out.path()).expect("copies");
        assert_eq!(copied, out.path().join("web-client"));
        assert_eq!(std::fs::read_to_string(copied.join("index.html")).unwrap(), "<html></html>");
        assert_eq!(std::fs::read_to_string(copied.join("assets/fonts/a.woff2")).unwrap(), "font");
        assert_eq!(std::fs::read_to_string(copied.join("program.client.orb")).unwrap(), "{\"name\":\"app\"}");
        assert_eq!(std::fs::read_to_string(copied.join("locales/en.json")).unwrap(), "{\"hi\":\"Hi\"}");
        let report = crate::commands::compile::CompileReport { files: 3, ..Default::default() };
        let answer = generate_answer(&report, Some(&copied));
        assert_eq!(answer["webClient"], copied.display().to_string());
        assert!(generate_answer(&report, None).get("webClient").is_some_and(serde_json::Value::is_null));
    }

    #[test]
    fn control_no_built_client_or_no_client_mode_files_is_refused() {
        let out = tempfile::tempdir().expect("tempdir");
        let none = copy_web_client(None, out.path()).unwrap_err().message;
        assert!(none.contains("ships no prebuilt web client"), "{none}");
        let shell = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(shell.path().join("assets")).unwrap();
        let unbuilt = copy_web_client(Some(shell.path()), out.path()).unwrap_err().message;
        assert!(unbuilt.contains("holds no index.html"), "{unbuilt}");
        assert!(!out.path().join("web-client").exists());
        std::fs::write(shell.path().join("index.html"), "<html></html>").unwrap();
        let unprojected = copy_web_client(Some(shell.path()), out.path()).unwrap_err().message;
        assert!(unprojected.contains("wrote no client.json"), "{unprojected}");
    }

    #[test]
    fn control_eval_with_generate_refuses_a_relative_path_an_unknown_option_and_a_missing_shell() {
        let refusal = |options: serde_json::Value| generate_with(options).unwrap_err().message;
        assert!(refusal(serde_json::json!({ "generate": { "path": "n.lolo", "output": "/o", "shell": "typescript" } })).contains("absolute `path`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "o", "shell": "typescript" } })).contains("absolute `output`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "typescript", "verbose": true } })).contains("unsupported option \"verbose\""));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o" } })).contains("needs the `shell`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.orb", "output": "/o", "shell": "typescript", "source": "app.lolo" } })).contains("absolute `source`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "cobol" } })).contains("not one of the accepted values"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "typescript" }, "final": true })).contains("unsupported option \"final\""));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/nowhere/n.lolo", "output": "/o", "shell": "typescript" } })).contains("program/eval:"));
    }

    #[test]
    fn step_with_a_raw_report_is_what_orb_execute_steps_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let source = NOTE.to_string();
        let program = ProgramHost.run("program/read", &[Value::String(source.into())]).expect("reads");
        let steps = serde_json::json!([{ "trait": "NoteView", "event": "INIT" }]);
        let raw = from_json(serde_json::json!({ "report": "raw" })).unwrap();
        let answer = json(&ProgramHost.run("program/step", &[program.clone(), from_json(steps.clone()).unwrap(), raw]).expect("steps"));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("program.orb");
        std::fs::write(&orb, json(&program).to_string()).unwrap();
        let expected = crate::commands::execute::run_cascade_with(&orb, &steps, false).expect("execute --steps");
        assert_eq!(answer, expected);
        assert_eq!(answer["success"], true, "{answer}");
    }

    #[test]
    fn control_a_reshaped_step_still_needs_its_from_and_report_takes_only_raw() {
        let source = NOTE.to_string();
        let program = ProgramHost.run("program/read", &[Value::String(source.into())]).expect("reads");
        let steps = from_json(serde_json::json!([{ "orbital": "NoteOrbital", "trait": "NoteView", "event": "INIT" }])).unwrap();
        let error = ProgramHost.run("program/step", &[program.clone(), steps.clone()]).unwrap_err();
        assert!(error.message.contains("needs a from"), "{}", error.message);
        let wrong = from_json(serde_json::json!({ "report": "full" })).unwrap();
        let error = ProgramHost.run("program/step", &[program, steps, wrong]).unwrap_err();
        assert!(error.message.contains("must be \"raw\""), "{}", error.message);
    }

    #[test]
    fn resolve_with_path_answers_what_orb_behaviors_which_prints() {
        let spec = "std/behaviors/std-button";
        let value = from_json(serde_json::json!({ "behavior": spec })).unwrap();
        let options = from_json(serde_json::json!({ "path": true })).unwrap();
        let answer = ProgramHost.run("behavior/resolve", &[value, options]);
        match crate::commands::behaviors::which(spec).expect("packages discover") {
            Ok((path, owner)) => assert_eq!(
                json(&answer.expect("resolves")),
                serde_json::json!({ "path": path.display().to_string(), "owner": owner })
            ),
            Err(refusal) => assert!(answer.unwrap_err().message.contains(&refusal)),
        }
    }

    #[test]
    fn resolve_with_expects_answers_what_orb_behaviors_expects_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("note.orb");
        let program = ProgramHost.run("program/read", &[Value::String(NOTE.to_string().into())]).expect("reads");
        std::fs::write(&orb, json(&program).to_string()).unwrap();
        let spec = orb.display().to_string();
        let value = from_json(serde_json::json!({ "behavior": spec })).unwrap();
        let options = from_json(serde_json::json!({ "expects": true })).unwrap();
        let answer = json(&ProgramHost.run("behavior/resolve", &[value, options]).expect("derives"));
        assert_eq!(answer, crate::commands::behaviors_catalog::expectations(&spec).unwrap());
        assert!(answer.get("NoteOrbital").is_some(), "{answer}");
    }

    #[test]
    fn control_resolve_with_path_refuses_a_value_naming_no_behavior() {
        let options = from_json(serde_json::json!({ "path": true })).unwrap();
        let error = ProgramHost.run("behavior/resolve", &[Value::Null, options]).unwrap_err();
        assert!(error.message.contains("naming its behavior"), "{}", error.message);
    }

    #[test]
    fn validate_with_a_report_answers_what_orb_validate_json_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/validate", &[Value::Null, options]).expect("reports"));
        let expected = serde_json::to_value(crate::commands::validate::validation_report(&file, None, None, false, false, &[]).unwrap()).unwrap();
        assert_eq!(answer, expected);
        assert!(answer.get("valid").is_some(), "{answer}");
    }

    #[test]
    fn validate_with_findings_positions_each_lint_finding_like_a_compiler_finding() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let report = |findings: serde_json::Value| {
            let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string(), "findings": findings })).unwrap();
            ProgramHost.run("program/validate", &[Value::Null, options]).map(|v| json(&v))
        };
        let plain = report(serde_json::json!([])).expect("reports");
        let expected = serde_json::to_value(crate::commands::validate::validation_report(&file, None, None, false, false, &[]).unwrap()).unwrap();
        assert_eq!(plain, expected, "control: no findings leave the report as it is");
        let with = report(serde_json::json!([
            { "code": "ORB_LINT_E", "path": "orbitals[0].traits[0]", "message": "m", "suggestion": "", "severity": "error", "validValues": ["a", "b"] },
            { "code": "ORB_LINT_W", "path": "orbitals[0]", "message": "w", "suggestion": "s", "severity": "warning" }
        ]))
        .expect("reports");
        let error = with["errors"].as_array().unwrap().iter().find(|e| e["code"] == "ORB_LINT_E").expect("the lint error joins errors");
        assert!(error["line"].is_number() && error["column"].is_number(), "positioned: {error}");
        assert!(error.get("suggestion").is_none(), "an empty suggestion is left out: {error}");
        assert_eq!(error["validValues"], serde_json::json!(["a", "b"]), "the closed set travels with the finding");
        assert!(with["warnings"].as_array().unwrap().iter().all(|w| w.get("validValues").is_none()), "control: a finding without one carries none");
        assert_eq!(with["valid"], false, "an error-severity finding fails the report");
        let warning = with["warnings"].as_array().unwrap().iter().find(|w| w["code"] == "ORB_LINT_W").expect("the lint warning joins warnings");
        assert_eq!(warning["suggestion"], "s");
        assert!(warning["line"].is_number(), "positioned: {warning}");
        let refused = report(serde_json::json!([{ "code": "X", "path": "p", "message": "m", "severity": "fatal" }])).unwrap_err();
        assert!(refused.message.contains("`findings` must be a list"), "{}", refused.message);
    }

    #[test]
    fn a_lint_error_keeps_the_compile_step_from_restating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("ghost.orb");
        std::fs::write(
            &file,
            serde_json::json!({ "name": "ghost", "version": "1.0.0", "orbitals": [{ "name": "Notes",
                "entity": { "name": "Note", "persistence": "runtime", "fields": [
                    { "name": "id", "type": "string", "required": true },
                    { "name": "owner", "type": "relation", "relation": { "entity": "Ghost", "cardinality": "one" } }
                ] }, "traits": [], "pages": [] }] })
            .to_string(),
        )
        .unwrap();
        let codes = |findings: serde_json::Value| {
            let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string(), "findings": findings })).unwrap();
            let report = json(&ProgramHost.run("program/validate", &[Value::Null, options]).expect("reports"));
            report["errors"].as_array().into_iter().flatten().map(|e| e["code"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>()
        };
        let relation = serde_json::json!({ "code": "ORB_E_INVALID_RELATION", "path": "orbitals[0].entity.fields[1].relation.entity", "message": "Relation target entity 'Ghost' does not exist", "severity": "error" });
        let with = codes(serde_json::json!([relation]));
        assert!(with.contains(&"ORB_E_INVALID_RELATION".to_string()), "{with:?}");
        assert!(!with.contains(&"COMPILATION_ERROR".to_string()), "the compile step only restates the lint error: {with:?}");
        let without = codes(serde_json::json!([]));
        assert!(without.contains(&"COMPILATION_ERROR".to_string()), "control: with no finding the compile step still reports: {without:?}");
        let warned = codes(serde_json::json!([{ "code": "ORB_LINT_W", "path": "orbitals[0]", "message": "w", "severity": "warning" }]));
        assert!(warned.contains(&"COMPILATION_ERROR".to_string()), "control: a warning does not hold the compile step back: {warned:?}");
    }

    #[test]
    fn control_validate_report_refuses_a_relative_path_and_an_unknown_option() {
        let relative = from_json(serde_json::json!({ "report": true, "path": "note.lolo" })).unwrap();
        assert!(ProgramHost.run("program/validate", &[Value::Null, relative]).unwrap_err().message.contains("absolute `path`"));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.lolo").display().to_string();
        let unknown = from_json(serde_json::json!({ "report": true, "path": path, "prior": {} })).unwrap();
        assert!(ProgramHost.run("program/validate", &[Value::Null, unknown]).unwrap_err().message.contains("unsupported option"));
    }

    fn print_with(options: serde_json::Value) -> Result<Value, SubstrateFailure> {
        ProgramHost.run("program/print", &[Value::Null, from_json(options).unwrap()])
    }

    #[test]
    fn print_of_source_keeps_its_comments_as_orb_format_does() {
        let commented = NOTE.replace("orbital NoteOrbital {", ";; the one note\norbital NoteOrbital {");
        let Value::String(text) = print_with(serde_json::json!({ "source": commented })).expect("prints") else {
            panic!("text")
        };
        assert!(text.contains(";; the one note"), "{text}");
        assert_eq!(text, crate::commands::parse::formatted_text(&commented, "x").unwrap(), "the same bytes as orb format");
    }

    #[test]
    fn print_of_a_node_is_that_construct_and_nodes_is_every_match() {
        let Value::String(entity) = print_with(serde_json::json!({ "source": NOTE, "node": "orbitals[0].entity" })).expect("prints") else {
            panic!("text")
        };
        assert!(entity.starts_with("entity Note"), "{entity}");
        let fields = json(&print_with(serde_json::json!({ "source": NOTE, "nodes": "orbitals[0].entity.fields[*]" })).expect("prints"));
        assert_eq!(fields.as_array().map(Vec::len), Some(2), "{fields}");
    }

    #[test]
    fn print_refuses_a_node_without_source_a_wildcard_node_and_both_keys() {
        let message = |options| print_with(options).unwrap_err().message;
        assert!(message(serde_json::json!({ "node": "orbitals[0]" })).contains("missing"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[*]" })).contains("use `nodes`"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[0]", "nodes": "orbitals[*]" })).contains("not both"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[3]" })).contains("out of range"));
    }

    #[test]
    fn validate_reports_the_validators_findings_and_a_bad_name_before_any_run() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
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
    fn a_resolved_behavior_carries_the_client_metadata_a_browser_kernel_registers_it_with() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let resolved = json(&ProgramHost.run("behavior/resolve", &[value]).expect("resolve"));
        let metadata = &resolved["_metadata"];
        assert!(metadata["protocols"].as_array().is_some_and(|rows| !rows.is_empty()), "{metadata}");
        assert!(metadata.get("dispatchFacts").is_some(), "{metadata}");
        assert_eq!(
            metadata.as_object().unwrap().iter().filter(|(k, _)| ["operatorSites", "dispatchFacts", "protocols"].contains(&k.as_str())).count(),
            orbital_core::effects::runs_on::client_program_metadata(&resolved["orbitals"]).len(),
            "the same metadata the served client program carries"
        );
    }

    #[test]
    fn edge_a_resolved_program_keeps_the_metadata_it_already_carries() {
        let mut program = serde_json::json!({ "name": "P", "orbitals": [], "_metadata": { "provenance": "kept" } });
        with_client_metadata(&mut program);
        assert_eq!(program["_metadata"]["provenance"], "kept");
        assert!(program["_metadata"].get("protocols").is_some());
    }

    #[test]
    fn source_with_the_file_option_answers_the_path_and_file_text_as_read_behavior_does() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
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
    fn packages_carry_the_index_fingerprint_their_catalog_is_cached_under() {
        let listed = json(&ProgramHost::packages().expect("packages"));
        let cwd = std::env::current_dir().unwrap();
        let installed = crate::commands::npm_project::discover_from(&cwd).unwrap();
        let entries = listed.as_array().unwrap();
        for (i, entry) in entries.iter().enumerate() {
            let expected = crate::commands::behaviors_index::fingerprint(&installed, i).unwrap();
            assert_eq!(entry["fingerprint"], expected.as_str(), "{}", entry["prefix"]);
            assert_eq!(expected.len(), 64);
        }
        let distinct: std::collections::HashSet<&str> =
            entries.iter().filter_map(|e| e["fingerprint"].as_str()).collect();
        assert_eq!(distinct.len(), entries.len(), "control: each package has its own fingerprint");
    }

    #[test]
    fn program_compose_assembles_an_orbital_value_into_an_app_carrying_its_hooks() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "Drop", "orbitals": [{ "behavior": "almadar-std/std-file-drop", "orbital": "FileDropOrbital" }] })).unwrap();
        let options =
            from_json(serde_json::json!({ "appName": "DropApp", "theme": "ocean" })).unwrap();
        let schema = json(
            &host
                .run(
                    "program/compose",
                    &[Value::Array(vec![program.clone()].into()), options],
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
            host.run("program/compose", &[Value::Array(vec![program].into()), bad])
                .unwrap_err()
                .message
                .contains("layout must be one of"),
            "control: an unknown layout is refused"
        );
    }

    #[test]
    fn eval_writes_a_valid_program_into_the_project_and_refuses_an_invalid_one() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = json(
            &host
                .run("program/read", &[Value::String(NOTE.to_string().into())])
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
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
