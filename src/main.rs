//! Orbital CLI
//!
//! Thin command-line interface for Orbital tools.
//! All heavy lifting is delegated to orbital-core, orbital-server, and orbital-client.

#![allow(unexpected_cfgs)]

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::path::PathBuf;

mod commands;
mod emit;
mod i18n;
mod utils;

use commands::{
    compile_schema, create_new_project, format_schema, list_templates, parse_schema,
    resolve_schema_cli, run_convert, run_dev_mode, run_serve, run_tests, simulate_render,
    validate_schema,
};
use utils::{evaluate_sexpr, spawn_agent, spawn_agent_with_args};

// Re-export parsing helpers + the `strip_json_nulls` util at the crate root so
// the `emit` module's `crate::strip_json_nulls` / parse references keep
// resolving after the module split. Public surface is unchanged.
pub(crate) use utils::strip_json_nulls;
pub use utils::{parse_schema_auto_typed, TypedParseError};

#[derive(Parser)]
#[command(name = "orb")]
#[command(about = "Orb - The Physics of Software", long_about = None)]
#[command(version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Initial prompt for the agent (when no subcommand is given)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) prompt: Vec<String>,

    /// LLM provider override
    #[arg(long, global = true)]
    pub(crate) provider: Option<String>,

    /// Autonomy level: full, balanced, cautious
    #[arg(long, global = true)]
    pub(crate) autonomy: Option<String>,

    /// Resume a previous session
    #[arg(long, global = true)]
    pub(crate) resume: Option<Option<String>>,

    /// Resume the most recent session
    #[arg(long, global = true)]
    pub(crate) last: bool,

    /// Cost budget in USD
    #[arg(long, global = true)]
    pub(crate) budget: Option<f64>,

    /// List all sessions
    #[arg(long)]
    list: bool,
}

/// Available shell types for code generation
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellType {
    /// TypeScript/React frontend with Express backend
    Typescript,
    /// Python with FastAPI and PyTorch
    Python,
    /// React Native mobile application
    Mobile,
    /// Rust native application with Axum server and egui client
    Rust,
    /// Native Android application with Jetpack Compose
    Android,
    /// Native iOS application with SwiftUI
    Ios,
}

/// Server framework choice for backend code generation
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum ServerFrameworkArg {
    /// Express.js (default, backward-compatible)
    #[default]
    Express,
    /// Hono (lightweight, edge-ready)
    Hono,
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
    /// Create a new project from a template
    New {
        /// Project name (will create directory with this name)
        #[arg(value_name = "NAME", required_unless_present = "list")]
        name: Option<String>,

        /// Template to use
        #[arg(short, long, default_value = "crud")]
        template: String,

        /// List available templates
        #[arg(long)]
        list: bool,

        /// Target shell for compilation
        #[arg(short, long, default_value = "typescript")]
        shell: ShellType,

        /// Skip git initialization
        #[arg(long)]
        no_git: bool,

        /// Skip dependency installation
        #[arg(long)]
        no_install: bool,
    },

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

        /// Simulate render output — trace every reachable state and report what render-ui effects fire
        #[arg(long)]
        simulate: bool,
    },

    /// Parse and display schema information
    Parse {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,
    },

    /// Print the embedded lolo type-signature catalog (signatures.lolo).
    ///
    /// Emits the generated catalog of every operator (arity + return types),
    /// enum/struct, binding-context rule, and std behavior entity/trait/page
    /// contract — the canonical lolo vocabulary that agents (e.g. rabit) read
    /// to author valid `.lolo`. Baked into the binary at compile time from
    /// `signatures.lolo`; regenerate with `make lolo-sync`.
    Signatures {
        /// Write to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// Resolve a schema: run Inline + Compose + Enrich and emit the
    /// fully-inlined .orb form without generating code.
    ///
    /// Produces a resolved artifact structurally identical to the input
    /// (same `OrbitalSchema` JSON shape) but with every `uses[]` import
    /// flattened, every trait ref collapsed, every `behavior/*` composer
    /// expanded, and every `events: { OLD: NEW }` rename applied through
    /// transitions AND render-ui props. Downstream tools (verifiers,
    /// parity mode, future analyzers) consume this as the canonical
    /// bundled view of the schema without reimplementing resolver logic.
    Resolve {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Write the resolved schema to a file. Defaults to stdout.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,

        /// Verbose phase logging on stderr.
        #[arg(short, long)]
        verbose: bool,
    },

    /// Verify an app headlessly: Layer 1 (kernel circuit) + Layer 2 (client render).
    /// Pass a FILE, or `--registry [--topic <t>]` to sweep the std/behaviors corpus.
    Verify {
        /// Path to a single .orb schema file (omit to use --registry).
        #[arg(value_name = "FILE")]
        path: Option<PathBuf>,

        /// Verify across the almadar-std + almadar-behaviors registry corpus.
        #[arg(long)]
        registry: bool,

        /// Restrict the corpus sweep to one topic (e.g. `app`, `game`).
        #[arg(long, value_name = "TOPIC")]
        topic: Option<String>,

        /// Write per-organism verify-report.json into this directory.
        #[arg(long, value_name = "DIR")]
        report_dir: Option<PathBuf>,
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

        /// Backend URL for frontend proxy (e.g., http://localhost:8000 for Python backend)
        #[arg(long)]
        backend_url: Option<String>,

        /// Path to extra patterns.json (auto-discovered if not specified)
        #[arg(long)]
        patterns: Option<PathBuf>,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Inject structured logging (createLogger calls) into generated hooks and handlers
        #[arg(long)]
        dev_logs: bool,

        /// Server framework for backend code generation (express or hono)
        #[arg(long, default_value = "express")]
        server: ServerFrameworkArg,

        /// Write the lowered .orb JSON to this path (for .lolo inputs).
        /// Useful for inspecting how a .lolo file lowers without running codegen.
        #[arg(long)]
        emit_orb: Option<PathBuf>,
    },

    /// Run exhaustive state machine tests against a schema
    Test {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Filter tests by orbital name
        #[arg(long)]
        orbital: Option<String>,

        /// Filter tests by trait name
        #[arg(long, name = "trait")]
        trait_name: Option<String>,

        /// Output format (text, json)
        #[arg(long, default_value = "text")]
        report: String,

        /// Actually execute tests headlessly against the state machine (RCG-09)
        #[arg(long)]
        execute: bool,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Format an Orbital schema file (pretty-print JSON or lolo source)
    Format {
        /// Path to the .orb or .lolo schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Edit the file in place (default: print to stdout)
        #[arg(short, long)]
        write: bool,
    },

    /// Compile and serve a full-stack app from a .orb file (zero-install)
    ///
    /// Compiles the schema with Hono backend, builds the client with Vite,
    /// and serves everything on a single port using the bundled Bun runtime.
    /// No Node.js, pnpm, or any other dependencies required.
    Serve {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Port to listen on
        #[arg(short, long, default_value = "3030")]
        port: u16,

        /// Open browser after start
        #[arg(long)]
        open: bool,

        /// Skip client build (serve existing build)
        #[arg(long)]
        no_build: bool,

        /// Path to extra patterns.json (auto-discovered if not specified)
        #[arg(long)]
        patterns: Option<PathBuf>,
    },

    /// Start the Orbital desktop GUI application (not yet stable)
    #[command(hide = true)]
    Gui {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Server URL to connect to
        #[arg(short, long, default_value = "http://127.0.0.1:3000")]
        server: String,
    },

    /// Start development server with hot-reload (not yet stable)
    #[command(hide = true)]
    Dev {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Port for the server
        #[arg(short, long, default_value = "3000")]
        port: u16,

        /// Force headless mode even when a display is available
        #[arg(long)]
        headless: bool,
    },

    /// Convert an existing project or website to an .orb program
    Convert {
        /// Source project path (source scan mode)
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,

        /// Output .orb file path
        #[arg(short, long, default_value = "converted.orb")]
        output: String,

        /// Crawl a live website (crawl mode, or hybrid if path also given)
        #[arg(long, value_name = "URL")]
        crawl: Option<String>,

        /// Read pre-collected CrawlFacts JSON instead of crawling
        #[arg(long, value_name = "FILE")]
        crawl_facts: Option<PathBuf>,

        /// Enable AI interpretation and fix loop (G4-G7)
        #[arg(long)]
        ai: bool,

        /// Enable round-trip visual verification
        #[arg(long)]
        verify: bool,

        /// Stop after fact collection (G0-G3)
        #[arg(long)]
        facts_only: bool,

        /// Run browser in headed mode
        #[arg(long)]
        headed: bool,

        /// Max crawl depth
        #[arg(long, default_value = "3")]
        depth: u32,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Auto-fix validation errors in a schema (delegates to agent)
    Fix {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,
    },

    /// Run evaluation specs (delegates to agent)
    Eval {
        /// Evaluation spec name
        spec: Option<String>,

        /// Run all evaluation specs
        #[arg(long)]
        all: bool,
    },

    /// View agent trace from a session
    Trace {
        /// Session ID to view
        session: Option<String>,
    },

    /// Authenticate with Almadar Cloud
    Login,

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
        /// Output target language: `orb`, `ts`, or `py`.
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

        /// Server framework for the TypeScript backend (default: hono).
        #[arg(long, value_enum, default_value_t = emit::EmitServer::Hono)]
        server: emit::EmitServer,

        /// Emit a Source Map v3 sidecar (`<output>.map`) that maps the
        /// generated code back to byte ranges in the `.orb` source.
        /// When writing to stdout this appends a `//# sourceMappingURL=…`
        /// inline comment instead (TypeScript target only).
        /// Has no effect on `target = orb` or `--format json|raw`.
        #[arg(long)]
        map: bool,
    },

    /// Reverse-lookup a position in an emitted file to its `.orb` origin.
    ///
    /// Reads `<output>.map` beside the given file and prints the source
    /// location. Requires the file to have been produced by
    /// `orb emit … --map -o <output>`.
    ///
    /// Example: `orb emit-trace out.ts:42:8`
    ///
    /// (Named `emit-trace` because `trace` is already used for agent sessions.)
    #[command(name = "emit-trace")]
    EmitTrace {
        /// Position as `<file>:<line>[:<col>]` (1-indexed).
        #[arg(value_name = "POSITION")]
        position: String,
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
}

/// The lolo type-signature catalog, baked at compile time. Generated from the
/// vendored @almadar/{core,std,patterns} packages by `tools/lolo-types-sync`
/// (`make lolo-sync`). Exposed via `orb signatures` so the bundled agent can
/// read the canonical vocabulary from the binary instead of a fragile path.
const LOLO_SIGNATURES: &str = include_str!("../../../signatures.lolo");

fn print_lolo_signatures(out: Option<&std::path::Path>) -> anyhow::Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, LOLO_SIGNATURES)?;
            eprintln!(
                "Wrote {} bytes to {}",
                LOLO_SIGNATURES.len(),
                path.display()
            );
        }
        None => {
            print!("{LOLO_SIGNATURES}");
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::New {
            name,
            template,
            list,
            shell,
            no_git,
            no_install,
        }) => {
            if list {
                list_templates()
            } else if let Some(project_name) = name {
                create_new_project(&project_name, &template, shell, no_git, no_install)
            } else {
                anyhow::bail!("Project name is required. Usage: orb new <name>")
            }
        }
        Some(Commands::Validate {
            path,
            json,
            patterns,
            services,
            simulate,
        }) => {
            validate_schema(&path, json, patterns.as_deref(), services.as_deref())?;
            if simulate {
                simulate_render(&path, patterns.as_deref())?;
            }
            Ok(())
        }
        Some(Commands::Parse { path }) => parse_schema(&path),
        Some(Commands::Signatures { out }) => print_lolo_signatures(out.as_deref()),
        Some(Commands::Resolve {
            path,
            output,
            verbose,
        }) => resolve_schema_cli(&path, output.as_deref(), verbose),
        Some(Commands::Verify {
            path,
            registry,
            topic,
            report_dir,
        }) => commands::verify::run_verify(path, registry, topic, report_dir),
        Some(Commands::Format { path, write }) => format_schema(&path, write),
        Some(Commands::Compile {
            path,
            output,
            shell,
            mode,
            frontend,
            backend,
            backend_url,
            patterns,
            verbose,
            dev_logs,
            server,
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
            verbose,
            dev_logs,
            server,
            emit_orb.as_deref(),
        ),
        Some(Commands::Test {
            path,
            orbital,
            trait_name,
            report,
            execute,
            verbose,
        }) => run_tests(&path, orbital, trait_name, &report, execute, verbose),
        Some(Commands::Serve {
            path,
            port,
            open,
            no_build,
            patterns,
        }) => run_serve(&path, port, open, no_build, patterns.as_deref()),
        Some(Commands::Gui { path, server }) => orbital_client::run_desktop(&path, Some(&server)),
        Some(Commands::Dev {
            path,
            port,
            headless,
        }) => run_dev_mode(&path, port, headless),
        Some(Commands::Convert {
            path,
            output,
            crawl,
            crawl_facts,
            ai,
            verify,
            facts_only,
            headed,
            depth,
            verbose,
        }) => run_convert(
            path,
            &output,
            crawl,
            crawl_facts,
            ai,
            verify,
            facts_only,
            headed,
            depth,
            verbose,
        ),
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
        // Agent-delegating commands
        Some(Commands::Fix { path }) => {
            spawn_agent_with_args(&["fix", &path.display().to_string()])
        }
        Some(Commands::Eval { spec, all }) => {
            let mut args = vec!["eval"];
            if all {
                args.push("--all");
            }
            if let Some(ref s) = spec {
                args.push(s);
            }
            spawn_agent_with_args(&args)
        }
        Some(Commands::Trace { session }) => {
            let mut args = vec!["trace"];
            if let Some(ref s) = session {
                args.push(s);
            }
            spawn_agent_with_args(&args)
        }
        Some(Commands::Login) => spawn_agent_with_args(&["login"]),
        Some(Commands::Emit {
            target,
            path,
            section,
            output,
            format,
            only,
            server,
            map,
        }) => emit::run(emit::EmitOptions {
            target,
            path,
            section,
            output,
            format,
            only,
            server,
            map,
        }),
        Some(Commands::EmitTrace { position }) => emit::trace::run(&position),
        // No subcommand: spawn the interactive agent
        None => {
            if cli.list {
                spawn_agent_with_args(&["status"])
            } else {
                spawn_agent(&cli)
            }
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationJsonWarning {
    pub(crate) code: String,
    pub(crate) path: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValidationStats {
    pub(crate) orbitals: usize,
    pub(crate) entities: usize,
    pub(crate) traits: usize,
    pub(crate) pages: usize,
}
