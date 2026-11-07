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
    "run",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

/// The reserved word a hook would take, if any.
fn reserved_word_of(hook: &str) -> Option<&'static str> {
    let command = hook.strip_prefix("cli-")?;
    RESERVED_COMMANDS.iter().copied().find(|word| {
        command == *word
            || command
                .strip_prefix(*word)
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

/// The package words: std's packages program answers all four on the one hook `cli-packages`, and reads the word
/// itself from the first argument.
const PACKAGE_WORDS: &[&str] = &["update", "add", "remove", "list"];
const PACKAGES_COMMAND: &str = "packages";

fn package_word(args: &[String]) -> bool {
    args.first().is_some_and(|first| PACKAGE_WORDS.contains(&first.as_str()))
}

/// What bare `orb` runs: the program declaring `cli-launchpad`, else `orb`'s help.
const BARE_COMMAND: &str = "launchpad";

/// The commands `args` (argv after the binary) may name, in the order they are tried: `w1 w2 …` is `w1-w2` with
/// `…`, then `w1` with `w2 …`; a leading flag names none; no words name [`BARE_COMMAND`].
fn command_candidates(args: &[String]) -> Vec<(String, &[String])> {
    let is_word = |a: &String| !a.starts_with('-');
    match args {
        [first, second, rest @ ..] if is_word(first) && is_word(second) => {
            vec![
                (format!("{first}-{second}"), rest),
                (first.clone(), &args[1..]),
            ]
        }
        [first, rest @ ..] if is_word(first) => vec![(first.clone(), rest)],
        [] => vec![(BARE_COMMAND.to_string(), args)],
        _ => Vec::new(),
    }
}

/// The installed program `args` names, if any (see [`command_candidates`]); `None` leaves the line to `orb`'s own
/// subcommands. A reserved word goes to them without reading any package.
pub(crate) fn resolve_program_command(
    args: &[String],
    cwd: &Path,
    user_store: &Path,
) -> anyhow::Result<Option<ProgramCommand>> {
    if args
        .first()
        .is_some_and(|first| RESERVED_COMMANDS.contains(&first.as_str()))
    {
        return Ok(None);
    }
    let candidates = if package_word(args) {
        vec![(PACKAGES_COMMAND.to_string(), args)]
    } else {
        command_candidates(args)
    };
    if candidates.is_empty() {
        return Ok(None);
    }
    let programs = cli_programs(cwd, user_store)?;
    for (command, argv) in &candidates {
        if let Some(program) = programs.declaring(&hook_name(command))? {
            return Ok(Some(ProgramCommand {
                program,
                command: command.clone(),
                argv: argv.to_vec(),
            }));
        }
    }
    programs.unanswered(
        &candidates
            .iter()
            .map(|(command, _)| hook_name(command))
            .collect::<Vec<_>>(),
    )?;
    Ok(None)
}

/// The refusal for a command word no installed program declares and the bootloader does not carry: orb's commands
/// live in std, so a missing std reads as a missing command, never as clap's unknown subcommand.
pub(crate) fn not_installed(args: &[String]) -> Option<String> {
    use clap::CommandFactory;
    let word = args.first().filter(|w| !w.starts_with('-'))?;
    if package_word(args) {
        return Some(format!(
            "ORB_COMMAND_NOT_INSTALLED: `orb {word}` is not a bootloader command and no installed program declares `{}`; orb's commands ship in @almadar/std, which the installer puts in the user store",
            hook_name(PACKAGES_COMMAND)
        ));
    }
    if crate::Cli::command().find_subcommand(word).is_some() {
        return None;
    }
    Some(format!(
        "ORB_COMMAND_NOT_INSTALLED: `orb {word}` is not a bootloader command and no installed program declares `{}`; orb's commands ship in @almadar/std — run `orb update` to install it",
        hook_name(word)
    ))
}

/// The installed program in a `cli` topic that declares the hook `cli-<command>`, from the behavior packages resolved
/// from `cwd` and then `user_store`. More than one is an error that names them.
#[cfg(test)]
fn find_command_program(
    command: &str,
    cwd: &Path,
    user_store: &Path,
) -> anyhow::Result<Option<PathBuf>> {
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
        let mut found: Vec<&PathBuf> = self
            .declared
            .iter()
            .filter(|(_, hooks)| hooks.iter().any(|h| h == hook))
            .map(|(p, _)| p)
            .collect();
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop().cloned()),
            _ => anyhow::bail!(
                "more than one installed program declares `{hook}`: {}",
                found
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Fails naming every unusable program, any of which may be the one meant to declare one of `hooks`.
    fn unanswered(&self, hooks: &[String]) -> anyhow::Result<()> {
        if self.unusable.is_empty() {
            return Ok(());
        }
        let hooks = hooks
            .iter()
            .map(|hook| format!("`{hook}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        anyhow::bail!(
            "no installed program declares {hooks}, and these `cli` programs declare no command:\n{}",
            self.unusable.join("\n")
        )
    }
}

fn cli_programs(cwd: &Path, user_store: &Path) -> anyhow::Result<CliPrograms> {
    let packages = orbital_core::behavior_packages::discover(cwd, user_store)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut programs = CliPrograms {
        declared: Vec::new(),
        unusable: Vec::new(),
    };
    for package in &packages {
        let files = orbital_core::behavior_index::indexed_files(package)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for file in files.iter().filter(|file| file.topic() == "cli") {
            let Some(path) =
                orbital_core::behavior_index::lolo_path_of(package, file).filter(|p| p.is_file())
            else {
                continue;
            };
            let source = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            let hooks = match orbital_lolo::parse_app_header(&source) {
                Ok(header) => header.inbound,
                Err(inner) => {
                    programs.unusable.push(
                        orbital_lolo::LoloError::Parse {
                            path: path.display().to_string(),
                            inner,
                        }
                        .render(),
                    );
                    continue;
                }
            };
            if let Some((hook, word)) = hooks
                .iter()
                .find_map(|hook| reserved_word_of(hook).map(|word| (hook, word)))
            {
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
pub(crate) fn run_command_program(
    program: &Path,
    command: &str,
    argv: &[String],
    cwd: &Path,
) -> anyhow::Result<CommandAnswer> {
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
    let runtime = boot_command_program(program, anchors)?;
    Ok(answer_command(&runtime, command, argv, cwd))
}

fn boot_command_program(
    program: &Path,
    anchors: Option<orbital_core::runtime::os_host::OsAnchors>,
) -> anyhow::Result<std::sync::Arc<orbital_shell_rust::host::Host>> {
    let resolved = crate::commands::dev::resolve_command_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(
        crate::commands::program_host::ProgramHost,
    ));
    let config = orbital_shell_rust::host::HostConfig {
        substrate: Some(host),
        anchors,
        ..orbital_shell_rust::host::HostConfig::default()
    };
    orbital_shell_rust::host::boot::boot_file_with(
        &resolved.path(),
        &std::env::vars().collect(),
        false,
        config,
    )
}

fn answer_command(
    runtime: &orbital_shell_rust::host::Host,
    command: &str,
    argv: &[String],
    cwd: &Path,
) -> CommandAnswer {
    let body = serde_json::json!({ "argv": argv, "cwd": cwd.display().to_string() }).to_string();
    let answer = runtime.deliver_inbound_awaiting(
        &hook_name(command),
        [("content-type", "application/json")],
        &body,
        None,
    );
    CommandAnswer {
        code: exit_code(answer.status),
        body: answer.body,
        stderr: answer.stderr,
        headers: answer.headers.unwrap_or_default(),
    }
}

/// The daily updates-available notice: after a command, the installed program declaring `cli-notice`
/// (`std-orb-update-notice`) is run with `argv` (`record` after `orb update` stamps the cache) and its answer's
/// `stderr` line printed. The program owns the cache, the opt-outs and the registry lookup; a missing or failing
/// notice never fails the command it follows.
/// The notice's arguments after a command that answered 0: `record` after a command that changed the installed packages
/// (`update`, `add`, `remove`), so the notice's update record is stamped fresh instead of announcing what was just
/// installed; nothing otherwise.
pub(crate) fn notice_argv(argv: &[String]) -> Vec<String> {
    match argv.first().map(String::as_str) {
        Some("update" | "add" | "remove") => vec!["record".to_string()],
        _ => Vec::new(),
    }
}

pub(crate) fn update_notice(argv: &[String], cwd: &Path, user_store: &Path) {
    const COMMAND: &str = "notice";
    let program = match cli_programs(cwd, user_store).and_then(|programs| programs.declaring(&hook_name(COMMAND))) {
        Ok(Some(program)) => program,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(target: orbital_core::logging::ns::CLI, %error, "update notice: no program resolved");
            return;
        }
    };
    match run_command_program(&program, COMMAND, argv, cwd) {
        Ok(answer) if answer.code == 0 => {
            if let Some(text) = answer.stderr.as_deref().filter(|text| !text.is_empty()) {
                eprintln!("{text}");
            }
        }
        Ok(answer) => tracing::warn!(target: orbital_core::logging::ns::CLI, code = answer.code, body = %answer.body, "update notice refused"),
        Err(error) => tracing::warn!(target: orbital_core::logging::ns::CLI, %error, "update notice failed"),
    }
}

/// `orb run <program>`, booted: the program on the substrate host, `INIT` mounted on every trait.
pub(crate) fn boot_run(
    program: &Path,
    trace: bool,
) -> anyhow::Result<std::sync::Arc<orbital_shell_rust::host::Host>> {
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(
        crate::commands::program_host::ProgramHost,
    ));
    let runtime = orbital_shell_rust::host::boot::boot_file(
        &resolved.path(),
        &std::env::vars().collect(),
        trace,
        Some(host),
    )?;
    orbital_shell_rust::host::run::mount_init(
        &runtime,
        orbital_shell_rust::host::run::ProtocolSide::Process,
    )
    .map_err(|e| anyhow::anyhow!("INIT: {e}"))?;
    Ok(runtime)
}

/// `orb run <program>`: boot it, hold it while it has open calls, stop cleanly on SIGINT/SIGTERM.
pub(crate) fn run_program(program: &Path) -> anyhow::Result<()> {
    let stop = stop_on_signal()?;
    let runtime = boot_run(program, false)?;
    hold(&runtime, &stop);
    Ok(())
}

/// Whether the program declares an endpoint the server side serves (a page whose protocol is served by the server,
/// `http` today), so it is served as written instead of composed with std-http-server.
pub(crate) fn declares_server_endpoint(schema: &orbital_core::OrbitalSchema) -> bool {
    schema.orbitals.iter().flat_map(|o| o.pages.iter().flatten()).any(|page| match page {
        orbital_core::schema::PageRef::Inline(page) => page.protocol.as_deref().and_then(orbital_core::protocols::protocol_after_preflight)
            .is_some_and(|spec| spec.side == orbital_core::protocols::ProtocolSide::Server),
        _ => false,
    })
}

/// Run the program a cli command handed back (`orb-run` in its answer's headers), as the bootloader runs every program.
/// `orb-run: server` boots the composed app in the body as `orb serve` does: its lazy files beside `orb-app`, `INIT` mounted on the server side, held until a signal.
/// Takes the answer by value: its body is the whole resolved program as text, freed once parsed rather than held for
/// the server's lifetime.
pub(crate) fn run_handoff(answer: CommandAnswer) -> anyhow::Result<()> {
    let CommandAnswer { body, headers, .. } = answer;
    let header = |name: &str| headers.get(name).map(String::as_str);
    match header("orb-run") {
        Some("server") => {}
        other => {
            anyhow::bail!("orb-run: no hand-off named {other:?}; the bootloader runs `server`")
        }
    }
    let app = header("orb-app").ok_or_else(|| {
        anyhow::anyhow!("orb-run: server needs orb-app, the resolved app its lazy files sit beside")
    })?;
    let schema: orbital_core::OrbitalSchema = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("orb-run: server: the body is not a program: {e}"))?;
    drop(body);
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(
        crate::commands::program_host::ProgramHost,
    ));
    let stop = stop_on_signal()?;
    let runtime = orbital_shell_rust::host::boot::boot_app(
        schema,
        Path::new(app),
        host,
        &std::env::vars().collect(),
        false,
    )?;
    orbital_shell_rust::host::run::mount_init(
        &runtime,
        orbital_shell_rust::host::run::ProtocolSide::Server,
    )
    .map_err(|e| anyhow::anyhow!("INIT: {e}"))?;
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| {
            crate::lower_lolo_file(path)
        });
        let root =
            std::env::temp_dir().join(format!("orb-program-command-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("project/node_modules/@fake/cli");
        let dir = package.join("behaviors/lolo").join(topic).join("atoms");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{ "name": "@fake/cli", "version": "1.0.0", "orb": { "prefix": "fake", "source": "behaviors/lolo", "language": "*" } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("fake-echo.lolo"), ECHO).unwrap();
        let core = root.join("project/node_modules/@almadar/core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(
            core.join("package.json"),
            r#"{ "name": "@almadar/core", "version": "10.1.0", "orbLanguage": { "version": "1.0.0" } }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("store")).unwrap();
        (root.join("project"), root.join("store"))
    }

    #[test]
    fn a_program_with_an_http_page_declares_its_server_and_a_routes_only_program_does_not() {
        let program = |pages: serde_json::Value| -> orbital_core::OrbitalSchema {
            serde_json::from_value(serde_json::json!({ "name": "p", "orbitals": [{ "name": "O", "entity": { "name": "E", "fields": [] }, "traits": [], "pages": pages }] })).unwrap()
        };
        let http = program(serde_json::json!([{ "name": "Api", "path": "", "protocol": "http", "address": { "host": "127.0.0.1", "port": 8080, "path": "/" }, "traits": [{ "ref": "T" }] }]));
        assert!(declares_server_endpoint(&http));
        let routes = program(serde_json::json!([{ "name": "Home", "path": "/", "traits": [{ "ref": "T" }] }]));
        assert!(!declares_server_endpoint(&routes), "control: a route is the client's");
        let boot = program(serde_json::json!([{ "name": "Start", "path": "", "protocol": "boot", "traits": [{ "ref": "T" }] }]));
        assert!(!declares_server_endpoint(&boot), "control: a boot page is not a server endpoint");
    }

    #[test]
    fn the_status_class_is_the_exit_code() {
        assert_eq!([200, 201, 299].map(exit_code), [0, 0, 0]);
        assert_eq!([400, 404, 422].map(exit_code), [2, 2, 2]);
        assert_eq!([500, 503, 100, 302].map(exit_code), [1, 1, 1, 1]);
    }

    #[test]
    fn the_update_notice_runs_the_installed_notice_program_and_is_silent_without_one() {
        let (cwd, store) = project("notice", "cli");
        assert!(
            find_command_program("notice", &cwd, &store).unwrap().is_none(),
            "control: no program declares cli-notice yet"
        );
        update_notice(&["record".into()], &cwd, &store);
        with_programs(&cwd, &["cli-notice"]);
        let program = find_command_program("notice", &cwd, &store).unwrap().expect("the notice program is found");
        let answer = run_command_program(&program, "notice", &["record".into()], &cwd).unwrap();
        assert_eq!((answer.code, answer.body.as_str()), (0, "record"));
        update_notice(&["record".into()], &cwd, &store);
    }

    #[test]
    fn a_command_programs_host_is_freed_once_it_has_answered() {
        let (cwd, store) = project("freed", "cli");
        let program = find_command_program("echo", &cwd, &store).unwrap().expect("the cli program is found");
        let runtime = boot_command_program(&program, None).unwrap();
        let host = std::sync::Arc::downgrade(&runtime);
        let answer = answer_command(&runtime, "echo", &["hi".into()], &cwd);
        assert_eq!(answer.body, "hi");
        drop(runtime);
        assert!(host.upgrade().is_none(), "nothing may keep the command program's host alive after it answered");
    }

    #[test]
    fn control_a_held_command_host_is_still_alive() {
        let (cwd, store) = project("held", "cli");
        let program = find_command_program("echo", &cwd, &store).unwrap().expect("the cli program is found");
        let runtime = boot_command_program(&program, None).unwrap();
        let host = std::sync::Arc::downgrade(&runtime);
        answer_command(&runtime, "echo", &["hi".into()], &cwd);
        assert!(host.upgrade().is_some());
    }

    #[test]
    fn a_cli_program_declaring_the_hook_is_the_command_and_runs_with_its_arguments() {
        let (cwd, store) = project("found", "cli");
        let program = find_command_program("echo", &cwd, &store)
            .unwrap()
            .expect("the cli program is found");
        assert!(
            program.ends_with("cli/atoms/fake-echo.lolo"),
            "{}",
            program.display()
        );
        assert_eq!(
            find_command_program("nope", &cwd, &store).unwrap(),
            None,
            "control: no program declares cli-nope"
        );
        let CommandAnswer {
            code, body: out, ..
        } = run_command_program(&program, "echo", &["hello".into(), "world".into()], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hello world"));
        let CommandAnswer {
            code, body: out, ..
        } = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!(
            (code, out.as_str()),
            (2, "usage: orb echo <words>"),
            "a refused input is exit 2 with its message"
        );
    }

    fn with_stderr_program(cwd: &Path) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        let source = ECHO
            .replace("cli-echo", "cli-warn")
            .replace(
                r#"{ status: 400, body: "usage: orb echo <words>" }"#,
                r#"{ status: 400, body: "", stderr: "usage: orb warn <words>" }"#,
            )
            .replace(
                "{ status : number! body : string! }",
                "{ status : number! body : string! stderr : string }",
            );
        std::fs::write(dir.join("fake-warn.lolo"), source).unwrap();
    }

    fn written(answer: &CommandAnswer) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        answer.write_to(&mut out, &mut err).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn an_answer_stderr_reaches_stderr_and_its_status_is_the_exit_code() {
        let (cwd, store) = project("stderr", "cli");
        with_stderr_program(&cwd);
        let program = find_command_program("warn", &cwd, &store)
            .unwrap()
            .expect("the stderr program is found");
        let refused = run_command_program(&program, "warn", &[], &cwd).unwrap();
        assert_eq!(
            refused,
            CommandAnswer {
                code: 2,
                body: String::new(),
                stderr: Some("usage: orb warn <words>".into()),
                headers: Default::default()
            }
        );
        assert_eq!(
            written(&refused),
            (String::new(), "usage: orb warn <words>\n".into())
        );
        let answered = run_command_program(&program, "warn", &["hi".into()], &cwd).unwrap();
        assert_eq!(
            answered,
            CommandAnswer {
                code: 0,
                body: "hi".into(),
                stderr: None,
                headers: Default::default()
            },
            "an answer that omits stderr has none"
        );
        assert_eq!(written(&answered), ("hi\n".into(), String::new()));
    }

    #[test]
    fn control_an_answer_without_stderr_prints_its_body_on_stdout_only() {
        let (cwd, store) = project("no-stderr", "cli");
        let program = find_command_program("echo", &cwd, &store)
            .unwrap()
            .expect("the cli program is found");
        let refused = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!(
            refused,
            CommandAnswer {
                code: 2,
                body: "usage: orb echo <words>".into(),
                stderr: None,
                headers: Default::default()
            }
        );
        assert_eq!(
            written(&refused),
            ("usage: orb echo <words>\n".into(), String::new())
        );
    }

    #[test]
    fn a_program_that_fails_to_lower_fails_only_its_own_command() {
        let (cwd, store) = project("lower-broken", "cli");
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        std::fs::write(
            dir.join("fake-bad.lolo"),
            ECHO.replace("cli-echo", "cli-bad")
                .replace("status : number!", "status : Nope!"),
        )
        .unwrap();
        let echo = find_command_program("echo", &cwd, &store)
            .unwrap()
            .expect("the healthy command still resolves");
        let CommandAnswer {
            code, body: out, ..
        } = run_command_program(&echo, "echo", &["hi".into()], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hi"));
        let bad = find_command_program("bad", &cwd, &store)
            .unwrap()
            .expect("the broken program's hook is still readable");
        assert!(
            bad.ends_with("cli/atoms/fake-bad.lolo"),
            "{}",
            bad.display()
        );
        let error = run_command_program(&bad, "bad", &[], &cwd)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("fake-bad.lolo") && error.contains("Nope"),
            "{error}"
        );
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
        let echo = find_command_program("echo", &cwd, &store)
            .unwrap()
            .expect("the healthy command still resolves");
        assert!(
            echo.ends_with("cli/atoms/fake-echo.lolo"),
            "{}",
            echo.display()
        );
        assert_eq!(
            routed(&["echo", "hi"], &cwd, &store),
            Some(("echo".into(), words(&["hi"])))
        );
        let error = find_command_program("nope", &cwd, &store)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("fake-broken.lolo") && error.contains("`cli-nope`"),
            "{error}"
        );
    }

    fn with_programs(cwd: &Path, hooks: &[&str]) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        for hook in hooks {
            std::fs::write(
                dir.join(format!("fake-{hook}.lolo")),
                ECHO.replace("cli-echo", hook),
            )
            .unwrap();
        }
    }

    fn with_broken_program(cwd: &Path) {
        let dir = cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms");
        std::fs::write(
            dir.join("fake-broken.lolo"),
            "app fake-broken [ this does not parse",
        )
        .unwrap();
    }

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn routed(args: &[&str], cwd: &Path, store: &Path) -> Option<(String, Vec<String>)> {
        resolve_program_command(&words(args), cwd, store)
            .unwrap()
            .map(|c| (c.command, c.argv))
    }

    #[test]
    fn program_first_a_word_resolves_to_the_program_declaring_it() {
        let (cwd, store) = project("program-first", "cli");
        with_programs(
            &cwd,
            &["cli-run2", "cli-behaviors", "cli-behaviors-describe"],
        );
        assert_eq!(
            routed(&["run2", "a"], &cwd, &store),
            Some(("run2".into(), words(&["a"])))
        );
        assert_eq!(
            routed(&["behaviors", "describe", "x"], &cwd, &store),
            Some(("behaviors-describe".into(), words(&["x"]))),
            "the two-word hook is tried first"
        );
        assert_eq!(
            routed(&["behaviors", "catalog"], &cwd, &store),
            Some(("behaviors".into(), words(&["catalog"])))
        );
        let command =
            resolve_program_command(&words(&["behaviors", "describe", "hi"]), &cwd, &store)
                .unwrap()
                .unwrap();
        let CommandAnswer {
            code, body: out, ..
        } = run_command_program(&command.program, &command.command, &command.argv, &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hi"));
    }

    #[test]
    #[ignore = "lands with G-ORB-487 (absolute-path ruling)"]
    fn the_longest_declared_hook_is_tried_first_whatever_its_word_count() {
        let (cwd, store) = project("longest-first", "cli");
        with_programs(
            &cwd,
            &["cli-behaviors", "cli-behaviors-vectors", "cli-behaviors-vectors-snapshot"],
        );
        assert_eq!(
            routed(&["behaviors", "vectors", "snapshot", "out"], &cwd, &store),
            Some(("behaviors-vectors-snapshot".into(), words(&["out"])))
        );
        assert_eq!(
            routed(&["behaviors", "vectors", "check"], &cwd, &store),
            Some(("behaviors-vectors".into(), words(&["check"]))),
            "control: the next shorter hook answers when the longest is not declared"
        );
        assert_eq!(
            routed(&["behaviors", "catalog"], &cwd, &store),
            Some(("behaviors".into(), words(&["catalog"])))
        );
        assert_eq!(
            routed(&["behaviors", "vectors", "snapshot", "--json"], &cwd, &store),
            Some(("behaviors-vectors-snapshot".into(), words(&["--json"]))),
            "a flag ends the words and stays argv"
        );
        assert_eq!(
            routed(&["behaviors", "--json", "vectors"], &cwd, &store),
            Some(("behaviors".into(), words(&["--json", "vectors"]))),
            "control: words after a flag are argv, never part of the hook"
        );
    }

    #[test]
    #[ignore = "lands with G-ORB-487 (absolute-path ruling)"]
    fn command_candidates_run_from_all_leading_words_down_to_one() {
        let args = words(&["a", "b", "c", "--x", "d"]);
        let found: Vec<(String, Vec<String>)> = command_candidates(&args)
            .into_iter()
            .map(|(hook, rest)| (hook, rest.to_vec()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("a-b-c".to_string(), words(&["--x", "d"])),
                ("a-b".to_string(), words(&["c", "--x", "d"])),
                ("a".to_string(), words(&["b", "c", "--x", "d"])),
            ]
        );
        assert!(command_candidates(&words(&["--x", "a"])).is_empty());
    }

    #[test]
    fn simulate_and_language_which_resolve_to_their_programs_and_clap_no_longer_has_them() {
        use clap::Parser;
        let (cwd, store) = project("route1", "cli");
        with_programs(&cwd, &["cli-simulate", "cli-language"]);
        assert_eq!(
            routed(&["simulate", "app.lolo"], &cwd, &store),
            Some(("simulate".into(), words(&["app.lolo"])))
        );
        assert_eq!(
            routed(&["language", "which"], &cwd, &store),
            Some(("language".into(), words(&["which"])))
        );
        assert!(crate::Cli::try_parse_from(["orb", "validate", "a.lolo", "--simulate"]).is_err());
        assert!(crate::Cli::try_parse_from(["orb", "language", "which"]).is_err());
    }

    #[test]
    fn a_bare_orb_runs_the_program_declaring_the_launchpad_hook() {
        let (cwd, store) = project("bare-launchpad", "cli");
        with_programs(&cwd, &["cli-launchpad"]);
        assert_eq!(routed(&[], &cwd, &store), Some(("launchpad".to_string(), Vec::new())));
        assert_eq!(routed(&["launchpad"], &cwd, &store), Some(("launchpad".to_string(), Vec::new())), "control: the word names it too");
        assert_eq!(routed(&["--help"], &cwd, &store), None, "control: a flag alone still reaches clap");
    }

    #[test]
    fn control_a_word_with_no_program_reaches_clap() {
        let (cwd, store) = project("no-program", "cli");
        assert_eq!(routed(&["validate", "app.lolo"], &cwd, &store), None);
        assert_eq!(routed(&["behaviors", "catalog"], &cwd, &store), None);
        assert_eq!(routed(&["--help"], &cwd, &store), None);
        assert_eq!(
            routed(&["--list"], &cwd, &store),
            None,
            "a leading flag names no program"
        );
        assert_eq!(routed(&[], &cwd, &store), None);
    }

    #[test]
    fn the_agent_words_are_not_clap_subcommands() {
        use clap::Parser;
        for args in [
            &["orb", "fix", "a.orb"][..],
            &["orb", "login"],
            &["orb", "eval", "--all"],
            &["orb", "trace"],
            &["orb", "--list"],
        ] {
            let error = crate::Cli::try_parse_from(args)
                .err()
                .unwrap_or_else(|| panic!("{args:?} parsed"));
            assert!(
                matches!(
                    error.kind(),
                    clap::error::ErrorKind::InvalidSubcommand
                        | clap::error::ErrorKind::UnknownArgument
                ),
                "{args:?}: {:?}",
                error.kind()
            );
        }
    }

    #[test]
    fn a_word_no_program_declares_and_clap_lacks_is_a_named_not_installed_error() {
        let message = not_installed(&words(&["validate", "a.lolo"])).expect("refused");
        assert!(
            message.starts_with("ORB_COMMAND_NOT_INSTALLED")
                && message.contains("`cli-validate`")
                && message.contains("orb update"),
            "{message}"
        );
        assert_eq!(
            not_installed(&words(&["run", "a.lolo"])),
            None,
            "control: a bootloader command reaches clap"
        );
        assert_eq!(
            not_installed(&words(&["--version"])),
            None,
            "control: a flag reaches clap"
        );
        assert_eq!(
            not_installed(&[]),
            None,
            "control: no words prints the help"
        );
    }

    #[test]
    fn control_a_kept_subcommand_still_parses() {
        use clap::Parser;
        assert!(crate::Cli::try_parse_from(["orb", "run", "app.lolo"]).is_ok());
        assert!(crate::Cli::try_parse_from(["orb"]).is_ok());
    }

    #[test]
    fn every_reserved_word_is_a_clap_subcommand_or_flag() {
        use clap::CommandFactory;
        let clap = crate::Cli::command();
        for word in RESERVED_COMMANDS
            .iter()
            .filter(|w| !w.starts_with('-') && **w != "help")
        {
            assert!(
                clap.find_subcommand(word).is_some(),
                "`{word}` is reserved but clap has no such subcommand"
            );
        }
    }

    #[test]
    fn a_reserved_word_reaches_clap_even_with_a_broken_cli_program() {
        let (cwd, store) = project("reserved-broken", "cli");
        with_broken_program(&cwd);
        let lines: [&[&str]; 2] = [
            &["--version"],
            &["--help"],
        ];
        for args in lines {
            assert_eq!(
                resolve_program_command(&words(args), &cwd, &store)
                    .unwrap()
                    .map(|c| c.command),
                None,
                "{args:?}"
            );
        }
    }

    #[test]
    fn control_a_non_reserved_word_with_a_broken_cli_program_surfaces_the_error() {
        let (cwd, store) = project("nonreserved-broken", "cli");
        with_broken_program(&cwd);
        let error = resolve_program_command(&words(&["fix"]), &cwd, &store)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fake-broken.lolo"), "{error}");
    }

    #[test]
    fn a_program_declaring_a_reserved_word_is_refused() {
        let (cwd, store) = project("reserved-declared", "cli");
        with_programs(&cwd, &["cli-run"]);
        assert_eq!(
            routed(&["echo", "hi"], &cwd, &store),
            Some(("echo".into(), words(&["hi"]))),
            "other commands still resolve"
        );
        let error = resolve_program_command(&words(&["nope"]), &cwd, &store)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("`cli-run`") && error.contains("`run` is a reserved"),
            "{error}"
        );
        assert_eq!(
            routed(&["run"], &cwd, &store),
            None,
            "the reserved word itself still reaches clap"
        );
    }

    #[test]
    fn each_package_word_reaches_the_packages_program_with_its_word_in_argv() {
        let (cwd, store) = project("package-words", "cli");
        with_broken_program(&cwd);
        std::fs::remove_file(cwd.join("node_modules/@fake/cli/behaviors/lolo/cli/atoms/fake-broken.lolo")).unwrap();
        with_programs(&cwd, &["cli-packages"]);
        for (args, argv) in [
            (&["update"][..], &["update"][..]),
            (&["add", "@x/y"], &["add", "@x/y"]),
            (&["remove", "@x/y"], &["remove", "@x/y"]),
            (&["list", "--json"], &["list", "--json"]),
        ] {
            assert_eq!(routed(args, &cwd, &store), Some(("packages".into(), words(argv))), "{args:?}");
        }
        assert_eq!(routed(&["lists"], &cwd, &store), None, "control: a lookalike word is not a package word");
    }

    #[test]
    fn a_hook_under_a_reserved_word_is_reserved_and_a_lookalike_is_not() {
        assert_eq!(reserved_word_of("cli-run"), Some("run"));
        assert_eq!(
            reserved_word_of("cli-validate"),
            None,
            "validate is a std program now"
        );
        assert_eq!(reserved_word_of("cli-run-foo"), Some("run"));
        assert_eq!(reserved_word_of("cli-runner"), None);
        assert_eq!(reserved_word_of("cli-status"), None);
        assert_eq!(reserved_word_of("run"), None);
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
        orbital_compiler::loader::register_source_lowerer(|path, _, _| {
            crate::lower_lolo_file(path)
        });
        let dir = std::env::temp_dir().join(format!("orb-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.lolo"));
        std::fs::write(&path, source).unwrap();
        path
    }

    fn bound_port(runtime: &orbital_shell_rust::host::Host) -> u16 {
        let emits: Vec<_> = runtime
            .trace()
            .into_iter()
            .flat_map(|t| t.outcomes)
            .flat_map(|o| o.emits)
            .collect();
        let bound = emits
            .iter()
            .find(|e| e.event == "BOUND")
            .unwrap_or_else(|| panic!("no BOUND emit in {emits:?}"));
        bound.payload["port"]
            .as_f64()
            .filter(|p| p.fract() == 0.0 && (1.0..=65535.0).contains(p))
            .map(|p| p as u16)
            .expect("a port")
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
        assert_eq!(
            runtime.open_calls().iter().map(|key| orbital_core::runtime::resumable::program_call_key(key)).collect::<Vec<_>>(),
            vec!["listener"],
            "{}",
            serde_json::to_string(&runtime.trace()).unwrap()
        );
        let port = bound_port(&runtime);
        let (stop, stopped) = std::sync::mpsc::channel();
        let held = std::thread::spawn({
            let runtime = runtime.clone();
            move || hold(&runtime, &stopped)
        });
        let _conn =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("the listener accepts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runtime
            .get_state("Listen", RUN_ENTITY)
            .map(|s| s.current_state)
            != Some("serving".to_string())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the accepted connection reached the program"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!held.is_finished(), "an open listener keeps the run alive");
        stop.send(()).unwrap();
        assert_eq!(held.join().unwrap(), RunEnd::Stopped);
        assert!(
            runtime.open_calls().is_empty(),
            "stopping cancels the open stream"
        );
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
        let relay_arm = if relay {
            "\n      RELAYED -> ready\n        (emit SPLIT_ANSWERED { status: @payload.status, body: @payload.body })"
        } else {
            ""
        };
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
            let CommandAnswer {
                code, body: out, ..
            } = run_command_program(&program, "split", &[], Path::new(".")).unwrap();
            assert_eq!(code, 1, "{out}");
            let body: serde_json::Value =
                serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
            assert_eq!(
                (
                    body["code"].as_str(),
                    body["trait"].as_str(),
                    body["event"].as_str()
                ),
                (
                    Some("inbound-unanswered"),
                    Some("SplitCli"),
                    Some("SPLIT_CALLED")
                )
            );
        });
    }

    #[test]
    fn a_command_whose_sibling_trait_effect_fails_answers_unanswered_naming_that_failure() {
        bounded(|| {
            let failing = split_program(false)
                .replace("id : string!\n  }", "id : string!\n    n : number = 0\n  }")
                .replace(
                    "      STARTED -> ready\n",
                    "      STARTED -> ready\n        (set @entity.n (array/first [1] 0))\n",
                );
            let program = run_fixture("split-fail", &failing);
            let CommandAnswer { code, body: out, .. } =
                run_command_program(&program, "split", &[], Path::new(".")).unwrap();
            assert_eq!(code, 1, "{out}");
            let body: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
            assert_eq!(body["code"].as_str(), Some("inbound-unanswered"), "{out}");
            let failures = body["failures"].as_array().unwrap_or_else(|| panic!("no failures: {out}"));
            assert!(failures.iter().any(|f| f.as_str().is_some_and(|f| f.starts_with("SplitEngine:"))), "{out}");
            let quiet = run_fixture("split-fail-control", &split_program(false));
            let CommandAnswer { body: control, .. } =
                run_command_program(&quiet, "split", &[], Path::new(".")).unwrap();
            assert!(!control.contains("failures"), "control: a clean cascade names no failure: {control}");
        });
    }

    #[test]
    fn control_a_command_that_relays_its_engines_answer_answers_it() {
        bounded(|| {
            let program = run_fixture("split-relay", &split_program(true));
            let CommandAnswer {
                code, body: out, ..
            } = run_command_program(&program, "split", &[], Path::new(".")).unwrap();
            assert_eq!((code, out.as_str()), (0, "engine done"));
        });
    }

    #[test]
    fn control_the_same_hook_outside_the_cli_topic_is_not_a_command() {
        let (cwd, store) = project("other-topic", "infra");
        assert_eq!(find_command_program("echo", &cwd, &store).unwrap(), None);
    }

    #[test]
    fn a_package_change_asks_the_notice_to_record_and_any_other_command_asks_nothing() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for changed in [&["update", "--global"][..], &["add", "@x/y@1.0.0"], &["remove", "@x/y"], &["update", "--urgent"]] {
            assert_eq!(notice_argv(&words(changed)), words(&["record"]), "{changed:?}");
        }
        assert!(notice_argv(&words(&["list", "--json"])).is_empty(), "control: listing changes nothing");
        assert!(notice_argv(&words(&["validate", "a.lolo"])).is_empty());
        assert!(notice_argv(&[]).is_empty(), "edge: no arguments");
    }
}
