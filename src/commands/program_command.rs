//! `orb <command> …` for a command `orb` has no handler of its own for: an installed behavior in the `cli` topic that
//! declares the inbound hook `cli-<command>` is the command. `orb` boots it, delivers `{ argv, cwd }` to that hook as
//! JSON, prints the answer's body and exits with the answer's status class (EXEC-INBOUND-1; the answer may come from a
//! dispatch the handler's keyed completions cause, `OrbitalServerRuntime::deliver_inbound_awaiting`).

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

/// The installed program in a `cli` topic that declares the hook `cli-<command>`, from the behavior packages resolved
/// from `cwd` and then `user_store`. More than one is an error that names them.
pub(crate) fn find_command_program(command: &str, cwd: &Path, user_store: &Path) -> anyhow::Result<Option<PathBuf>> {
    let hook = hook_name(command);
    let packages = orbital_core::behavior_packages::discover(cwd, user_store).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut found: Vec<PathBuf> = Vec::new();
    for package in &packages {
        let files = orbital_core::behavior_index::indexed_files(package).map_err(|e| anyhow::anyhow!("{e}"))?;
        for file in files.iter().filter(|file| file.topic() == "cli") {
            let Some(path) = orbital_core::behavior_index::lolo_path_of(package, file).filter(|p| p.is_file()) else {
                continue;
            };
            let schema = orbital_compiler::parse_file(&path).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
            if schema.inbound.iter().flatten().any(|name| *name == hook) {
                found.push(path);
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => anyhow::bail!(
            "more than one installed program declares `{hook}`: {}",
            found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Run `program` as `orb <command> <argv…>`: its answer's body on stdout, its exit code returned.
pub(crate) fn run_command_program(program: &Path, command: &str, argv: &[String], cwd: &Path) -> anyhow::Result<(i32, String)> {
    let resolved = crate::commands::dev::resolve_to_temp(program)?;
    let host = orbital_server::SubstratePort(std::sync::Arc::new(crate::commands::program_host::ProgramHost));
    let runtime = orbital_server::boot_runtime_with(&resolved, false, &std::env::vars().collect(), false, Some(host))?;
    let body = serde_json::json!({ "argv": argv, "cwd": cwd.display().to_string() }).to_string();
    let answer = runtime.deliver_inbound_awaiting(&hook_name(command), [("content-type", "application/json")], &body, None);
    Ok((exit_code(answer.status), answer.body))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (code, out) = run_command_program(&program, "echo", &["hello".into(), "world".into()], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (0, "hello world"));
        let (code, out) = run_command_program(&program, "echo", &[], &cwd).unwrap();
        assert_eq!((code, out.as_str()), (2, "usage: orb echo <words>"), "a refused input is exit 2 with its message");
    }

    #[test]
    fn control_the_same_hook_outside_the_cli_topic_is_not_a_command() {
        let (cwd, store) = project("other-topic", "infra");
        assert_eq!(find_command_program("echo", &cwd, &store).unwrap(), None);
    }
}
