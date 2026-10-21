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
    "parse", "lower", "validate", "compile", "emit", "evaluate", "update", "add", "remove", "list", "run", "help", "--help",
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
pub(crate) fn find_command_program(command: &str, cwd: &Path, user_store: &Path) -> anyhow::Result<Option<PathBuf>> {
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
            let hooks = match orbital_lolo::parse_str(&source) {
                Ok(program) => program.app.inbound,
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
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let runtime = orbital_shell_rust::host::boot::boot_file(&resolved, false, &std::env::vars().collect(), false, Some(host))?;
    let body = serde_json::json!({ "argv": argv, "cwd": cwd.display().to_string() }).to_string();
    let answer = runtime.deliver_inbound_awaiting(&hook_name(command), [("content-type", "application/json")], &body, None);
    Ok(CommandAnswer { code: exit_code(answer.status), body: answer.body, stderr: answer.stderr })
}

/// `orb run <program>`, booted: the program on the substrate host, `INIT` mounted on every trait.
pub(crate) fn boot_run(program: &Path, trace: bool) -> anyhow::Result<std::sync::Arc<orbital_shell_rust::host::Host>> {
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let runtime = orbital_shell_rust::host::boot::boot_file(&resolved, false, &std::env::vars().collect(), trace, Some(host))?;
    orbital_shell_rust::host::run::mount_init(&runtime).map_err(|e| anyhow::anyhow!("INIT: {e}"))?;
    Ok(runtime)
}

/// `orb run <program>`: boot it, hold it while it has open calls, stop cleanly on SIGINT/SIGTERM.
pub(crate) fn run_program(program: &Path) -> anyhow::Result<()> {
    let stop = stop_on_signal()?;
    let runtime = boot_run(program, false)?;
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

  trait Echo -> EchoRun [integration] {
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

  page "/echo" -> Echo
}
"#;

    fn project(name: &str, topic: &str) -> (PathBuf, PathBuf) {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
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
            CommandAnswer { code: 2, body: String::new(), stderr: Some("usage: orb warn <words>".into()) }
        );
        assert_eq!(written(&refused), (String::new(), "usage: orb warn <words>\n".into()));
        let answered = run_command_program(&program, "warn", &["hi".into()], &cwd).unwrap();
        assert_eq!(
            answered,
            CommandAnswer { code: 0, body: "hi".into(), stderr: None },
            "an answer that omits stderr has none"
        );
        assert_eq!(written(&answered), ("hi\n".into(), String::new()));
    }

    #[test]
    fn control_an_answer_without_stderr_prints_its_body_on_stdout_only() {
        let (cwd, store) = project("no-stderr", "cli");
        let program = find_command_program("echo", &cwd, &store).unwrap().expect("the cli program is found");
        let refused = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!(refused, CommandAnswer { code: 2, body: "usage: orb echo <words>".into(), stderr: None });
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
        let lines: [&[&str]; 8] =
            [&["update"], &["add", "@x/y"], &["remove", "@x/y"], &["list"], &["validate", "a.lolo"], &["emit", "orb"], &["--version"], &["--help"]];
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

  trait Listen -> Listener [integration] {
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

  page "/listener" -> Listen
}
"#;

    fn run_fixture(name: &str, source: &str) -> PathBuf {
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
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

    #[test]
    fn control_the_same_hook_outside_the_cli_topic_is_not_a_command() {
        let (cwd, store) = project("other-topic", "infra");
        assert_eq!(find_command_program("echo", &cwd, &store).unwrap(), None);
    }
}
