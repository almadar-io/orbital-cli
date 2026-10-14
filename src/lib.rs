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

use commands::{
    compile_schema, execute_cascade_cli, execute_cascade_steps_cli, execute_schema_cli,
    format_schema, parse_schema, resolve_schema_cli, run_convert, run_dev_mode,
    run_engines_install, run_engines_list, run_engines_remove, run_engines_update, run_i18n_check,
    run_i18n_vocab, run_language_which, run_serve, run_tests, run_which, simulate_render, stamp_schema_cli, validate_schema,
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
    /// Rust native application with an Axum server and a Dioxus (or TUI) client
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
            ShellType::Mobile => "mobile",
            ShellType::Rust => "rust",
            ShellType::Android => "android",
            ShellType::Ios => "ios",
            ShellType::ChromeExtension => "chrome",
        }
    }
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

/// Who runs a generated TypeScript app's execution steps
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum HostArg {
    /// The generated per-event handlers and hooks (default)
    #[default]
    Generated,
    /// The embedded `@almadar/kernel` (Express server only)
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

        /// Simulate render output — trace every reachable state and report what render-ui effects fire
        #[arg(long)]
        simulate: bool,

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

    /// Serve the language to an agent: an MCP server on stdin/stdout whose tools are
    /// orb's own commands (validate, complete, behaviors, describe, play, test, evaluate, …).
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },

    /// The language reference, generated from the compiler: no topic lists the
    /// constructs, vocabulary and validation-code topics; a topic prints one
    /// construct (with real examples from the installed std), `vocabulary`, or
    /// `codes/<category>`.
    Reference {
        topic: Option<String>,
    },

    /// Print the whole language catalog as JSON, generated from the compiler and the
    /// installed language data: every construct, the vocabulary, every validation
    /// code with its description, every operator and every pattern.
    Signatures {
        /// Write to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// What is valid at a cursor position in a `.lolo` source, as JSON
    /// (`orbital_lolo::complete`): keywords, effect operators, pattern tags and
    /// props, and installed behaviors' import paths, traits, orbitals and typed
    /// config keys. Candidates only — ranking is the caller's.
    Complete {
        /// The `.lolo` file (omit with --stdin to read the unsaved buffer).
        file: Option<PathBuf>,
        /// Byte offset of the cursor.
        #[arg(long)]
        offset: usize,
        /// Read the source from stdin instead of a file.
        #[arg(long)]
        stdin: bool,
    },

    /// Where each inline behavior embed sits in a `.lolo`, as JSON: the
    /// `Inline<Short><N>` trait name the `.orb` carries, the embedded
    /// behavior, and the 1-based line and character column of its tag.
    Embeds {
        /// The `.lolo` file (omit with --stdin to read the unsaved buffer).
        file: Option<PathBuf>,
        /// Read the source from stdin instead of a file.
        #[arg(long)]
        stdin: bool,
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

    /// Lower each `.lolo` (or read each `.orb`) to its program exactly as the
    /// loader does for a live behavior package — one NDJSON line per file,
    /// `{"path", "orb"}` or `{"path", "error"}`, in argument order. The single
    /// lowering entry point TS consumers use (Almadar_Behavior_Packages.md §15).
    Lower {
        /// The `.lolo` / `.orb` files to lower.
        #[arg(value_name = "FILE", required = true)]
        paths: Vec<PathBuf>,
    },

    /// Pure headless state-machine execution: fire one event through every
    /// inline trait and report transitions/effects/guard evaluations as JSON.
    ///
    /// Effects are reported, never executed — this is the Rust-side twin of
    /// `packages/almadar-parity`'s TypeScript harness (`ts-harness.ts`
    /// `executeWithRuntime`), so the two can be diffed for cross-
    /// implementation parity. Resolves imports/refs first (same as
    /// `orb resolve`), so both harnesses see the identical inlined trait set.
    Execute {
        /// Path to the .orb schema file
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// Event to fire (e.g. INIT)
        #[arg(long, required_unless_present = "steps")]
        event: Option<String>,

        /// Event payload as a JSON object (default: {})
        #[arg(long)]
        payload: Option<String>,

        /// Initial entity data as a JSON object (default: {})
        #[arg(long)]
        entity: Option<String>,

        /// Only execute the trait with this name
        #[arg(long = "trait")]
        trait_name: Option<String>,

        /// Output the execution result DTO as JSON (for programmatic consumption)
        #[arg(long)]
        json: bool,

        /// Run the event through the kernel's full dispatch (listens fan-out, executed effects)
        /// and print each processed trait's outcome as JSON. Requires `--trait`.
        #[arg(long, requires = "trait_name")]
        cascade: bool,

        /// A JSON array of `{trait, event, payload, from?}` run as `--cascade` does, in order
        /// against one kernel, in place of `--event`/`--trait`. A step with `from` plays from
        /// that state. Each outcome reports the arm that fired, every guard tried and how each
        /// authored effect of the fired arm ran.
        #[arg(long, conflicts_with_all = ["event", "trait_name", "payload"])]
        steps: Option<String>,
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

    /// Verify an app across up to three engines that play the SAME circuit:
    /// `--rust` in-process (default when no engine flag is given, preserving
    /// today's behavior exactly), `--runtime` (JS interpreter via playground),
    /// `--compiled` (codegen'd TS app), or `--all` for all three. Pass a FILE, a
    /// bare behavior NAME (resolved against the live registry), or `--registry
    /// [--topic <t>]` to sweep the std/behaviors corpus.
    Verify {
        /// Path to a single .orb/.lolo schema file, or a bare behavior name
        /// resolved against the registry corpus (omit to use --registry).
        #[arg(value_name = "TARGET")]
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

        /// Layer 3: render each organism's desktop UI to a PNG in this directory
        /// (requires `orb` built with `--features screenshot`).
        #[arg(long, value_name = "DIR")]
        screenshot: Option<PathBuf>,

        /// Layer 3 (per-frame): drive each state transition and capture a PNG per
        /// frame + a frames.json manifest for the vision-review workflow.
        #[arg(long, value_name = "DIR")]
        screenshot_frames: Option<PathBuf>,

        /// Also run Layer 2 over real HTTP against an in-process `orbital-server`
        /// (server↔client render parity, not just the fused in-process kernel).
        #[arg(long)]
        server: bool,

        /// Additionally emit a native shell to a temp dir and run its compile
        /// gate: `python` (real `py_compile` syntax gate) | `android`
        /// (Kotlin structural gate) | `ios` (Swift structural gate). With
        /// `--compiled`, `android`/`ios` also walk the app on the emulator /
        /// Simulator (engine `compiled:android` / `compiled:ios`).
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,

        /// Tier: seconds, no browser/node — parse + validate + Rust kernel
        /// circuit walk only. Mutually exclusive with --deep.
        #[arg(long, conflicts_with = "deep")]
        smoke: bool,

        /// Tier: everything the selected engines offer, including browser
        /// layers (forces --server and a TypeScript --shell gate unless one was
        /// already given). Mutually exclusive with --smoke.
        #[arg(long, conflicts_with = "smoke")]
        deep: bool,

        /// Engine: orbital-rust in-process server+client (default when no
        /// engine flag is given).
        #[arg(long)]
        rust: bool,

        /// Engine: JS interpreter via playground — shells out to
        /// tools/runtime-verify.
        #[arg(long)]
        runtime: bool,

        /// Engine: emitted TS app, 4-phase pipeline — shells out to
        /// orbital-verify-unified. With `--shell ios|android` it walks the
        /// native app on the Simulator/emulator instead of the web client.
        #[arg(long)]
        compiled: bool,

        /// Engine: run all three (rust + runtime + compiled).
        #[arg(long)]
        all: bool,

        /// Print one unified report (circuit vocabulary, engine-tagged rows) to
        /// stdout; all human-readable lines move to stderr.
        #[arg(long)]
        json: bool,

        /// Base port for spawned node engines (runtime/compiled); each target
        /// gets an offset so a multi-target sweep never collides.
        #[arg(long, default_value = "4100")]
        port: u16,

        /// Minimum transition coverage ratio, forwarded to engines that
        /// support it (today: --runtime).
        #[arg(long, value_name = "RATIO")]
        min_coverage: Option<f64>,

        /// Watchdog budget in seconds for each spawned node engine
        /// (runtime/compiled), overriding the tier default (120 smoke /
        /// 600 standard / 1800 deep).
        #[arg(long, value_name = "SECS")]
        engine_timeout: Option<u64>,

        /// Walk the target's declared `[identity]` roster (task #129):
        /// re-runs the L1 kernel circuit once per persona, binding `@user` to
        /// each in turn, and reports a per-persona pass/fail with declared
        /// access-policy denials bucketed separately from genuine failures —
        /// no more manually re-running with one ALMADAR_PERSONA at a time and
        /// eyeballing which denial was expected. Targets with no `[identity]`
        /// entity fall back to today's single env-driven run. Only supported
        /// with the default engine/tier (no --runtime/--compiled/--all/
        /// --smoke/--deep/--json).
        #[arg(long)]
        all_personas: bool,

        /// Verify only this trait's `(from, event)` pairs (repeatable or
        /// comma-separated: `--trait A --trait B` / `--trait A,B`) instead of the
        /// target's full walk. Matches both the compiled, possibly orbital-prefixed
        /// name and the bare `.lolo`/import name a trait carries post-resolve.
        /// Composes with --smoke/--deep/--server/--report-dir. A name matching no
        /// trait in the target is an error listing every available trait name.
        #[arg(long = "trait", value_name = "NAME", value_delimiter = ',')]
        traits: Vec<String>,
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

        /// Who runs the generated TypeScript app: the generated handlers and hooks, or the embedded kernel
        #[arg(long, default_value = "generated")]
        host: HostArg,

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

        /// Serve without the Firebase emulators: no dev personas, every request anonymous
        #[arg(long)]
        no_emulators: bool,
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

        /// Renderer: dioxus (window) or tui (terminal)
        #[arg(long, default_value = "dioxus")]
        renderer: String,
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
        /// Output target: `orb`, `ts`, `py`, or `lolo`.
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

    /// Language i18n vocabulary — inspect the baked `.lolo`/`.orb` vocabulary
    /// tables and check translation coverage.
    I18n {
        #[command(subcommand)]
        command: I18nCommand,
    },

    /// Behavior packages — install/remove/update/inspect the npm packages
    /// `uses` resolves against (Almadar_Behavior_Packages.md).
    Behaviors {
        #[command(subcommand)]
        command: BehaviorsCommand,
    },

    /// The two `orb verify` node engines (`--runtime`/`--compiled`) — ordinary
    /// npm packages resolved node-style, same mechanism as `orb behaviors`.
    Engines {
        #[command(subcommand)]
        command: EnginesCommand,
    },

    /// The language data `orb` compiles against — `@almadar/core` (patterns,
    /// theme presets, fixtures) and `@almadar/std`'s operator table — installed,
    /// pinned npm packages (Almadar_Behavior_Packages.md §14).
    Language {
        #[command(subcommand)]
        command: LanguageCommand,
    },

}

#[derive(Subcommand)]
enum LanguageCommand {
    /// Which copy of each language package compiles here.
    Which,
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
    },
    /// Describe one trait of an installed behavior: knobs (with embedding
    /// vectors), events, embedding model.
    Describe {
        /// `<behavior>.traits.<Trait>`, or `-` to read a trait value (JSON) from stdin.
        spec: String,
        /// Print as JSON (`TraitDescription`).
        #[arg(long)]
        json: bool,
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

#[derive(Subcommand)]
enum EnginesCommand {
    /// Install the `orb verify` node engines (`bun add -d <pkg>@<pin>`),
    /// pinned to the versions this `orb` was baked against. Neither
    /// `--runtime` nor `--compiled` given installs both. Verifies the
    /// installed package's version + declared `bin` entry, removing what bun
    /// added on a failed check, then installs the Chromium browser bun
    /// engines need (skip with `--no-browsers`).
    Install {
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
        /// Install only the runtime engine (`@almadar-io/runtime-verify-tool`).
        #[arg(long)]
        runtime: bool,
        /// Install only the compiled engine (`@almadar-io/verify-tool`).
        #[arg(long)]
        compiled: bool,
        /// Skip the Chromium browser install step.
        #[arg(long)]
        no_browsers: bool,
    },
    /// List the resolved engines: package, version, bin entry, path, link.
    List {
        /// Print as a JSON array instead of a table.
        #[arg(long)]
        json: bool,
        /// List the user store (`~/.orb`) instead of the working directory's walk.
        #[arg(long)]
        global: bool,
    },
    /// Re-install the engines at the versions this `orb` was baked against.
    /// Neither `--runtime` nor `--compiled` given updates both.
    Update {
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
        /// Update only the runtime engine.
        #[arg(long)]
        runtime: bool,
        /// Update only the compiled engine.
        #[arg(long)]
        compiled: bool,
    },
    /// Remove an installed engine (`bun remove <pkg>`). Neither `--runtime`
    /// nor `--compiled` given removes both.
    Remove {
        /// Target the user store (`~/.orb`) instead of the nearest project.
        #[arg(long)]
        global: bool,
        /// Remove only the runtime engine.
        #[arg(long)]
        runtime: bool,
        /// Remove only the compiled engine.
        #[arg(long)]
        compiled: bool,
    },
}

#[derive(Subcommand)]
enum I18nCommand {
    /// Print the `.lolo` parser vocabulary (keywords/shapes/tags/categories/
    /// capabilities/annotations/sigils/effects/types/units/literals/
    /// reservedEvents) plus the `.orb` structural key list and the operator
    /// table.
    Vocab {
        /// Print as JSON instead of a human-readable listing.
        #[arg(long)]
        json: bool,
    },
    /// Check translation coverage: every non-English language's tables
    /// against `en` (missing/orphan/empty keys), and the `orbital-lolo`
    /// parser vocabulary against `en`. Exits 1 and lists every problem when
    /// any are found.
    Check,
}

#[derive(Subcommand)]
enum McpCommand {
    /// Register `orb mcp` in this project's config for an MCP client
    Install {
        #[arg(long, value_enum)]
        client: commands::mcp::McpClient,
    },
}

/// `orb complete`: the completion candidates at `offset`, as JSON on stdout.
fn print_completions(
    file: Option<&std::path::Path>,
    offset: usize,
    stdin: bool,
) -> anyhow::Result<()> {
    let source = match (file, stdin) {
        (_, true) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
        (Some(path), false) => std::fs::read_to_string(path)?,
        (None, false) => anyhow::bail!("orb complete: pass a .lolo file or --stdin"),
    };
    println!("{}", serde_json::to_string(&completions(&source, offset)?)?);
    Ok(())
}

/// What is valid at `offset` in a `.lolo` source, against the installed behaviors.
pub(crate) fn completions(source: &str, offset: usize) -> anyhow::Result<serde_json::Value> {
    let cwd = std::env::current_dir()?;
    let user_store =
        orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    let packages = orbital_core::behavior_packages::discover(&cwd, &user_store)
        .map_err(|e| anyhow::anyhow!("failed to discover behavior packages: {e}"))?;
    let contracts = orbital_lolo::registry::LoloContracts::load(&packages).ok();
    let result = orbital_lolo::complete::complete(source, offset, contracts.as_ref());
    Ok(serde_json::to_value(&result)?)
}

fn print_embeds(file: Option<&std::path::Path>, stdin: bool) -> anyhow::Result<()> {
    let source = match (file, stdin) {
        (_, true) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
        (Some(path), false) => std::fs::read_to_string(path)?,
        (None, false) => anyhow::bail!("orb embeds: pass a .lolo file or --stdin"),
    };
    let embeds = orbital_lolo::embeds::inline_embeds(&source)
        .map_err(|e| anyhow::anyhow!("orb embeds: {e}"))?;
    println!("{}", serde_json::to_string(&embeds)?);
    Ok(())
}

fn print_lolo_signatures(out: Option<&std::path::Path>) -> anyhow::Result<()> {
    let catalog = serde_json::to_string_pretty(&commands::reference::everything()?)?;
    match out {
        Some(path) => {
            std::fs::write(path, &catalog)?;
            eprintln!("Wrote {} bytes to {}", catalog.len(), path.display());
        }
        None => println!("{catalog}"),
    }
    Ok(())
}

/// The loader's `.lolo` lowerer: a relative import of a `.lolo`-only sibling.
fn lower_lolo_file(path: &std::path::Path) -> Result<orbital_core::schema::OrbitalSchema, String> {
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
    // Namespaced structured logging (mirror of @almadar/logger). Quiet by default
    // (warn/error only); enable namespaces with e.g. ALMADAR_DEBUG='almadar:runtime:*'.
    orbital_core::logging::init();
    orbital_compiler::loader::register_source_lowerer(lower_lolo_file);
    orbital_compiler::phases::reflect::register_trait_describer(describe_trait_value);

    let cli = Cli::parse();

    let needs_language_data = !matches!(
        &cli.command,
        None | Some(Commands::Validate { .. })
            | Some(Commands::I18n { .. })
            | Some(Commands::Engines { .. })
            | Some(Commands::Language { .. })
            | Some(Commands::Update { .. })
            | Some(Commands::Add { .. })
            | Some(Commands::Remove { .. })
            | Some(Commands::List { .. })
            | Some(Commands::Login)
            | Some(Commands::Mcp { .. })
            | Some(Commands::EmitTrace { .. })
            | Some(Commands::Trace { .. })
            | Some(Commands::Eval { .. })
            | Some(Commands::Behaviors {
                command: BehaviorsCommand::Which { .. }
            })
    );
    if needs_language_data {
        crate::utils::require_language_data()?;
    }
    let notify = !matches!(
        &cli.command,
        Some(Commands::Update { .. }) | Some(Commands::List { json: true, .. })
    );
    let result = dispatch(cli);
    if notify && result.is_ok() {
        commands::update_notice::notice();
    }
    result
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Commands::Validate {
            path,
            json,
            patterns,
            services,
            simulate,
            deny_warnings,
            final_program,
        }) => {
            validate_schema(
                &path,
                json,
                patterns.as_deref(),
                services.as_deref(),
                deny_warnings,
                final_program,
            )?;
            if simulate {
                simulate_render(&path, patterns.as_deref())?;
            }
            Ok(())
        }
        Some(Commands::Parse { path }) => parse_schema(&path),
        Some(Commands::Signatures { out }) => print_lolo_signatures(out.as_deref()),
        Some(Commands::Complete {
            file,
            offset,
            stdin,
        }) => print_completions(file.as_deref(), offset, stdin),
        Some(Commands::Embeds { file, stdin }) => print_embeds(file.as_deref(), stdin),
        Some(Commands::Resolve {
            path,
            output,
            verbose,
        }) => resolve_schema_cli(&path, output.as_deref(), verbose),
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
        Some(Commands::Execute {
            path,
            steps: Some(steps),
            ..
        }) => execute_cascade_steps_cli(&path, &steps),
        Some(Commands::Execute {
            path,
            event: Some(event),
            payload,
            trait_name: Some(trait_name),
            cascade: true,
            ..
        }) => execute_cascade_cli(&path, &event, payload.as_deref(), &trait_name),
        Some(Commands::Execute {
            path,
            event: Some(event),
            payload,
            entity,
            trait_name,
            json,
            ..
        }) => execute_schema_cli(
            &path,
            &event,
            payload.as_deref(),
            entity.as_deref(),
            trait_name.as_deref(),
            json,
        ),
        Some(Commands::Execute { event: None, .. }) => {
            Err(anyhow::anyhow!("orb execute needs --event or --steps"))
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
        Some(Commands::Verify {
            path,
            registry,
            topic,
            report_dir,
            screenshot,
            screenshot_frames,
            server,
            shell,
            smoke,
            deep,
            rust,
            runtime,
            compiled,
            all,
            json,
            port,
            min_coverage,
            engine_timeout,
            all_personas,
            traits,
        }) => commands::verify::run_verify(commands::verify::VerifyCliArgs {
            path,
            registry,
            topic,
            report_dir,
            screenshot_dir: screenshot,
            frames_dir: screenshot_frames,
            server,
            shell,
            smoke,
            deep,
            rust,
            runtime,
            compiled,
            all,
            json,
            port,
            min_coverage,
            engine_timeout,
            all_personas,
            traits,
        }),
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
            host,
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
            host,
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
            no_emulators,
        }) => run_serve(
            &path,
            port,
            open,
            no_build,
            patterns.as_deref(),
            !no_emulators,
        ),
        Some(Commands::Gui { path, server, renderer }) => utils::desktop::window(&renderer, &path, Some(&server)),
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
        Some(Commands::I18n { command }) => match command {
            I18nCommand::Vocab { json } => run_i18n_vocab(json),
            I18nCommand::Check => run_i18n_check(),
        },
        Some(Commands::Behaviors { command }) => match command {
            BehaviorsCommand::Which { spec } => run_which(spec),
            BehaviorsCommand::Catalog {
                paths,
                exposure,
                json,
            } => commands::run_catalog(paths, exposure, json),
            BehaviorsCommand::Describe { spec, json } => commands::run_describe(spec, json),
            BehaviorsCommand::Source { spec, json } => commands::run_source(spec, json),
            BehaviorsCommand::Expects { spec, json } => commands::run_expects(spec, json),
        },
        Some(Commands::Engines { command }) => match command {
            EnginesCommand::Install {
                global,
                runtime,
                compiled,
                no_browsers,
            } => run_engines_install(global, runtime, compiled, no_browsers),
            EnginesCommand::List { json, global } => run_engines_list(json, global),
            EnginesCommand::Update {
                global,
                runtime,
                compiled,
            } => run_engines_update(global, runtime, compiled),
            EnginesCommand::Remove {
                global,
                runtime,
                compiled,
            } => run_engines_remove(global, runtime, compiled),
        },
        Some(Commands::Language { command }) => match command {
            LanguageCommand::Which => run_language_which(),
        },
        Some(Commands::Update { global }) => commands::packages::run_update(global),
        Some(Commands::Add { packages, global }) => commands::packages::run_add(packages, global),
        Some(Commands::Remove { packages, global }) => commands::packages::run_remove(packages, global),
        Some(Commands::List { json, global }) => commands::packages::run_list(json, global),
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
        Some(Commands::Mcp { command: None }) => commands::mcp::run_mcp(),
        Some(Commands::Reference { topic }) => commands::reference::run_reference(topic.as_deref()),
        Some(Commands::Mcp {
            command: Some(McpCommand::Install { client }),
        }) => {
            let path = commands::mcp::install_config(&std::env::current_dir()?, client)?;
            println!("Registered `orb mcp` in {}", path.display());
            Ok(())
        }
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
        // No subcommand: a truly bare `orb` (no trailing words) spawns the
        // interactive agent again (user decision 2026-08-27, reversing part
        // of 2026-07-17's blanket disable). Unmatched/typo'd input (non-empty
        // `cli.prompt`) still must never silently become an agent prompt —
        // that case stays gated behind ORB_AGENT_FALLBACK=1.
        None => {
            if cli.list {
                spawn_agent_with_args(&["status"])
            } else if cli.prompt.is_empty()
                || std::env::var("ORB_AGENT_FALLBACK").is_ok_and(|v| v == "1")
            {
                spawn_agent(&cli)
            } else {
                anyhow::bail!(
                    "unknown command {:?}. The free-prompt agent fallback is disabled for unmatched input — run `orb --help` for commands, or set ORB_AGENT_FALLBACK=1 to opt in.",
                    cli.prompt.join(" ")
                )
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
