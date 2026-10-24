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

use commands::{compile_schema, parse_schema, run_which, stamp_schema_cli, validate_schema};
use utils::evaluate_sexpr;

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
    /// Validate an Orbital schema using the TypeScript validator
    Validate {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Output results as JSON (for programmatic consumption)
        #[arg(long)]
        json: bool,

        /// Path to extra patterns.json (auto-discovered if not specified)
        #[arg(long)]
        patterns: Option<PathBuf>,

        /// Path to services-registry.json (auto-discovered if not specified)
        #[arg(long)]
        services: Option<PathBuf>,

        /// Fail on any warning (the 0 errors / 0 warnings bar)
        #[arg(long)]
        deny_warnings: bool,

        /// The program is a finished app (where a generation concludes): expectations and
        /// controls nothing in it satisfies are errors instead of being deferred to an importer
        #[arg(long = "final")]
        final_program: bool,
    },

    /// Parse and display schema information
    Parse {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,
    },

    /// Lower each `.lolo` (or read each `.orb`) to its program exactly as the
    /// loader does for a live behavior package — one NDJSON line per file,
    /// `{"path", "orb"}` or `{"path", "error"}`, in argument order. The single
    /// lowering entry point TS consumers use (Almadar_Behavior_Packages.md §15).
    Lower {
        /// The `.lolo` / `.orb` files to lower.
        #[arg(value_name = "FILE", required = true)]
        paths: Vec<PathBuf>,
    },

    /// Stamp V4 identity (ids + ledger) onto a name-only `.orb` schema.
    ///
    /// Runs the same `stamp_identity` minter the `.lolo` lowerer uses, but over
    /// an already-lowered `.orb` (e.g. a name-only factory schema). Mints ids
    /// for every declaration, resolves in-file refs to id edges, and writes the
    /// ledger. Unlike `resolve`, the schema shape is preserved — no Inline/
    /// Compose/Enrich runs. Pass `--prior` to keep ids stable across re-stamps.
    Stamp {
        /// Path to the name-only .orb schema file.
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Prior .orb to reuse ids from (byte-stable re-stamp).
        #[arg(long, value_name = "FILE")]
        prior: Option<PathBuf>,

        /// Write the stamped schema to a file. Defaults to stdout.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,

        /// Workspace root to resolve `uses` imports against, in addition to
        /// in-file stamping: after minting local ids, load each aliased
        /// import from this directory's live registries (std/behaviors) and
        /// write the imported declaration's id into every dotted
        /// `Alias.traits.X` / `Alias.entity` / `Alias.pages.X` ref whose id
        /// sibling is still unset. Omit to keep stamping in-file only.
        #[arg(long, value_name = "DIR")]
        imports: Option<PathBuf>,
    },

    /// Compile an Orbital schema to generated code
    Compile {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Output directory
        #[arg(short, long, default_value = "./output")]
        output: PathBuf,

        /// Target shell for code generation
        #[arg(short, long, default_value = "typescript")]
        shell: ShellType,

        /// Compilation mode (full, client, server)
        #[arg(short, long, default_value = "full")]
        mode: ShellMode,

        /// Frontend shell for mixed compilation (e.g., --frontend typescript --backend python)
        #[arg(long)]
        frontend: Option<ShellType>,

        /// Backend shell for mixed compilation
        #[arg(long)]
        backend: Option<ShellType>,

        /// Backend URL for frontend proxy (e.g., http://localhost:3030 for Python backend)
        #[arg(long)]
        backend_url: Option<String>,

        /// Path to extra patterns.json (auto-discovered if not specified)
        #[arg(long)]
        patterns: Option<PathBuf>,

        /// Inject structured logging (createLogger calls) into generated hooks and handlers
        #[arg(long)]
        dev_logs: bool,

        /// Who runs the generated TypeScript app: the embedded kernel (the only host)
        #[arg(long, default_value = "kernel")]
        host: HostArg,

        /// The address the compiled server listens on (the std-http1 host knob)
        #[arg(long, default_value = "127.0.0.1")]
        listen_host: String,

        /// The port the compiled server listens on (the std-http1 port knob); PORT in its environment replaces it
        #[arg(long, default_value = "3030")]
        port: u16,

        /// Write the lowered .orb JSON to this path (for .lolo inputs).
        /// Useful for inspecting how a .lolo file lowers without running codegen.
        #[arg(long)]
        emit_orb: Option<PathBuf>,
    },

    /// Serve a program: the native server, the program's runtime files and the TypeScript shell's prebuilt web client,
    /// on one port. Nothing is built and no Node process runs the app.
    ///
    /// In memory with sample rows by default; with `--no-mock` the environment picks the store (`DATA_BACKEND`, ...)
    /// and sign-in (`AUTH_PROVIDER`, `FIREBASE_PROJECT_ID`, ...). The dev Firebase emulators give the dev personas.
    Serve {
        /// Path to the .orb or .lolo program
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Port to listen on
        #[arg(short, long, default_value = "3030")]
        port: u16,

        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Open the browser once the server listens
        #[arg(long)]
        open: bool,

        /// Serve the store the environment selects instead of sample rows in memory
        #[arg(long)]
        no_mock: bool,

        /// Serve without the Firebase emulators: no dev personas, every request anonymous
        #[arg(long)]
        no_emulators: bool,

        /// A built web client to serve instead of the installed typescript shell's
        #[arg(long, value_name = "DIR")]
        client_dir: Option<PathBuf>,
    },

    /// Emit generated code for inspection (`.orb` JSON, TypeScript, or Python).
    ///
    /// Runs the same compile pipeline as `orb compile` but skips the shell
    /// template copy and filesystem writes — outputs the generated code
    /// directly to stdout (or a file with --output).
    ///
    /// Supports either `.lolo` surface syntax or `.orb` JSON as input.
    /// `--section` narrows the output to a specific part of the schema
    /// (e.g. a single entity or trait). See
    /// `docs/Almadar_Compiler_Emit_Code.md` for the selector grammar.
    Emit {
        /// Output target: `orb`, `ts`, `py`, `lolo`, or `manifest` (the io-gate manifest of each deployment unit).
        #[arg(value_name = "TARGET")]
        target: emit::EmitTarget,

        /// Path to the source `.lolo` or `.orb` file. Omit to read from stdin.
        #[arg(value_name = "FILE")]
        path: Option<PathBuf>,

        /// Emit only the specified section (e.g. `traits.ModalRecordModal`,
        /// `entities`, `pages.login`). Supported heads: entities, traits,
        /// pages, services.
        #[arg(long, value_name = "SELECTOR")]
        section: Option<String>,

        /// Write the rendered output to a file (default: stdout).
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        /// Output format: stream (default), markdown, json, raw.
        /// Auto-detected from --output extension when omitted
        /// (.md → markdown, .json → json, else → stream).
        #[arg(long, value_enum)]
        format: Option<emit::EmitFormat>,

        /// Coarse secondary filter — comma-separated. Kinds: entities, traits,
        /// pages, hooks, components, routes, services, tests.
        #[arg(long, value_name = "KINDS")]
        only: Option<String>,

        /// Emit a Source Map v3 sidecar (`<output>.map`) that maps the
        /// generated code back to byte ranges in the `.orb` source.
        /// When writing to stdout this appends a `//# sourceMappingURL=…`
        /// inline comment instead (TypeScript target only).
        /// Has no effect on `target = orb` or `--format json|raw`.
        #[arg(long)]
        map: bool,

        /// `orb` only: return a source inspection envelope using this original absolute .lolo path.
        #[arg(long, value_name = "PATH")]
        inspect_source: Option<PathBuf>,

        #[arg(long, requires = "inspect_source")]
        document_version: Option<u64>,

        /// `manifest` only: the deployment unit to emit (default: every unit of the program).
        #[arg(long, value_name = "NAME")]
        unit: Option<String>,

        /// `manifest` only: write `<unit>.manifest.json` into this directory (default: stdout, one unit).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,

        /// `manifest` only: the file holding the 64-hex-character ed25519 signing seed. Without it the
        /// manifest is written unsigned.
        #[arg(long, value_name = "PATH", requires = "key_id")]
        sign_key: Option<PathBuf>,

        /// `manifest` only: the id the deploy key set knows the signing key by.
        #[arg(long, value_name = "ID", requires = "sign_key")]
        key_id: Option<String>,
    },

    /// Evaluate an S-expression and print the result
    Evaluate {
        /// S-expression in JSON format (e.g. '[\"+\", 1, 2]'). Reads from stdin if omitted.
        #[arg(value_name = "EXPR")]
        expr: Option<String>,

        /// Bind @entity fields from a JSON object (e.g. '{"score":100}')
        #[arg(long, value_name = "JSON")]
        entity: Option<String>,

        /// Bind @payload fields from a JSON object (e.g. '{"amount":50}')
        #[arg(long, value_name = "JSON")]
        payload: Option<String>,

        /// Output format: value (human-readable), json (compact), pretty (indented JSON)
        #[arg(long, default_value = "value")]
        output: String,
    },

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

    let cli = Cli::parse_from(std::iter::once("orb".to_string()).chain(args.iter().cloned()));

    let needs_language_data = !matches!(
        &cli.command,
        None | Some(Commands::Validate { .. })
            | Some(Commands::Update { .. })
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
        Some(Commands::Validate {
            path,
            json,
            patterns,
            services,
            deny_warnings,
            final_program,
        }) => validate_schema(
            &path,
            json,
            patterns.as_deref(),
            services.as_deref(),
            deny_warnings,
            final_program,
        ),
        Some(Commands::Parse { path }) => parse_schema(&path),
        Some(Commands::Lower { paths }) => {
            use std::io::Write;
            let lower_one = |path: &std::path::PathBuf| match lower_lolo_file(path) {
                Ok(mut schema) => {
                    orbital_core::stamp::ensure_stamped(&mut schema);
                    serde_json::json!({ "path": path, "orb": schema }).to_string()
                }
                Err(error) => serde_json::json!({ "path": path, "error": error }).to_string(),
            };
            // Files lower independently; workers take every n-th file, output keeps input order.
            let workers = std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(paths.len().max(1));
            let mut lines: Vec<String> = vec![String::new(); paths.len()];
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..workers)
                    .map(|w| {
                        let paths = &paths;
                        scope.spawn(move || {
                            (w..paths.len())
                                .step_by(workers)
                                .map(|i| (i, lower_one(&paths[i])))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                for handle in handles {
                    for (i, line) in handle.join().expect("lowering worker panicked") {
                        lines[i] = line;
                    }
                }
            });
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for line in lines {
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
        Some(Commands::Stamp {
            path,
            prior,
            output,
            imports,
        }) => stamp_schema_cli(
            &path,
            prior.as_deref(),
            output.as_deref(),
            imports.as_deref(),
        ),
        Some(Commands::Compile {
            path,
            output,
            shell,
            mode,
            frontend,
            backend,
            backend_url,
            patterns,
            dev_logs,
            host,
            listen_host,
            port,
            emit_orb,
        }) => compile_schema(
            &path,
            &output,
            shell,
            mode,
            frontend,
            backend,
            backend_url,
            patterns.as_deref(),
            dev_logs,
            host,
            &utils::shell_programs::ServedAddress { host: &listen_host, port },
            emit_orb.as_deref(),
        ),
        Some(Commands::Serve { path, port, host, open, no_mock, no_emulators, client_dir }) => {
            if open {
                anyhow::bail!("orb serve --open has no declared counterpart in std-http-server: open the browser yourself");
            }
            if no_emulators {
                anyhow::bail!("orb serve --no-emulators has no declared counterpart in std-http-server: sign-in is selected by AUTH_PROVIDER in the environment");
            }
            commands::program_command::serve_program(
                &path,
                &commands::program_command::ServeOptions { host: &host, port, no_mock, client_dir: client_dir.as_deref() },
            )
        }
        Some(Commands::Evaluate {
            expr,
            entity,
            payload,
            output,
        }) => evaluate_sexpr(
            expr.as_deref(),
            entity.as_deref(),
            payload.as_deref(),
            &output,
        ),
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
        Some(Commands::Emit {
            target,
            path,
            section,
            output,
            format,
            only,
            map,
            inspect_source,
            document_version,
            unit,
            out,
            sign_key,
            key_id,
        }) => emit::run(emit::EmitOptions {
            target,
            path,
            section,
            output,
            format,
            only,
            map,
            inspect_source,
            document_version,
            unit,
            out,
            sign_key,
            key_id,
        }),
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
