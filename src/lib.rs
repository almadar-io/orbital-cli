//! Orbital CLI
//!
//! Thin command-line interface for Orbital tools.
//! All heavy lifting is delegated to orbital-core, orbital-server, and orbital-client.

#![allow(unexpected_cfgs)]

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

mod commands;
mod reflect;
mod utils;

pub use utils::{parse_schema_auto_typed, TypedParseError};

/// `orb --version`: the release and the language versions this orb reads, `X.Y.Z (language <range>)` (P-022).
fn version_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| format!("{} (language {})", env!("CARGO_PKG_VERSION"), orbital_core::language_data::LANGUAGE_READS))
}

#[derive(Parser)]
#[command(name = "orb")]
#[command(about = "Orb - The Physics of Software", long_about = None)]
#[command(version = version_line())]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    log: utils::terminal::LogArgs,
}

/// A command's exit status other than success, after it printed its own report.
#[derive(Debug)]
pub(crate) struct Exit(pub(crate) i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// Available shell types for code generation
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellType {
    /// TypeScript/React client with a Node server running the app composed with std-http-server
    Typescript,
    /// Python host running the program on the native kernel
    Python,
    /// Rust native host running the served program on the kernel, plus Dioxus and TUI clients
    Rust,
    /// Native Android application with Jetpack Compose
    Android,
    /// Native iOS application with SwiftUI
    Ios,
    /// Chrome extension (Manifest V3) hosting the program in its service worker
    ChromeExtension,
    /// Orbital OS: an seL4 root task running the program on the kernel, booted on QEMU's aarch64 virt board
    Os,
}

impl ShellType {
    /// The shell program (`orb-shell-<name>`) this target runs.
    pub(crate) fn program(self) -> &'static str {
        match self {
            ShellType::Typescript => "typescript",
            ShellType::Python => "python",
            ShellType::Rust => "rust",
            ShellType::Android => "android",
            ShellType::Ios => "ios",
            ShellType::ChromeExtension => "chrome",
            ShellType::Os => "os",
        }
    }
}

/// Who runs a generated TypeScript app's execution steps
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum HostArg {
    /// The embedded `@almadar/kernel` (Express server)
    #[default]
    Kernel,
}

/// Compilation mode for mixed-language support
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum ShellMode {
    /// Generate everything (default)
    #[default]
    Full,
    /// Client-only: pages, components, client effects
    Client,
    /// Server-only: routes, handlers, server effects
    Server,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a long-lived program: boot it on the substrate host, mount INIT, and keep it
    /// running while it has an open stream or listener; SIGINT/SIGTERM cancels them and exits.
    Run {
        /// The `.lolo`/`.orb` program.
        program: PathBuf,
    },

}

/// The loader's `.lolo` lowerer: a relative import of a `.lolo`-only sibling.
fn lower_lolo_file(path: &std::path::Path) -> Result<orbital_core::schema::OrbitalSchema, String> {
    let _span = tracing::debug_span!(target: orbital_core::logging::ns::CLI, "lower import", file = %path.display()).entered();
    let content = orbital_core::read_set::read_to_string(path).map_err(|e| e.to_string())?;
    crate::utils::parse_schema_auto_typed(&content, &path.to_string_lossy())
        .map_err(|e| e.describe())
}

/// A newer orb `orb update` installed into the user store, unless this run was already handed to it.
fn handoff_target() -> Option<std::path::PathBuf> {
    if std::env::var_os(utils::bootstrap::HANDED_OFF_ENV).is_some() {
        return None;
    }
    let store = orbital_core::behavior_packages::user_store_dir()?;
    utils::bootstrap::newer_orb(&store, utils::bootstrap::platform_package()?, env!("CARGO_PKG_VERSION"))
}

/// Run `newer` with this run's arguments: replace this process where the platform can, else wait and exit with its code.
fn hand_off(newer: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let mut command = std::process::Command::new(newer);
    command.args(args).env(utils::bootstrap::HANDED_OFF_ENV, env!("CARGO_PKG_VERSION"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        anyhow::bail!("could not hand the run to {}: {error}", newer.display());
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| anyhow::anyhow!("could not hand the run to {}: {e}", newer.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// `orb behaviors describe` for a trait value: what the compiler answers a literal describe with.
fn describe_trait_value(
    value: &serde_json::Value,
    from: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    reflect::describe(reflect::DescribeTarget::Value(value.clone()), from).map_err(|e| e.to_string())
}

pub fn run() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(newer) = handoff_target() {
        return hand_off(&newer, &raw);
    }
    let _ = orbital_core::language_data::register_host_release(env!("CARGO_PKG_VERSION"));
    let (log, args) = utils::terminal::split_log_args(&raw)?;
    let terminal = utils::terminal::init(&log);
    orbital_compiler::loader::register_source_lowerer(|path, _, _| lower_lolo_file(path));
    orbital_compiler::phases::reflect::register_trait_describer(describe_trait_value);

    let root = tracing::info_span!(
        target: orbital_core::logging::ns::CLI,
        "orb",
        args = %args.join(" "),
        outcome = tracing::field::Empty,
    );
    let result = root.in_scope(|| run_command(&args));
    orbital_core::logging::record_outcome(&root, &result);
    drop(root);
    terminal.finish()?;
    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(e) => match e.downcast_ref::<Exit>() {
            Some(Exit(code)) => std::process::exit(*code),
            None => Err(e),
        },
    }
}

/// Run the command `args` names and answer its exit code.
fn run_command(args: &[String]) -> anyhow::Result<i32> {
    use orbital_core::logging::ns;
    let cwd = std::env::current_dir()?;
    let store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    let resolve = || {
        tracing::info_span!(target: ns::CLI, "resolve command")
            .in_scope(|| commands::program_command::resolve_program_command(args, &cwd, &store))
    };
    let program = match resolve() {
        Ok(Some(command)) => Some(command),
        found if args.first().map(String::as_str) == Some("update") => {
            let installed = utils::bootstrap::repair(&[cwd.as_path(), store.as_path()], &store)
                .map_err(|e| e.context(match &found {
                    Ok(_) => "no installed program answers `orb update`".to_string(),
                    Err(cause) => format!("the installed programs do not load ({cause})"),
                }))?;
            for (pin, files) in installed {
                eprintln!("orb: installed {}@{} ({files} files)", pin.name, pin.version);
            }
            resolve()?
        }
        found => found?,
    };
    if let Some(command) = program {
        let answer = tracing::info_span!(target: ns::CLI, "program", hook = %command.command)
            .in_scope(|| commands::program_command::run_command_program(&command.program, &command.command, &command.argv, &cwd))?;
        let ran = command.command.clone();
        let notice = commands::program_command::notice_argv(&command.argv);
        drop(command);
        if answer.headers.contains_key("orb-run") {
            if let Some(text) = answer.stderr.as_deref().filter(|t| !t.is_empty()) {
                eprintln!("{text}");
            }
            let code = answer.code;
            commands::program_command::run_handoff(answer)?;
            return Ok(code);
        }
        answer.write_to(&mut std::io::stdout(), &mut std::io::stderr())?;
        if answer.code == 0 && ran != "notice" {
            tracing::info_span!(target: ns::CLI, "update notice")
                .in_scope(|| commands::program_command::update_notice(&notice, &cwd, &store));
        }
        return Ok(answer.code);
    }

    if let Some(message) = commands::program_command::not_installed(&args) {
        // A command program missing because the language data is not installed or invalid: the typed preflight error
        // names the cause and its fix; only with the language data in place is the program itself the missing piece.
        crate::utils::require_language_data()?;
        anyhow::bail!(message);
    }
    let cli = Cli::parse_from(std::iter::once("orb".to_string()).chain(args.iter().cloned()));

    let needs_language_data = !matches!(
        &cli.command,
        None
    );
    if needs_language_data {
        tracing::info_span!(target: ns::CLI, "language data").in_scope(crate::utils::require_language_data)?;
    }
    let result = tracing::info_span!(target: ns::CLI, "run").in_scope(|| dispatch(cli));
    result.map(|()| 0)
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Commands::Run { program }) => commands::program_command::run_program(&program),
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub(crate) use orbital_core::validation_report::{SourcePosition, ValidationJsonError, ValidationJsonOutput, ValidationJsonWarning, ValidationStats};

pub(crate) fn source_position(span: orbital_lolo::Span, exact: bool) -> SourcePosition {
    SourcePosition {
        line: span.line,
        column: span.col,
        end_line: span.line,
        end_column: span.col + u32::from(span.len.max(1)),
        approximate: !exact,
    }
}
