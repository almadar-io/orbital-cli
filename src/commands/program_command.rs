//! `orb <command> …`: an installed behavior in the `cli` topic that declares the inbound hook `cli-<command>` is the
//! command, resolved before `orb`'s own subcommands so a Rust variant cannot shadow a program. `orb` boots it, delivers
//! `{ argv, cwd }` to that hook as JSON, prints the answer's body and exits with the answer's status class
//! (EXEC-INBOUND-1; the answer may come from a dispatch the handler's keyed completions cause,
//! `orbital_shell_rust::host::Host::deliver_inbound_awaiting`).

use orbital_shell_rust::host::run::{hold, stop_on_signal};
use std::path::{Path, PathBuf};

/// The hook a command's program declares.
pub(crate) fn hook_name(command: &str) -> String {
    format!("cli-{command}")
}

/// The process exit code of an answer's status: 0 for 2xx, 2 for 4xx (the caller's input was refused), else 1.
pub(crate) fn exit_code(status: u16) -> i32 {
    match status {
        200..=299 => 0,
        400..=499 => 2,
        _ => 1,
    }
}

/// A command line resolved to an installed program: `orb <command…> <argv…>` runs `program` on the hook `cli-<command>`.
#[derive(Debug)]
pub(crate) struct ProgramCommand {
    pub(crate) program: PathBuf,
    pub(crate) command: String,
    pub(crate) argv: Vec<String>,
}

/// The bootloader's own words: clap takes them before any program discovery (so a broken package can be repaired
/// with `orb update|add|remove`), and a `cli` program declaring `cli-<word>` or `cli-<word>-…` is refused.
const RESERVED_COMMANDS: &[&str] = &[
    "update", "add", "remove", "list", "run", "help", "--help",
    "-h", "--version", "-V",
];

/// The reserved word a hook would take, if any.
fn reserved_word_of(hook: &str) -> Option<&'static str> {
    let command = hook.strip_prefix("cli-")?;
    RESERVED_COMMANDS
        .iter()
        .copied()
        .find(|word| command == *word || command.strip_prefix(*word).is_some_and(|rest| rest.starts_with('-')))
}

/// The commands `args` (argv after the binary) may name, in the order they are tried: `w1 w2 …` is `w1-w2` with
/// `…`, then `w1` with `w2 …`; a leading flag names none.
fn command_candidates(args: &[String]) -> Vec<(String, &[String])> {
    let is_word = |a: &String| !a.starts_with('-');
    match args {
        [first, second, rest @ ..] if is_word(first) && is_word(second) => {
            vec![(format!("{first}-{second}"), rest), (first.clone(), &args[1..])]
        }
        [first, rest @ ..] if is_word(first) => vec![(first.clone(), rest)],
        _ => Vec::new(),
    }
}

/// The installed program `args` names, if any (see [`command_candidates`]); `None` leaves the line to `orb`'s own
/// subcommands. A reserved word goes to them without reading any package.
pub(crate) fn resolve_program_command(args: &[String], cwd: &Path, user_store: &Path) -> anyhow::Result<Option<ProgramCommand>> {
    if args.first().is_some_and(|first| RESERVED_COMMANDS.contains(&first.as_str())) {
        return Ok(None);
    }
    let candidates = command_candidates(args);
    if candidates.is_empty() {
        return Ok(None);
    }
    let programs = cli_programs(cwd, user_store)?;
    for (command, argv) in &candidates {
        if let Some(program) = programs.declaring(&hook_name(command))? {
            return Ok(Some(ProgramCommand { program, command: command.clone(), argv: argv.to_vec() }));
        }
    }
    programs.unanswered(&candidates.iter().map(|(command, _)| hook_name(command)).collect::<Vec<_>>())?;
    Ok(None)
}

/// The installed program in a `cli` topic that declares the hook `cli-<command>`, from the behavior packages resolved
/// from `cwd` and then `user_store`. More than one is an error that names them.
#[cfg(test)]
fn find_command_program(command: &str, cwd: &Path, user_store: &Path) -> anyhow::Result<Option<PathBuf>> {
    let programs = cli_programs(cwd, user_store)?;
    let hook = hook_name(command);
    let found = programs.declaring(&hook)?;
    if found.is_none() {
        programs.unanswered(std::slice::from_ref(&hook))?;
    }
    Ok(found)
}

/// The installed `cli`-topic programs, indexed by the hooks their app header declares (parsed, not lowered: a program
/// that fails to lower still owns its command and reports the failure when that command runs). A program whose header
/// cannot be parsed, or which declares a reserved word, owns no command; it is reported by a lookup no program answers.
struct CliPrograms {
    declared: Vec<(PathBuf, Vec<String>)>,
    unusable: Vec<String>,
}

impl CliPrograms {
    fn declaring(&self, hook: &str) -> anyhow::Result<Option<PathBuf>> {
        let mut found: Vec<&PathBuf> =
            self.declared.iter().filter(|(_, hooks)| hooks.iter().any(|h| h == hook)).map(|(p, _)| p).collect();
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop().cloned()),
            _ => anyhow::bail!(
                "more than one installed program declares `{hook}`: {}",
                found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Fails naming every unusable program, any of which may be the one meant to declare one of `hooks`.
    fn unanswered(&self, hooks: &[String]) -> anyhow::Result<()> {
        if self.unusable.is_empty() {
            return Ok(());
        }
        let hooks = hooks.iter().map(|hook| format!("`{hook}`")).collect::<Vec<_>>().join(" or ");
        anyhow::bail!(
            "no installed program declares {hooks}, and these `cli` programs declare no command:\n{}",
            self.unusable.join("\n")
        )
    }
}

fn cli_programs(cwd: &Path, user_store: &Path) -> anyhow::Result<CliPrograms> {
    let packages = orbital_core::behavior_packages::discover(cwd, user_store).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut programs = CliPrograms { declared: Vec::new(), unusable: Vec::new() };
    for package in &packages {
        let files = orbital_core::behavior_index::indexed_files(package).map_err(|e| anyhow::anyhow!("{e}"))?;
        for file in files.iter().filter(|file| file.topic() == "cli") {
            let Some(path) = orbital_core::behavior_index::lolo_path_of(package, file).filter(|p| p.is_file()) else {
                continue;
            };
            let source = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            let hooks = match orbital_lolo::parse_app_header(&source) {
                Ok(header) => header.inbound,
                Err(inner) => {
                    programs
                        .unusable
                        .push(orbital_lolo::LoloError::Parse { path: path.display().to_string(), inner }.render());
                    continue;
                }
            };
            if let Some((hook, word)) = hooks.iter().find_map(|hook| reserved_word_of(hook).map(|word| (hook, word))) {
                programs.unusable.push(format!(
                    "{} declares `{hook}`, but `{word}` is a reserved `orb` command no program can take",
                    path.display()
                ));
                continue;
            }
            programs.declared.push((path, hooks));
        }
    }
    Ok(programs)
}

/// A command program's answer: the process exit code of its status, the body for stdout and the `stderr` text, if any.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommandAnswer {
    pub(crate) code: i32,
    pub(crate) body: String,
    pub(crate) stderr: Option<String>,
    /// The answer's declared headers; `orb-run` hands a program back to the bootloader to run (see [`run_handoff`]).
    pub(crate) headers: std::collections::BTreeMap<String, String>,
}

impl CommandAnswer {
    /// The body on `stdout` and the `stderr` text on `stderr`, each as a line when not empty.
    pub(crate) fn write_to(
        &self,
        stdout: &mut dyn std::io::Write,
        stderr: &mut dyn std::io::Write,
    ) -> std::io::Result<()> {
        if !self.body.is_empty() {
            writeln!(stdout, "{}", self.body)?;
        }
        match self.stderr.as_deref() {
            Some(text) if !text.is_empty() => writeln!(stderr, "{text}"),
            _ => Ok(()),
        }
    }
}

/// Run `program` as `orb <command> <argv…>`: its answer, with the exit code of its status.
pub(crate) fn run_command_program(program: &Path, command: &str, argv: &[String], cwd: &Path) -> anyhow::Result<CommandAnswer> {
    run_command_program_anchored(program, command, argv, cwd, None)
}

/// [`run_command_program`] with the program's grants and paths anchored at `anchors` instead of this process's.
pub(crate) fn run_command_program_anchored(
    program: &Path,
    command: &str,
    argv: &[String],
    cwd: &Path,
    anchors: Option<orbital_core::runtime::os_host::OsAnchors>,
) -> anyhow::Result<CommandAnswer> {
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let config = orbital_shell_rust::host::HostConfig {
        substrate: Some(host),
        anchors,
        ..orbital_shell_rust::host::HostConfig::default()
    };
    let runtime =
        orbital_shell_rust::host::boot::boot_file_with(&resolved, false, &std::env::vars().collect(), false, config)?;
    let body = serde_json::json!({ "argv": argv, "cwd": cwd.display().to_string() }).to_string();
    let answer = runtime.deliver_inbound_awaiting(&hook_name(command), [("content-type", "application/json")], &body, None);
    Ok(CommandAnswer {
        code: exit_code(answer.status),
        body: answer.body,
        stderr: answer.stderr,
        headers: answer.headers.unwrap_or_default(),
    })
}

/// `orb run <program>`, booted: the program on the substrate host, `INIT` mounted on every trait.
pub(crate) fn boot_run(program: &Path, trace: bool) -> anyhow::Result<std::sync::Arc<orbital_shell_rust::host::Host>> {
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let runtime = orbital_shell_rust::host::boot::boot_file(&resolved, false, &std::env::vars().collect(), trace, Some(host))?;
    orbital_shell_rust::host::run::mount_init(&runtime, orbital_shell_rust::host::run::ProtocolSide::Process).map_err(|e| anyhow::anyhow!("INIT: {e}"))?;
    Ok(runtime)
}

/// `orb run <program>`: boot it, hold it while it has open calls, stop cleanly on SIGINT/SIGTERM.
pub(crate) fn run_program(program: &Path) -> anyhow::Result<()> {
    let stop = stop_on_signal()?;
    let runtime = boot_run(program, false)?;
    hold(&runtime, &stop);
    Ok(())
}

/// The behavior `orb serve` composes beside the app.
const SERVER_BEHAVIOR: &str = "std/behaviors/std-http-server";

/// The `std-http1` host and port with their listen grant, whether the `PORT` environment variable replaces the port
/// (with the `env PORT` listen grant on the host), and a client directory (relative to the server's working directory)
/// with its read grant.
pub(crate) fn server_overrides(host: &str, port: u16, port_from_env: bool, client_dir: Option<&str>) -> serde_json::Value {
    let mut overrides = serde_json::json!({
        "Http1.host": host,
        "Http1.port": port,
        "Http1.portFromEnv": port_from_env,
        "grants": { "net": [{ "target": format!("{host}:{port}"), "kinds": ["listen"] }], "anchored": {} },
    });
    if port_from_env {
        overrides["grants"]["anchored"]["net"] = serde_json::json!([{
            "host": { "literal": host },
            "port": { "anchor": { "env": "PORT" } },
            "kinds": ["listen"],
        }]);
    }
    if let Some(relative) = client_dir {
        overrides["ServerStatic.clientDir"] = serde_json::json!(relative);
        overrides["grants"]["anchored"]["fs"] =
            serde_json::json!([{ "anchor": "cwd", "glob": format!("{relative}/**"), "modes": ["read"] }]);
    }
    overrides
}

/// `std-http-server` resolved with the serve options' knobs set as call-site config on its traits (`Trait.knob`), so the
/// resolver substitutes them where the program reads them; a key naming no trait of the server is refused.
fn resolved_server(overrides: &serde_json::Value) -> anyhow::Result<std::path::PathBuf> {
    let source_path = crate::commands::behaviors_catalog::behavior_source_path(SERVER_BEHAVIOR)?;
    let mut source = crate::commands::behaviors_catalog::source(SERVER_BEHAVIOR)?;
    for (key, value) in overrides.as_object().into_iter().flatten().filter(|(k, _)| k.as_str() != "grants") {
        let (trait_name, knob) = key
            .split_once('.')
            .ok_or_else(|| anyhow::anyhow!("serve override {key} is not <Trait>.<knob>"))?;
        let found = source
            .get_mut("orbitals")
            .and_then(serde_json::Value::as_array_mut)
            .into_iter()
            .flatten()
            .flat_map(|o| o.get_mut("traits").and_then(serde_json::Value::as_array_mut).into_iter().flatten())
            .find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some(trait_name))
            .ok_or_else(|| anyhow::anyhow!("serve override {key}: {SERVER_BEHAVIOR} has no trait {trait_name}"))?;
        let config = found
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("serve override {key}: trait {trait_name} is not an object"))?
            .entry("config")
            .or_insert_with(|| serde_json::json!({}));
        let kind = match value {
            serde_json::Value::String(_) => "string",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::Bool(_) => "boolean",
            _ => return Err(anyhow::anyhow!("serve override {key}: only a text, number or boolean knob can be set")),
        };
        config[knob] = serde_json::json!({ "type": kind, "default": value });
    }
    let schema: orbital_core::OrbitalSchema =
        serde_json::from_value(source).map_err(|e| anyhow::anyhow!("{SERVER_BEHAVIOR}: {e}"))?;
    crate::commands::dev::resolve_schema_to_temp(schema, &source_path)
}

/// The app and `std-http-server` (its knobs set from `overrides`), the programs `program/compose` joins, with the app's
/// name.
fn served_programs(app: serde_json::Value, overrides: &serde_json::Value) -> anyhow::Result<(Vec<orbital_core::Value>, String)> {
    let app_name = app
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the program to serve names no app"))?
        .to_string();
    let server_path = resolved_server(overrides)?;
    let server: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&server_path).map_err(|e| anyhow::anyhow!("read {}: {e}", server_path.display()))?,
    )
    .map_err(|e| anyhow::anyhow!("{}: {e}", server_path.display()))?;
    let programs = [app, server]
        .into_iter()
        .map(|p| serde_json::from_value(p).map_err(|e| anyhow::anyhow!("the program to compose: {e}")))
        .collect::<anyhow::Result<Vec<orbital_core::Value>>>()?;
    Ok((programs, app_name))
}

fn grants_only(overrides: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "grants": overrides["grants"].clone() })
}

/// The resolved `app` composed with `std-http-server` as `orb serve` composes it, for a compiled server to run.
pub(crate) fn served_schema(app: &orbital_core::OrbitalSchema, overrides: &serde_json::Value) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let app = serde_json::to_value(app).map_err(|e| anyhow::anyhow!("the program to serve: {e}"))?;
    let (programs, app_name) = served_programs(app, overrides)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    orbital_shell_rust::host::boot::compose(programs, &app_name, Some(grants_only(overrides)), &host)
}

/// Run the program a cli command handed back (`orb-run` in its answer's headers), as the bootloader runs every program.
/// `orb-run: server` boots the composed app in the body as `orb serve` does: catalogs beside `orb-app`, the mock store
/// under `orb-mock: on`, `INIT` mounted on the server side, held until a signal.
pub(crate) fn run_handoff(answer: &CommandAnswer) -> anyhow::Result<()> {
    let header = |name: &str| answer.headers.get(name).map(String::as_str);
    match header("orb-run") {
        Some("server") => {}
        other => anyhow::bail!("orb-run: no hand-off named {other:?}; the bootloader runs `server`"),
    }
    let mock = match header("orb-mock") {
        Some("on") => true,
        Some("off") => false,
        other => anyhow::bail!("orb-run: server needs orb-mock on|off, got {other:?}"),
    };
    let app = header("orb-app").ok_or_else(|| anyhow::anyhow!("orb-run: server needs orb-app, the resolved app beside its catalogs"))?;
    let schema: orbital_core::OrbitalSchema =
        serde_json::from_str(&answer.body).map_err(|e| anyhow::anyhow!("orb-run: server: the body is not a program: {e}"))?;
    let locales = schema.locales.clone().unwrap_or_default();
    let catalogs = orbital_shell_rust::host::boot::read_catalogs(Path::new(app), &locales)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let stop = stop_on_signal()?;
    let runtime = orbital_shell_rust::host::boot::boot_app(schema, host, catalogs, mock, &std::env::vars().collect(), false)?;
    orbital_shell_rust::host::run::mount_init(&runtime, orbital_shell_rust::host::run::ProtocolSide::Server).map_err(|e| anyhow::anyhow!("INIT: {e}"))?;
    hold(&runtime, &stop);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbital_shell_rust::host::run::{RunEnd, RUN_ENTITY};

    const ECHO: &str = r#"app fake-echo [
  version: "1.0.0",
  inbound: ["cli-echo"],
  description: "Answers the arguments it was given, joined by spaces."
]

orbital EchoOrbital {
  type EchoArgs = { argv : [string]!, cwd : string! }

  entity EchoRun [runtime] {
    id : string!
  }

  trait Echo -> EchoRun {
    initial: ready

    state ready {
      INIT -> ready
      ECHO_CALLED -> ready when (= (array/len (object/get (json/parse @payload.rawBody EchoArgs) "argv")) 0)
        (emit ECHO_ANSWERED { status: 400, body: "usage: orb echo <words>" })
      ECHO_CALLED -> ready when (> (array/len (object/get (json/parse @payload.rawBody EchoArgs) "argv")) 0)
        (emit ECHO_ANSWERED { status: 200, body: (str/join (object/get (json/parse @payload.rawBody EchoArgs) "argv") " ") })
    }

    listens {
      ECHO_CALLED -> inbound "cli-echo" answers ECHO_ANSWERED {
        headers : Map string string
        rawBody : string
      }
    }

    emits {
      ECHO_ANSWERED -> internal { status : number! body : string! }
    }
  }

  page boot as EchoStart -> Echo
}
"#;

    fn project(name: &str, topic: &str) -> (PathBuf, PathBuf) {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let root = std::env::temp_dir().join(format!("orb-program-command-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("project/node_modules/@fake/cli");
        let dir = package.join("behaviors/lolo").join(topic).join("atoms");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{ "name": "@fake/cli", "version": "1.0.0", "orb": { "prefix": "fake", "source": "behaviors/lolo", "compiler": "*" } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("fake-echo.lolo"), ECHO).unwrap();
        std::fs::create_dir_all(root.join("store")).unwrap();
        (root.join("project"), root.join("store"))
    }

    #[test]
    fn the_status_class_is_the_exit_code() {
        assert_eq!([200, 201, 299].map(exit_code), [0, 0, 0]);
        assert_eq!([400, 404, 422].map(exit_code), [2, 2, 2]);
        assert_eq!([500, 503, 100, 302].map(exit_code), [1, 1, 1, 1]);
    }

    #[test]
    fn a_cli_program_declaring_the_hook_is_the_command_and_runs_with_its_arguments() {
        let (cwd, store) = project("found", "cli");
        let program = find_command_program("echo", &cwd, &store).unwrap().expect("the cli program is found");
        assert!(program.ends_with("cli/atoms/fake-echo.lolo"), "{}", program.display());
        assert_eq!(find_command_program("nope", &cwd, &store).unwrap(), None, "control: no program declares cli-nope");
        let CommandAnswer { code, body: out, .. } = run_command_program(&program, "echo", &["hello".into(), "world".into()], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hello world"));
        let CommandAnswer { code, body: out, .. } = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (2, "usage: orb echo <words>"), "a refused input is exit 2 with its message");
    }

    fn with_stderr_program(cwd: &Path) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        let source = ECHO
            .replace("cli-echo", "cli-warn")
            .replace(
                r#"{ status: 400, body: "usage: orb echo <words>" }"#,
                r#"{ status: 400, body: "", stderr: "usage: orb warn <words>" }"#,
            )
            .replace("{ status : number! body : string! }", "{ status : number! body : string! stderr : string }");
        std::fs::write(dir.join("fake-warn.lolo"), source).unwrap();
    }

    fn written(answer: &CommandAnswer) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        answer.write_to(&mut out, &mut err).unwrap();
        (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn an_answer_stderr_reaches_stderr_and_its_status_is_the_exit_code() {
        let (cwd, store) = project("stderr", "cli");
        with_stderr_program(&cwd);
        let program = find_command_program("warn", &cwd, &store).unwrap().expect("the stderr program is found");
        let refused = run_command_program(&program, "warn", &[], &cwd).unwrap();
        assert_eq!(
            refused,
            CommandAnswer { code: 2, body: String::new(), stderr: Some("usage: orb warn <words>".into()), headers: Default::default() }
        );
        assert_eq!(written(&refused), (String::new(), "usage: orb warn <words>\n".into()));
        let answered = run_command_program(&program, "warn", &["hi".into()], &cwd).unwrap();
        assert_eq!(
            answered,
            CommandAnswer { code: 0, body: "hi".into(), stderr: None, headers: Default::default() },
            "an answer that omits stderr has none"
        );
        assert_eq!(written(&answered), ("hi\n".into(), String::new()));
    }

    #[test]
    fn control_an_answer_without_stderr_prints_its_body_on_stdout_only() {
        let (cwd, store) = project("no-stderr", "cli");
        let program = find_command_program("echo", &cwd, &store).unwrap().expect("the cli program is found");
        let refused = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!(refused, CommandAnswer { code: 2, body: "usage: orb echo <words>".into(), stderr: None, headers: Default::default() });
        assert_eq!(written(&refused), ("usage: orb echo <words>\n".into(), String::new()));
    }

    #[test]
    fn a_program_that_fails_to_lower_fails_only_its_own_command() {
        let (cwd, store) = project("lower-broken", "cli");
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        std::fs::write(
            dir.join("fake-bad.lolo"),
            ECHO.replace("cli-echo", "cli-bad").replace("status : number!", "status : Nope!"),
        )
        .unwrap();
        let echo = find_command_program("echo", &cwd, &store).unwrap().expect("the healthy command still resolves");
        let CommandAnswer { code, body: out, .. } = run_command_program(&echo, "echo", &["hi".into()], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hi"));
        let bad =
            find_command_program("bad", &cwd, &store).unwrap().expect("the broken program's hook is still readable");
        assert!(bad.ends_with("cli/atoms/fake-bad.lolo"), "{}", bad.display());
        let error = run_command_program(&bad, "bad", &[], &cwd).unwrap_err().to_string();
        assert!(error.contains("fake-bad.lolo") && error.contains("Nope"), "{error}");
        assert_eq!(
            find_command_program("nope", &cwd, &store).unwrap(),
            None,
            "control: a lowering failure is not a missing command"
        );
    }

    #[test]
    fn an_unparseable_program_is_reported_only_where_no_healthy_program_answers() {
        let (cwd, store) = project("parse-broken", "cli");
        with_broken_program(&cwd);
        let echo = find_command_program("echo", &cwd, &store).unwrap().expect("the healthy command still resolves");
        assert!(echo.ends_with("cli/atoms/fake-echo.lolo"), "{}", echo.display());
        assert_eq!(routed(&["echo", "hi"], &cwd, &store), Some(("echo".into(), words(&["hi"]))));
        let error = find_command_program("nope", &cwd, &store).unwrap_err().to_string();
        assert!(error.contains("fake-broken.lolo") && error.contains("`cli-nope`"), "{error}");
    }

    fn with_programs(cwd: &Path, hooks: &[&str]) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        for hook in hooks {
            std::fs::write(dir.join(format!("fake-{hook}.lolo")), ECHO.replace("cli-echo", hook)).unwrap();
        }
    }

    fn with_broken_program(cwd: &Path) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        std::fs::write(dir.join("fake-broken.lolo"), "app fake-broken [ this does not parse").unwrap();
    }

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn routed(args: &[&str], cwd: &Path, store: &Path) -> Option<(String, Vec<String>)> {
        resolve_program_command(&words(args), cwd, store).unwrap().map(|c| (c.command, c.argv))
    }

    #[test]
    fn program_first_a_clap_subcommand_word_resolves_to_the_program_declaring_it() {
        use clap::CommandFactory;
        let (cwd, store) = project("program-first", "cli");
        with_programs(&cwd, &["cli-serve", "cli-behaviors", "cli-behaviors-describe"]);
        let clap = crate::Cli::command();
        assert!(clap.find_subcommand("serve").is_some() && clap.find_subcommand("behaviors").is_some());
        assert_eq!(routed(&["serve", "a"], &cwd, &store), Some(("serve".into(), words(&["a"]))));
        assert_eq!(
            routed(&["behaviors", "describe", "x"], &cwd, &store),
            Some(("behaviors-describe".into(), words(&["x"]))),
            "the two-word hook is tried first"
        );
        assert_eq!(routed(&["behaviors", "catalog"], &cwd, &store), Some(("behaviors".into(), words(&["catalog"]))));
        let command = resolve_program_command(&words(&["behaviors", "describe", "hi"]), &cwd, &store).unwrap().unwrap();
        let CommandAnswer { code, body: out, .. } = run_command_program(&command.program, &command.command, &command.argv, &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hi"));
    }

    #[test]
    fn simulate_and_language_which_resolve_to_their_programs_and_clap_no_longer_has_them() {
        use clap::Parser;
        let (cwd, store) = project("route1", "cli");
        with_programs(&cwd, &["cli-simulate", "cli-language"]);
        assert_eq!(routed(&["simulate", "app.lolo"], &cwd, &store), Some(("simulate".into(), words(&["app.lolo"]))));
        assert_eq!(routed(&["language", "which"], &cwd, &store), Some(("language".into(), words(&["which"]))));
        assert!(crate::Cli::try_parse_from(["orb", "validate", "a.lolo", "--simulate"]).is_err());
        assert!(crate::Cli::try_parse_from(["orb", "language", "which"]).is_err());
    }

    #[test]
    fn control_a_word_with_no_program_reaches_clap() {
        let (cwd, store) = project("no-program", "cli");
        assert_eq!(routed(&["validate", "app.lolo"], &cwd, &store), None);
        assert_eq!(routed(&["behaviors", "catalog"], &cwd, &store), None);
        assert_eq!(routed(&["--help"], &cwd, &store), None);
        assert_eq!(routed(&["--list"], &cwd, &store), None, "a leading flag names no program");
        assert_eq!(routed(&[], &cwd, &store), None);
    }

    #[test]
    fn the_agent_words_are_not_clap_subcommands() {
        use clap::Parser;
        for args in [&["orb", "fix", "a.orb"][..], &["orb", "login"], &["orb", "eval", "--all"], &["orb", "trace"], &["orb", "--list"]] {
            let error = crate::Cli::try_parse_from(args).err().unwrap_or_else(|| panic!("{args:?} parsed"));
            assert!(
                matches!(error.kind(), clap::error::ErrorKind::InvalidSubcommand | clap::error::ErrorKind::UnknownArgument),
                "{args:?}: {:?}",
                error.kind()
            );
        }
    }

    #[test]
    fn control_a_kept_subcommand_still_parses() {
        use clap::Parser;
        assert!(crate::Cli::try_parse_from(["orb", "validate", "app.lolo"]).is_ok());
        assert!(crate::Cli::try_parse_from(["orb"]).is_ok());
    }

    #[test]
    fn every_reserved_word_is_a_clap_subcommand_or_flag() {
        use clap::CommandFactory;
        let clap = crate::Cli::command();
        for word in RESERVED_COMMANDS.iter().filter(|w| !w.starts_with('-') && **w != "help") {
            assert!(clap.find_subcommand(word).is_some(), "`{word}` is reserved but clap has no such subcommand");
        }
    }

    #[test]
    fn a_reserved_word_reaches_clap_even_with_a_broken_cli_program() {
        let (cwd, store) = project("reserved-broken", "cli");
        with_broken_program(&cwd);
        let lines: [&[&str]; 6] =
            [&["update"], &["add", "@x/y"], &["remove", "@x/y"], &["list"], &["--version"], &["--help"]];
        for args in lines {
            assert_eq!(resolve_program_command(&words(args), &cwd, &store).unwrap().map(|c| c.command), None, "{args:?}");
        }
    }

    #[test]
    fn control_a_non_reserved_word_with_a_broken_cli_program_surfaces_the_error() {
        let (cwd, store) = project("nonreserved-broken", "cli");
        with_broken_program(&cwd);
        let error = resolve_program_command(&words(&["fix"]), &cwd, &store).unwrap_err().to_string();
        assert!(error.contains("fake-broken.lolo"), "{error}");
    }

    #[test]
    fn a_program_declaring_a_reserved_word_is_refused() {
        let (cwd, store) = project("reserved-declared", "cli");
        with_programs(&cwd, &["cli-validate"]);
        assert_eq!(
            routed(&["echo", "hi"], &cwd, &store),
            Some(("echo".into(), words(&["hi"]))),
            "other commands still resolve"
        );
        let error = resolve_program_command(&words(&["nope"]), &cwd, &store).unwrap_err().to_string();
        assert!(error.contains("`cli-validate`") && error.contains("`validate` is a reserved"), "{error}");
        assert_eq!(routed(&["validate", "a.lolo"], &cwd, &store), None, "the reserved word itself still reaches clap");
    }

    #[test]
    fn a_hook_under_a_reserved_word_is_reserved_and_a_lookalike_is_not() {
        assert_eq!(reserved_word_of("cli-validate"), Some("validate"));
        assert_eq!(reserved_word_of("cli-list-foo"), Some("list"));
        assert_eq!(reserved_word_of("cli-lister"), None);
        assert_eq!(reserved_word_of("cli-status"), None);
        assert_eq!(reserved_word_of("validate"), None);
    }

    const LISTENER: &str = r#"app fake-listener [
  version: "1.0.0",
  net: { "127.0.0.1:0": [listen] },
  description: "Listens on a free local port and moves to serving once it accepts a connection."
]

orbital ListenerOrbital {
  entity Listener [runtime] {
    id : string!
  }

  trait Listen -> Listener {
    initial: booting

    state booting {
      INIT -> booting
        (os/tcp-listen "127.0.0.1" 0 { key: "listener" emit: { success: LISTENING failure: LISTEN_FAILED onMessage: ACCEPTED } })
      LISTENING -> listening
        (emit BOUND { port: @payload.port })
      LISTEN_FAILED -> booting
    }

    state listening {
      INIT -> listening
      ACCEPTED -> serving
    }

    state serving {
      INIT -> serving
      ACCEPTED -> serving
    }

    emits {
      LISTENING { sock : number address : string port : number key : string }
      LISTEN_FAILED { error : string kind : string }
      ACCEPTED { sock : number peer : string }
      BOUND -> external { port : number! }
    }
  }

  page boot as ListenerStart -> Listen
}
"#;

    fn run_fixture(name: &str, source: &str) -> PathBuf {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = std::env::temp_dir().join(format!("orb-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.lolo"));
        std::fs::write(&path, source).unwrap();
        path
    }

    fn bound_port(runtime: &orbital_shell_rust::host::Host) -> u16 {
        let emits: Vec<_> = runtime.trace().into_iter().flat_map(|t| t.outcomes).flat_map(|o| o.emits).collect();
        let bound = emits.iter().find(|e| e.event == "BOUND").unwrap_or_else(|| panic!("no BOUND emit in {emits:?}"));
        bound.payload["port"].as_f64().filter(|p| p.fract() == 0.0 && (1.0..=65535.0).contains(p)).map(|p| p as u16).expect("a port")
    }

    #[test]
    fn run_holds_a_listening_program_open_until_stopped() {
        bounded(run_holds_a_listening_program_body);
    }

    /// Run `body` on a thread of its own and fail if it has not finished within 30 s, so a blocked listener, accept or
    /// lock fails the test instead of hanging the binary.
    fn bounded(body: fn()) {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(body);
            let _ = done.send(outcome.is_ok());
        });
        match finished.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(true) => {}
            Ok(false) => panic!("the bounded body failed (its panic is printed above)"),
            Err(_) => panic!("the bounded body did not finish within 30 s"),
        }
    }

    fn run_holds_a_listening_program_body() {
        let program = run_fixture("listener", LISTENER);
        let runtime = boot_run(&program, true).unwrap();
        assert_eq!(runtime.open_calls(), vec!["listener".to_string()], "{}", serde_json::to_string(&runtime.trace()).unwrap());
        let port = bound_port(&runtime);
        let (stop, stopped) = std::sync::mpsc::channel();
        let held = std::thread::spawn({
            let runtime = runtime.clone();
            move || hold(&runtime, &stopped)
        });
        let _conn = std::net::TcpStream::connect(("127.0.0.1", port)).expect("the listener accepts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runtime.get_state("Listen", RUN_ENTITY).map(|s| s.current_state) != Some("serving".to_string()) {
            assert!(std::time::Instant::now() < deadline, "the accepted connection reached the program");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!held.is_finished(), "an open listener keeps the run alive");
        stop.send(()).unwrap();
        assert_eq!(held.join().unwrap(), RunEnd::Stopped);
        assert!(runtime.open_calls().is_empty(), "stopping cancels the open stream");
    }

    #[test]
    fn control_run_of_a_program_with_no_stream_ends_after_init() {
        bounded(control_run_body);
    }

    fn control_run_body() {
        let program = run_fixture("idle", ECHO);
        let runtime = boot_run(&program, false).unwrap();
        let (_stop, stopped) = std::sync::mpsc::channel();
        assert_eq!(hold(&runtime, &stopped), RunEnd::Idle);
    }

    /// `relay: false` is G-ORB-472: `SplitCli` handles `cli-split` while only `SplitEngine` emits its answer.
    fn split_program(relay: bool) -> String {
        let relay_arm = if relay { "\n      RELAYED -> ready\n        (emit SPLIT_ANSWERED { status: @payload.status, body: @payload.body })" } else { "" };
        let relay_listen = if relay {
            "\n      SplitEngine.SPLIT_ANSWERED -> RELAYED\n      RELAYED { status : number!  body : string! }"
        } else {
            ""
        };
        format!(
            r#"app fake-split [
  version: "1.0.0",
  inbound: ["cli-split"],
  description: "Hands its call to an engine trait that answers."
]

orbital SplitOrbital {{
  entity SplitRun [runtime] {{
    id : string!
  }}

  trait SplitCli -> SplitRun {{
    initial: ready

    state ready {{
      INIT -> ready
      SPLIT_CALLED -> ready
        (emit SPLIT_START {{}}){relay_arm}
    }}

    emits {{
      SPLIT_ANSWERED -> internal {{ status : number! body : string! }}
      SPLIT_START -> external {{}}
    }}

    listens {{
      SPLIT_CALLED -> inbound "cli-split" answers SPLIT_ANSWERED {{
        headers : Map string string
        rawBody : string
      }}{relay_listen}
    }}
  }}

  trait SplitEngine -> SplitRun {{
    initial: ready

    state ready {{
      INIT -> ready
      STARTED -> ready
        (emit SPLIT_ANSWERED {{ status: 200, body: "engine done" }})
    }}

    emits {{
      SPLIT_ANSWERED -> external {{ status : number! body : string! }}
    }}

    listens {{
      SplitCli.SPLIT_START -> STARTED
      STARTED {{}}
    }}
  }}

  page boot as SplitStart -> SplitCli, SplitEngine
}}
"#
        )
    }

    #[test]
    fn a_command_whose_answer_only_a_sibling_trait_emits_answers_unanswered_instead_of_hanging() {
        bounded(|| {
            let program = run_fixture("split", &split_program(false));
            let CommandAnswer { code, body: out, .. } = run_command_program(&program, "split", &[], Path::new(".")).unwrap();
            assert_eq!(code, 1, "{out}");
            let body: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
            assert_eq!(
                (body["code"].as_str(), body["trait"].as_str(), body["event"].as_str()),
                (Some("inbound-unanswered"), Some("SplitCli"), Some("SPLIT_CALLED"))
            );
        });
    }

    #[test]
    fn control_a_command_that_relays_its_engines_answer_answers_it() {
        bounded(|| {
            let program = run_fixture("split-relay", &split_program(true));
            let CommandAnswer { code, body: out, .. } = run_command_program(&program, "split", &[], Path::new(".")).unwrap();
            assert_eq!((code, out.as_str()), (0, "engine done"));
        });
    }

    #[test]
    fn control_the_same_hook_outside_the_cli_topic_is_not_a_command() {
        let (cwd, store) = project("other-topic", "infra");
        assert_eq!(find_command_program("echo", &cwd, &store).unwrap(), None);
    }
}
