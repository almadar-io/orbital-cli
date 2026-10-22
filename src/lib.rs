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
    format_schema, parse_schema, resolve_schema_cli, run_convert,
    run_i18n_check,
    run_i18n_vocab, run_tests, run_which, stamp_schema_cli, validate_schema,
};
use utils::evaluate_sexpr;

pub use utils::{parse_schema_auto_typed, TypedParseError};

#[derive(Parser)]
#[command(name = "orb")]
#[command(about = "Orb - The Physics of Software", long_about = None)]
#[command(version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

/// Available shell types for code generation
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellType {
    /// TypeScript/React frontend with Express backend
    Typescript,
    /// Python with FastAPI and PyTorch
    Python,
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
    /// Express.js (the only server; the Hono one is retired)
    #[default]
    Express,
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

    /// Serve the language to an agent: an MCP server on stdin/stdout whose tools are
    /// orb's own commands (validate, complete, behaviors, describe, play, test, evaluate, …).
    Mcp {
        /// Also serve this program's declared inbound hooks as tools: a call is delivered as the
        /// hook's request and answered with the handler's answer
        #[arg(long, value_name = "FILE")]
        program: Option<PathBuf>,
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

        /// With `--steps`: run the steps on the native program host, so their os/* and http asks are answered
        /// by this machine instead of left pending.
        #[arg(long, requires = "steps")]
        live: bool,
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

        /// Server framework for backend code generation (express)
        #[arg(long, default_value = "express")]
        server: ServerFrameworkArg,

        /// Who runs the generated TypeScript app: the embedded kernel (the only host)
        #[arg(long, default_value = "kernel")]
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

        /// Server framework for the TypeScript backend (express).
        #[arg(long, value_enum, default_value_t = emit::EmitServer::Express)]
        server: emit::EmitServer,

        /// Emit a Source Map v3 sidecar (`<output>.map`) that maps the
        /// generated code back to byte ranges in the `.orb` source.
        /// When writing to stdout this appends a `//# sourceMappingURL=…`
        /// inline comment instead (TypeScript target only).
        /// Has no effect on `target = orb` or `--format json|raw`.
        #[arg(long)]
        map: bool,

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

    /// Run a long-lived program: boot it on the substrate host, mount INIT, and keep it
    /// running while it has an open stream or listener; SIGINT/SIGTERM cancels them and exits.
    Run {
        /// The `.lolo`/`.orb` program.
        program: PathBuf,
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



}

#[derive(Subcommand)]
enum VectorsCommand {
    /// Convert a legacy JSON embedding manifest (`behaviors-embeddings.json`) into the user vector
    /// store, keeping a vector only when the text the package builds now hashes to the one the
    /// calibrate text index recorded for it. Run it from the project that installs the package.
    Import {
        /// The manifest file, inside an installed behavior package.
        #[arg(long)]
        from: std::path::PathBuf,
        /// The calibrate text index (`<derived cache>/embed-index/<sha256(abs manifest path)>.json`);
        /// defaults to that path.
        #[arg(long)]
        index: Option<std::path::PathBuf>,
    },
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir()?;
    let store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    if let Some(command) = commands::program_command::resolve_program_command(&args, &cwd, &store)? {
        let answer = commands::program_command::run_command_program(&command.program, &command.command, &command.argv, &cwd)?;
        answer.write_to(&mut std::io::stdout(), &mut std::io::stderr())?;
        std::process::exit(answer.code);
    }

    let cli = Cli::parse();

    let needs_language_data = !matches!(
        &cli.command,
        None | Some(Commands::Validate { .. })
            | Some(Commands::I18n { .. })
            | Some(Commands::Update { .. })
            | Some(Commands::Add { .. })
            | Some(Commands::Remove { .. })
            | Some(Commands::List { .. })
            | Some(Commands::Mcp { .. })
            | Some(Commands::EmitTrace { .. })
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
            live,
            ..
        }) => execute_cascade_steps_cli(&path, &steps, live),
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
        Some(Commands::Serve { .. }) => anyhow::bail!("orb serve is temporarily unavailable: the legacy server is archived; the native std-http-server caller integration remains to be completed"),
        Some(Commands::Gui { path, server, renderer }) => utils::desktop::window(&renderer, &path, Some(&server)),
        Some(Commands::Dev { .. }) => anyhow::bail!("orb dev is temporarily unavailable: the legacy server is archived; the native server and desktop lifecycle integration remains to be completed"),
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
                no_vectors,
            } => commands::run_catalog(paths, exposure, json, no_vectors),
            BehaviorsCommand::Texts { paths, kind, json: _ } => commands::run_texts(paths, kind),
            BehaviorsCommand::Describe {
                spec,
                json,
                knob_vectors,
            } => commands::run_describe(spec, json, knob_vectors),
            BehaviorsCommand::Vectors { command } => match command {
                VectorsCommand::Import { from, index } => {
                    commands::behaviors_vectors_import::run_import(from, index)
                }
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
        Some(Commands::Mcp {
            program,
            command: None,
        }) => commands::mcp::run_mcp(program.as_deref()),
        Some(Commands::Reference { topic }) => commands::reference::run_reference(topic.as_deref()),
        Some(Commands::Mcp {
            command: Some(McpCommand::Install { client }),
            ..
        }) => {
            let (path, warning) = commands::mcp::install(&std::env::current_dir()?, client, || {
                crate::utils::patterns::ensure_language_ready().map_err(|e| format!("{}: {e}", e.code()))
            })?;
            println!("Registered `orb mcp` in {}", path.display());
            if let Some(warning) = warning {
                eprintln!("orb: {warning}");
            }
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
            server,
            map,
            unit,
            out,
            sign_key,
            key_id,
        }),
        Some(Commands::EmitTrace { position }) => emit::trace::run(&position),
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
