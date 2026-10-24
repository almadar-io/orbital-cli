//! Orbital CLI
//!
//! Thin command-line interface for Orbital tools.
//! All heavy lifting is delegated to orbital-core, orbital-server, and orbital-client.

#![allow(unexpected_cfgs)]

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::path::PathBuf;

mod baked_engine_pins;
mod commands;
mod emit;
mod utils;

use commands::run_which;

pub use utils::{parse_schema_auto_typed, TypedParseError};

#[derive(Parser)]
#[command(name = "orb")]
#[command(about = "Orb - The Physics of Software", long_about = None)]
#[command(version)]
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
    /// Install what orb needs and is missing (the language data it pins, std, the
    /// first-party language packs, the TypeScript shell, orb-desktop), update
    /// every orb package within its range, and check each loads. Also the
    /// first-run setup: the installers run `orb update --global`.
    Update {
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
    },

    /// Install orb packages — behaviors, a language pack, a shell
    /// (`@almadar/orb-shell-<name>`), orb-desktop: any npm package whose
    /// package.json declares orbLanguage, orb, orbVocabulary or orbProgram.
    /// A package that does not load for this orb is removed again.
    Add {
        /// bun specs: `name` or `name@version`.
        #[arg(required = true)]
        packages: Vec<String>,
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
    },

    /// Uninstall orb packages.
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
    },

    /// The orb packages installed for the project (or `--global`), each with
    /// its kinds, version and whether it loads for this orb.
    List {
        #[arg(long)]
        json: bool,
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
    },

    /// Run a long-lived program: boot it on the substrate host, mount INIT, and keep it
    /// running while it has an open stream or listener; SIGINT/SIGTERM cancels them and exits.
    Run {
        /// The `.lolo`/`.orb` program.
        program: PathBuf,
    },

    /// Behavior packages — install/remove/update/inspect the npm packages
    /// `uses` resolves against (Almadar_Behavior_Packages.md).
    Behaviors {
        #[command(subcommand)]
        command: BehaviorsCommand,
    },



}

#[derive(Subcommand)]
enum VectorsCommand {
    /// Write a package's vector snapshot (`dist/vectors.vec`): the vectors of its organism and
    /// `#capabilities` texts, taken from the user store and embedding misses (needs an embedding
    /// provider for those). The same inputs give the same bytes.
    Snapshot {
        /// The installed behavior package directory.
        #[arg(long)]
        package: std::path::PathBuf,
        /// Where to write; defaults to `<package>/dist/vectors.vec`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum BehaviorsCommand {
    /// Resolve one `uses` specifier ("which copy compiled?") — prints the
    /// resolved `.orb` path and the owning package.
    Which {
        /// `<prefix>/<name>` or `std/behaviors/<name>`.
        spec: String,
    },
    /// The installed behaviors under one or more registry scopes
    /// (`<prefix>/<topic>[/<tier>]`, e.g. `almadar-std/ui/core/molecules`;
    /// `std/...` spells the std package), with descriptions, trait values and
    /// shipped embedding vectors.
    Catalog {
        /// Scope paths: a package prefix, optionally narrowed by topic and tier.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Keep only entries with one of these exposures
        /// (`app|experience|palette|both|internal`); repeatable.
        #[arg(long)]
        exposure: Vec<String>,
        /// Print as JSON (`CatalogEntry[]`).
        #[arg(long)]
        json: bool,
        /// Leave out the vectors (and the embedding model): no vector store or provider is read.
        #[arg(long)]
        no_vectors: bool,
    },
    /// The texts an installed package embeds, one `{ key, text, hash }` each: the organism
    /// text, its `<name>#capabilities` text and every knob text, in embed order.
    Texts {
        /// Scope paths, as `catalog` takes them (a package prefix, optionally narrowed).
        #[arg(required = true)]
        paths: Vec<String>,
        /// Keep only one kind of text (`organism|capability|knob`).
        #[arg(long)]
        kind: Option<String>,
        /// Print as JSON (the only format; accepted for symmetry with `catalog`).
        #[arg(long)]
        json: bool,
    },
    /// Describe one trait of an installed behavior: knobs, events, embedding
    /// model. Knob vectors are attached only with `--knob-vectors`.
    Describe {
        /// `<behavior>.traits.<Trait>`, or `-` to read a trait value (JSON) from stdin.
        spec: String,
        /// Print as JSON (`TraitDescription`).
        #[arg(long)]
        json: bool,
        /// Attach each knob's embedding vector, embedding the trait's knob texts on a miss
        /// (needs an embedding provider; stored in `~/.almadar/vectors`).
        #[arg(long)]
        knob_vectors: bool,
    },
    /// The embedding vector store (`~/.almadar/vectors`).
    Vectors {
        #[command(subcommand)]
        command: VectorsCommand,
    },
    /// Derive every orbital's `expects` from the behavior as written (imports resolved through
    /// the loader): `{ <orbital>: { expectations, diagnostics } }`.
    Expects {
        /// `<prefix>/<name>`, `std/behaviors/<name>`, a workspace-relative `./<dir>/<name>`, or a `.orb` path.
        spec: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a behavior's `.orb` as JSON (the file the loader resolves).
    Source {
        /// `<prefix>/<name>`, `std/behaviors/<name>` or a workspace-relative `./<dir>/<name>`.
        spec: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The loader's `.lolo` lowerer: a relative import of a `.lolo`-only sibling.
fn lower_lolo_file(path: &std::path::Path) -> Result<orbital_core::schema::OrbitalSchema, String> {
    let _span = tracing::debug_span!(target: orbital_core::logging::ns::CLI, "lower import", file = %path.display()).entered();
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    crate::utils::parse_schema_auto_typed(&content, &path.to_string_lossy())
        .map_err(|e| e.describe())
}

/// `orb behaviors describe` for a trait value: what the compiler answers a literal describe with.
fn describe_trait_value(
    value: &serde_json::Value,
    from: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    commands::behaviors_catalog::describe_from(
        commands::behaviors_catalog::DescribeTarget::Value(value.clone()),
        from,
    )
    .map_err(|e| e.to_string())
}

pub fn run() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
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
    let program = tracing::info_span!(target: ns::CLI, "resolve command")
        .in_scope(|| commands::program_command::resolve_program_command(args, &cwd, &store))?;
    if let Some(command) = program {
        let answer = tracing::info_span!(target: ns::CLI, "program", hook = %command.command)
            .in_scope(|| commands::program_command::run_command_program(&command.program, &command.command, &command.argv, &cwd))?;
        if answer.headers.contains_key("orb-run") {
            if let Some(text) = answer.stderr.as_deref().filter(|t| !t.is_empty()) {
                eprintln!("{text}");
            }
            commands::program_command::run_handoff(&answer)?;
            return Ok(answer.code);
        }
        answer.write_to(&mut std::io::stdout(), &mut std::io::stderr())?;
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
        None | Some(Commands::Update { .. })
            | Some(Commands::Add { .. })
            | Some(Commands::Remove { .. })
            | Some(Commands::List { .. })
            | Some(Commands::Behaviors {
                command: BehaviorsCommand::Which { .. }
            })
    );
    if needs_language_data {
        tracing::info_span!(target: ns::CLI, "language data").in_scope(crate::utils::require_language_data)?;
    }
    let notify = !matches!(
        &cli.command,
        Some(Commands::Update { .. }) | Some(Commands::List { json: true, .. })
    );
    let result = tracing::info_span!(target: ns::CLI, "run").in_scope(|| dispatch(cli));
    if notify && result.is_ok() {
        tracing::info_span!(target: ns::CLI, "update notice").in_scope(commands::update_notice::notice);
    }
    result.map(|()| 0)
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Commands::Behaviors { command }) => match command {
            BehaviorsCommand::Which { spec } => run_which(spec),
            BehaviorsCommand::Catalog {
                paths,
                exposure,
                json,
                no_vectors,
            } => commands::run_catalog(paths, exposure, json, no_vectors),
            BehaviorsCommand::Texts { paths, kind, json: _ } => commands::run_texts(paths, kind),
            BehaviorsCommand::Describe {
                spec,
                json,
                knob_vectors,
            } => commands::run_describe(spec, json, knob_vectors),
            BehaviorsCommand::Vectors { command } => match command {
                VectorsCommand::Snapshot { package, out } => {
                    commands::behaviors_vectors_snapshot::run_snapshot(package, out)
                }
            },
            BehaviorsCommand::Source { spec, json } => commands::run_source(spec, json),
            BehaviorsCommand::Expects { spec, json } => commands::run_expects(spec, json),
        },
        Some(Commands::Run { program }) => commands::program_command::run_program(&program),
        Some(Commands::Update { global }) => commands::packages::run_update(global),
        Some(Commands::Add { packages, global }) => commands::packages::run_add(packages, global),
        Some(Commands::Remove { packages, global }) => commands::packages::run_remove(packages, global),
        Some(Commands::List { json, global }) => commands::packages::run_list(json, global),
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

// ============================================================================
// JSON Output Types for validate --json
// ============================================================================

/// JSON output for validation results (matches TypeScript DeepAgent tool expectations)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationJsonOutput {
    pub(crate) success: bool,
    pub(crate) valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) errors: Option<Vec<ValidationJsonError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) warnings: Option<Vec<ValidationJsonWarning>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stats: Option<ValidationStats>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationJsonError {
    pub(crate) code: String,
    pub(crate) path: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) suggestion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fix_guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_shape: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) valid_values: Option<Vec<String>>,
    #[serde(flatten)]
    pub(crate) position: Option<SourcePosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) needs_install: Option<bool>,
}

/// Where a diagnostic sits in the `.lolo` source: 1-based `line`/`column`,
/// `endColumn` just past the last character. `approximate`: the span is the
/// enclosing construct's, not the offending token's.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourcePosition {
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) end_line: u32,
    pub(crate) end_column: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) approximate: bool,
}

impl SourcePosition {
    pub(crate) fn of(span: orbital_lolo::Span, exact: bool) -> Self {
        Self {
            line: span.line,
            column: span.col,
            end_line: span.line,
            end_column: span.col + u32::from(span.len.max(1)),
            approximate: !exact,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationJsonWarning {
    pub(crate) code: String,
    pub(crate) path: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) suggestion: Option<String>,
    #[serde(flatten)]
    pub(crate) position: Option<SourcePosition>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationStats {
    pub(crate) orbitals: usize,
    pub(crate) entities: usize,
    pub(crate) traits: usize,
    pub(crate) pages: usize,
}
