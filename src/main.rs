//! Orbital CLI
//!
//! Thin command-line interface for Orbital tools.
//! All heavy lifting is delegated to orbital-core, orbital-server, and orbital-client.

#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    clippy::all,
    unexpected_cfgs
)]

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};

mod emit;
mod i18n;

/// Heuristic to detect language from content
fn detect_language(content: &str) -> Option<String> {
    let definitions = i18n::get_definitions();

    // Check purely based on unique keyword presence
    // This is a simple heuristic. A more robust way would be score-based.
    for (code, def) in definitions {
        if code == "en" {
            continue;
        }

        // check for "orbitals" keyword first (most common root key)
        if let Some(kw) = def.keywords.get("orbitals") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }

        // check for "entity" keyword
        if let Some(kw) = def.keywords.get("entity") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }

        // check for "orbital" keyword
        if let Some(kw) = def.keywords.get("orbital") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }
    }

    None
}

/// Copy locale JSON files from {schema_dir}/locales/ into {output}/locales/.
/// The generated App.tsx imports from '../../../locales/{locale}.json' which resolves
/// to {output}/locales/ (3 levels up from packages/client/src/).
fn copy_locales(schema_path: &PathBuf, output: &PathBuf) {
    let schema_dir = match schema_path.parent() {
        Some(d) => d.to_path_buf(),
        None => return,
    };
    let src_locales = schema_dir.join("locales");
    let dest_locales = output.join("locales");
    if let Err(e) = std::fs::create_dir_all(&dest_locales) {
        eprintln!("Warning: could not create locales dir in output: {}", e);
        return;
    }

    if src_locales.exists() {
        // Copy locale files from source
        let entries = match std::fs::read_dir(&src_locales) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                let dest = dest_locales.join(entry.file_name());
                if let Err(e) = std::fs::copy(&p, &dest) {
                    eprintln!("Warning: could not copy locale file {:?}: {}", p, e);
                }
            }
        }
    }

    // Ensure en/ar/sl locale files exist (fallback for standalone behaviors
    // that have no locales directory but the generated App.tsx imports them)
    for lang in &["en", "ar", "sl"] {
        let locale_path = dest_locales.join(format!("{}.json", lang));
        if !locale_path.exists() {
            if let Err(e) = std::fs::write(&locale_path, "{}") {
                eprintln!(
                    "Warning: could not create fallback locale {}.json: {}",
                    lang, e
                );
            }
        }
    }
}

/// Recursively strip serde_json `null` values from objects so the emitted
/// `.orb` JSON matches the sparse hand-authored shape used elsewhere in the
/// repo. Used by the .lolo lowering path before writing to disk.
pub(crate) fn strip_json_nulls(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let stripped = strip_json_nulls(v);
                if !stripped.is_null() {
                    out.insert(k, stripped);
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_json_nulls).collect()),
        v => v,
    }
}

/// Translate schema content from native language to English
fn translate_schema_content(content: &str) -> String {
    if let Some(lang) = detect_language(content) {
        let definitions = i18n::get_definitions();
        if let Some(def) = definitions.get(&lang) {
            eprintln!("Detected language: {} ({})", def.meta.name, lang);
            let mut translated = content.to_string();

            // 1. Translate Keys
            for (eng_key, native_key) in &def.keywords {
                let target = format!("\"{}\":", eng_key);
                let source = format!("\"{}\":", native_key);
                translated = translated.replace(&source, &target);
            }

            // 2. Translate Operators (e.g. "يساوي" -> "eq")
            if let Some(ops) = &def.operators {
                for (eng_op, native_op) in ops {
                    let target = format!("\"{}\"", eng_op);
                    let source = format!("\"{}\"", native_op);
                    translated = translated.replace(&source, &target);
                }
            }

            // 3. Translate Bindings (e.g. "@كيان" -> "@entity")
            // This must handle path access like @كيان.رسالة -> @entity.message
            // But we only translate the ROOT binding name. The fields remain as defined in schema?
            // Actually, the schema fields might be localized too if "name" is localized.
            // But for now, let's just translate the root binding prefixes.
            if let Some(bindings) = &def.bindings {
                for (eng_binding, native_binding) in bindings {
                    // We must be careful not to replace partial matches if prefixes overlap (e.g. @ent vs @entity)
                    // But these designated bindings start with @ so it's safer.
                    // We simply replace all occurrences of the native binding prefix.
                    translated = translated.replace(native_binding, eng_binding);
                }
            }

            // 2. Translate Types (e.g. "type": "نص" -> "type": "string")
            // This is trickier because we need to match the value.
            // We can iterate over types and replace generic string matches if they look like types?
            // Or better: iterate over the *English* type names, finding their native equivalents and replacing them *only if* they appear as values for "type".
            // That requires parsing... which we can't do if it's mixed language.
            // Let's do a rigorous string replacement for types appearing in quotes.
            for (eng_type, native_type) in &def.types {
                // "type": "نص" -> "type": "string"
                // We can't rely on whitespace.
                // Let's just replacing the value string itself if it's unique enough?
                // "نص" is short. Might clobber user data.
                // SAFE APPROACH: We only translate keys for now as proof of concept.
                // Types might need to be exact matches.

                // Let's try replacing `"type": "NATIVE"` with `"type": "ENG"`
                // We need to account for possible whitespace.
                let patterns = vec![
                    format!("\"type\": \"{}\"", native_type),
                    format!("\"type\":  \"{}\"", native_type), // double space
                    format!("\"type\" : \"{}\"", native_type),
                ];

                for p in patterns {
                    translated = translated.replace(&p, &format!("\"type\": \"{}\"", eng_type));
                }
            }

            return translated;
        }
    }

    content.to_string()
}

/// Syntax variant detected from file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputSyntax {
    /// Standard `.orb` JSON
    OrbJson,
    /// LOLO surface language
    Lolo,
}

/// Detect whether a source string is LOLO or `.orb` JSON by inspecting the
/// first non-whitespace character. Every valid `.orb` JSON starts with `{`;
/// LOLO starts with a keyword (`app`, `orbital`, `type`), comment (`#`, `;;`),
/// or other non-brace character.
fn detect_syntax(content: &str) -> InputSyntax {
    let first = content
        .trim_start_matches('\u{FEFF}') // strip UTF-8 BOM
        .trim_start()
        .chars()
        .next();
    match first {
        Some('{') => InputSyntax::OrbJson,
        _ => InputSyntax::Lolo,
    }
}

/// Parse a schema from its raw content, auto-detecting LOLO vs Orb JSON syntax.
/// For Orb JSON, i18n translation is applied before parsing.
/// Returns the parsed `OrbitalSchema`.
fn parse_schema_auto(content: &str) -> anyhow::Result<orbital_core::schema::OrbitalSchema> {
    parse_schema_auto_typed(content).map_err(|e| anyhow::anyhow!("{}", e.describe()))
}

/// Typed parse-error for use by callers that want to map to structured
/// diagnostics (e.g., `orbital validate --json` → `ORB_T_TYPE_*` codes).
/// `parse_schema_auto` wraps this in `anyhow` for legacy callers.
pub enum TypedParseError {
    Lolo(orbital_lolo::LoloError),
    Json(String),
}

impl TypedParseError {
    /// Human-readable single-line message for CLI stderr or anyhow wrappers.
    pub fn describe(&self) -> String {
        match self {
            TypedParseError::Lolo(e) => format!("LOLO parse error: {}", e.render()),
            TypedParseError::Json(msg) => format!("JSON parse error: {msg}"),
        }
    }
}

/// Like `parse_schema_auto`, but preserves the underlying error shape so
/// callers can surface typed diagnostics (e.g., `ORB_T_TYPE_ANNOTATION_MISMATCH`
/// when the lowerer's resolver rejected a `::` annotation).
pub fn parse_schema_auto_typed(
    content: &str,
) -> Result<orbital_core::schema::OrbitalSchema, TypedParseError> {
    use orbital_core::schema::parse_schema;
    match detect_syntax(content) {
        InputSyntax::Lolo => orbital_lolo::parse_and_lower_str(content).map_err(TypedParseError::Lolo),
        InputSyntax::OrbJson => {
            let translated = translate_schema_content(content);
            parse_schema(&translated).map_err(|e| TypedParseError::Json(e.to_string()))
        }
    }
}

/// Map a `TypedParseError` into one or more `ValidationJsonError` rows so
/// `orbital validate --json` can surface typed codes. The hint format
/// matches the 💡-style suggestion column used by the rest of the validator.
pub(crate) fn typed_parse_error_to_json(e: &TypedParseError) -> Vec<ValidationJsonError> {
    use orbital_lolo::error::TypeError;
    use orbital_lolo::{LoloError, LowerError};

    match e {
        TypedParseError::Json(msg) => vec![ValidationJsonError {
            code: "JSON_PARSE_ERROR".to_string(),
            path: "".to_string(),
            message: format!("Failed to parse schema: {msg}"),
            suggestion: Some("Check JSON syntax and structure".to_string()),
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
        }],
        TypedParseError::Lolo(LoloError::Io { path, message }) => vec![ValidationJsonError {
            code: "LOLO_IO_ERROR".to_string(),
            path: path.clone(),
            message: message.clone(),
            suggestion: None,
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
        }],
        TypedParseError::Lolo(LoloError::Parse { path, inner }) => {
            let span = inner.span();
            vec![ValidationJsonError {
                code: "LOLO_PARSE_ERROR".to_string(),
                path: format!("{path}:{}:{}", span.line, span.col),
                message: format!("{inner}"),
                suggestion: None,
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
            }]
        }
        TypedParseError::Lolo(LoloError::Lower {
            path,
            inner: LowerError::Type(te),
        }) => {
            let span = te.span();
            let path_loc = format!("{path}:{}:{}", span.line, span.col);
            let (code, suggestion) = match te {
                TypeError::UnknownType { suggestion, .. } => (
                    "ORB_T_TYPE_UNKNOWN",
                    suggestion
                        .as_ref()
                        .map(|s| format!("did you mean {s:?}?")),
                ),
                TypeError::CircularType { .. } => (
                    "ORB_T_TYPE_CYCLE",
                    Some(
                        "break the cycle by removing one reference in the composition chain"
                            .to_string(),
                    ),
                ),
                TypeError::KindMismatch { .. } => (
                    "ORB_T_TYPE_KIND_MISMATCH",
                    Some("composed types must share a kind (all Entity, all Trait, …)".to_string()),
                ),
                TypeError::CompositionConflict { field, .. } => (
                    "ORB_T_TYPE_COMPOSITION_CONFLICT",
                    Some(format!(
                        "remove field {field:?} from one side or reconcile its type"
                    )),
                ),
                TypeError::AnnotationMismatch { .. } => (
                    "ORB_T_TYPE_ANNOTATION_MISMATCH",
                    Some(
                        "update the body to match the annotated type or drop the `::` annotation"
                            .to_string(),
                    ),
                ),
                TypeError::UnknownTypeParam { name, alias, .. } => (
                    "ORB_T_TYPE_UNKNOWN_PARAM",
                    Some(format!(
                        "type parameter {name:?} is not declared on alias {alias:?}"
                    )),
                ),
                TypeError::WrongTypeArgCount { expected, got, .. } => (
                    "ORB_T_TYPE_WRONG_ARG_COUNT",
                    Some(format!("supply exactly {expected} type argument(s), got {got}")),
                ),
                TypeError::EventTypeWrongKind { name, actual, .. } => (
                    "ORB_T_EVENT_TYPE_WRONG_KIND",
                    Some(format!(
                        "declare {name:?} with `type {name} = Event<...>`, got a {actual} instead"
                    )),
                ),
            };
            vec![ValidationJsonError {
                code: code.to_string(),
                path: path_loc,
                message: format!("{te}"),
                suggestion,
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
            }]
        }
        TypedParseError::Lolo(LoloError::Lower { path, inner }) => vec![ValidationJsonError {
            code: "LOLO_LOWER_ERROR".to_string(),
            path: path.clone(),
            message: format!("{inner}"),
            suggestion: None,
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
        }],
    }
}

#[derive(Parser)]
#[command(name = "orb")]
#[command(about = "Orb - The Physics of Software", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Initial prompt for the agent (when no subcommand is given)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    prompt: Vec<String>,

    /// LLM provider override
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Autonomy level: full, balanced, cautious
    #[arg(long, global = true)]
    autonomy: Option<String>,

    /// Resume a previous session
    #[arg(long, global = true)]
    resume: Option<Option<String>>,

    /// Resume the most recent session
    #[arg(long, global = true)]
    last: bool,

    /// Cost budget in USD
    #[arg(long, global = true)]
    budget: Option<f64>,

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
        Some(Commands::Resolve { path, output, verbose }) => resolve_schema_cli(&path, output.as_deref(), verbose),
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
// Agent Spawning
// ============================================================================

fn find_bun() -> anyhow::Result<PathBuf> {
    // 1. ORB_BUN_PATH env var
    if let Ok(p) = std::env::var("ORB_BUN_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
    }

    // 2. Sibling to current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bun_name = if cfg!(windows) { "bun.exe" } else { "bun" };
            let sibling = dir.join(bun_name);
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }

    // 3. PATH lookup
    if let Ok(output) = Command::new("which").arg("bun").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!("Bun runtime not found. Install Bun (https://bun.sh) or set ORB_BUN_PATH.")
}

fn find_agent_entry() -> anyhow::Result<PathBuf> {
    // 1. ORB_AGENT_DIR env var
    if let Ok(dir) = std::env::var("ORB_AGENT_DIR") {
        let entry = PathBuf::from(&dir).join("cli.js");
        if entry.exists() {
            return Ok(entry);
        }
    }

    // 2. agent/ relative to current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let entry = dir.join("agent").join("cli.js");
            if entry.exists() {
                return Ok(entry);
            }
        }
    }

    anyhow::bail!(
        "Agent bundle not found. Set ORB_AGENT_DIR or place agent/cli.js next to the orb binary."
    )
}

fn spawn_agent(cli: &Cli) -> anyhow::Result<()> {
    let bun = find_bun()?;
    let entry = find_agent_entry()?;

    let mut cmd = Command::new(&bun);
    cmd.arg(&entry);

    // Forward prompt words
    if !cli.prompt.is_empty() {
        let prompt = cli.prompt.join(" ");
        cmd.arg(&prompt);
    }

    // Forward flags
    if let Some(ref provider) = cli.provider {
        cmd.arg("--provider").arg(provider);
    }
    if let Some(ref autonomy) = cli.autonomy {
        cmd.arg("--autonomy").arg(autonomy);
    }
    if let Some(ref resume) = cli.resume {
        cmd.arg("--resume");
        if let Some(ref id) = resume {
            cmd.arg(id);
        }
    }
    if cli.last {
        cmd.arg("--last");
    }
    if let Some(budget) = cli.budget {
        cmd.arg("--budget").arg(budget.to_string());
    }

    // Set ORB_BINARY_PATH so agent tools call back to us
    if let Ok(own_exe) = std::env::current_exe() {
        cmd.env("ORB_BINARY_PATH", &own_exe);
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to spawn agent: {}", e))?;

    std::process::exit(status.code().unwrap_or(1));
}

fn spawn_agent_with_args(args: &[&str]) -> anyhow::Result<()> {
    let bun = find_bun()?;
    let entry = find_agent_entry()?;

    let mut cmd = Command::new(&bun);
    cmd.arg(&entry);
    for arg in args {
        cmd.arg(arg);
    }

    if let Ok(own_exe) = std::env::current_exe() {
        cmd.env("ORB_BINARY_PATH", &own_exe);
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to spawn agent: {}", e))?;

    std::process::exit(status.code().unwrap_or(1));
}

// ============================================================================
// JSON Output Types for validate --json
// ============================================================================

/// JSON output for validation results (matches TypeScript DeepAgent tool expectations)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationJsonOutput {
    success: bool,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<Vec<ValidationJsonError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warnings: Option<Vec<ValidationJsonWarning>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<ValidationStats>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationJsonError {
    code: String,
    path: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_shape: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_values: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationJsonWarning {
    code: String,
    path: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationStats {
    orbitals: usize,
    entities: usize,
    traits: usize,
    pages: usize,
}

/// Load extra patterns from a patterns.json file.
///
/// If `explicit_path` is provided, load from that path.
/// Otherwise, auto-discover patterns.json adjacent to the schema file or in parent directories.
fn load_extra_patterns(
    schema_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> Option<orbital_compiler::ExtraPatterns> {
    use std::collections::{HashMap, HashSet};

    let patterns_path = if let Some(p) = explicit_path {
        if p.exists() {
            p.to_path_buf()
        } else {
            eprintln!("Warning: patterns file not found: {}", p.display());
            return None;
        }
    } else {
        // Auto-discover: look for patterns.json adjacent to schema, then parent dirs
        let schema_dir = schema_path.parent()?;
        let candidates = [
            schema_dir.join("patterns.json"),
            schema_dir.join("design-system/patterns.json"),
        ];
        match candidates.into_iter().find(|p| p.exists()) {
            Some(p) => p,
            None => return None,
        }
    };

    // Read and parse patterns.json
    let content = match std::fs::read_to_string(&patterns_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: could not read {}: {}", patterns_path.display(), e);
            return None;
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "Warning: invalid JSON in {}: {}",
                patterns_path.display(),
                e
            );
            return None;
        }
    };

    // Parse into ExtraPatterns
    let mut types = HashSet::new();
    let mut props_schema: HashMap<String, HashSet<String>> = HashMap::new();

    if let Some(patterns) = json.get("patterns").and_then(|v| v.as_object()) {
        for (pattern_name, pattern_def) in patterns {
            types.insert(pattern_name.clone());
            // Support both formats:
            // 1. "props": [{ "name": "propName" }]  (legacy array format)
            // 2. "propsSchema": { "propName": { "types": [...] } }  (sync tool format)
            if let Some(props) = pattern_def.get("props").and_then(|v| v.as_array()) {
                let prop_names: HashSet<String> = props
                    .iter()
                    .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect();
                props_schema.insert(pattern_name.clone(), prop_names);
            } else if let Some(schema) = pattern_def.get("propsSchema").and_then(|v| v.as_object())
            {
                let prop_names: HashSet<String> = schema.keys().cloned().collect();
                props_schema.insert(pattern_name.clone(), prop_names);
            }
        }
    }

    if types.is_empty() {
        return None;
    }

    eprintln!(
        "Loaded {} extra patterns from {}",
        types.len(),
        patterns_path.display()
    );

    Some(orbital_compiler::ExtraPatterns {
        types,
        props_schema,
    })
}

/// Load services registry from services-registry.json.
/// Returns a HashMap mapping service name → set of valid action names.
fn load_services_registry(
    explicit_path: Option<&std::path::Path>,
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    use std::collections::{HashMap, HashSet};

    let registry_path = if let Some(p) = explicit_path {
        if p.exists() {
            p.to_path_buf()
        } else {
            eprintln!("Warning: services registry not found: {}", p.display());
            return HashMap::new();
        }
    } else {
        // Auto-discover: check well-known location in the repo
        let candidates = [std::path::PathBuf::from(
            "packages/almadar-patterns/src/services-registry.json",
        )];
        match candidates.into_iter().find(|p| p.exists()) {
            Some(p) => p,
            None => return HashMap::new(),
        }
    };

    let content = match std::fs::read_to_string(&registry_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: could not read {}: {}", registry_path.display(), e);
            return HashMap::new();
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "Warning: invalid JSON in {}: {}",
                registry_path.display(),
                e
            );
            return HashMap::new();
        }
    };

    let mut service_actions: HashMap<String, HashSet<String>> = HashMap::new();

    if let Some(services) = json.get("services").and_then(|v| v.as_object()) {
        for (service_name, service_def) in services {
            let mut actions = HashSet::new();
            if let Some(action_list) = service_def.get("actions").and_then(|v| v.as_array()) {
                for action in action_list {
                    if let Some(name) = action.get("name").and_then(|v| v.as_str()) {
                        actions.insert(name.to_string());
                    }
                }
            }
            if !actions.is_empty() {
                service_actions.insert(service_name.clone(), actions);
            }
        }
    }

    if !service_actions.is_empty() {
        eprintln!(
            "Loaded {} services from {}",
            service_actions.len(),
            registry_path.display()
        );
    }

    service_actions
}

/// Load project-specific component mappings from patterns.json for code generation.
/// This returns a HashMap suitable for passing to TypeScriptBackend.extra_mappings.
fn load_project_component_mappings(
    schema_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> Option<std::collections::HashMap<String, orbital_shell_typescript::ExtraComponentMapping>> {
    use std::collections::HashMap;

    let patterns_path = if let Some(p) = explicit_path {
        if p.exists() {
            p.to_path_buf()
        } else {
            return None;
        }
    } else {
        // Auto-discover: same logic as load_extra_patterns
        let schema_dir = schema_path.parent()?;
        let candidates = [
            schema_dir.join("patterns.json"),
            schema_dir.join("design-system/patterns.json"),
        ];
        match candidates.into_iter().find(|p| p.exists()) {
            Some(p) => p,
            None => return None,
        }
    };

    let content = std::fs::read_to_string(&patterns_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let mut mappings = HashMap::new();

    if let Some(patterns) = json.get("patterns").and_then(|v| v.as_object()) {
        for (pattern_name, pattern_def) in patterns {
            // Get component name — either from explicit "component" field or derive from pattern type
            let component = pattern_def
                .get("component")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| kebab_to_pascal(pattern_name));

            let category = pattern_def
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("display")
                .to_string();

            mappings.insert(
                pattern_name.clone(),
                orbital_shell_typescript::ExtraComponentMapping {
                    component,
                    category,
                },
            );
        }
    }

    if mappings.is_empty() {
        return None;
    }

    eprintln!(
        "Loaded {} project component mappings from {}",
        mappings.len(),
        patterns_path.display()
    );

    Some(mappings)
}

/// Convert kebab-case to PascalCase (e.g., "signature-capture" → "SignatureCapture")
fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

/// Validate schema using native Rust validation
fn validate_schema(
    path: &PathBuf,
    json_output: bool,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Load extra patterns (explicit path or auto-discover)
    let extra_patterns = load_extra_patterns(path, patterns_path);

    // Load services registry (explicit path or auto-discover)
    let service_actions = load_services_registry(services_path);

    if json_output {
        return validate_schema_json(path, extra_patterns, service_actions);
    }

    println!("Validating schema: {}", path.display());

    // Use native Rust validation only
    validate_schema_native(path, extra_patterns, service_actions)
}

/// Validate schema with JSON output (for programmatic consumption by DeepAgent)
fn validate_schema_json(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_compiler::phases::{inline_schema, InlineConfig};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    // Step 1: Parse schema (auto-detect LOLO vs JSON syntax)
    let content_raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            // Handle file read error similar to parse error
            let output = ValidationJsonOutput {
                success: true,
                valid: false,
                errors: Some(vec![ValidationJsonError {
                    code: "FILE_READ_ERROR".to_string(),
                    path: "".to_string(),
                    message: format!("Failed to read schema file: {}", e),
                    suggestion: None,
                    fix_guidance: None,
                    expected_shape: None,
                    valid_values: None,
                }]),
                warnings: None,
                stats: None,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }
    };

    let mut schema = match parse_schema_auto_typed(&content_raw) {
        Ok(s) => s,
        Err(e) => {
            // Map parse/lower/type errors into typed `ValidationJsonError`
            // rows — includes the `ORB_T_TYPE_*` family for `.lolo` type
            // check failures so consumers (agent retry loops, editor
            // tooling) can switch on the code instead of regexing a
            // human-readable string.
            let output = ValidationJsonOutput {
                success: true,
                valid: false,
                errors: Some(typed_parse_error_to_json(&e)),
                warnings: None,
                stats: None,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }
    };

    // Run the inline phase BEFORE validation. Composed .orb files carry
    // `uses:` imports and `ref:` strings; without inlining, the validator
    // reports every imported trait/page/entity as "undefined". The inline
    // phase resolves external refs via the embedded std loader and
    // filesystem — same path `orb compile` uses. Validation-only mode used
    // to skip this and produced false positives on every multi-use schema.
    if let Err(e) = inline_schema(&mut schema, &InlineConfig::default(), Some(path.as_path())) {
        // Source-tag from the inline-phase variant: when the producer
        // populated a structured path (e.g. DuplicateTraitName carries
        // `orbitals[name=Foo].traits[idx]`), surface it on the JSON
        // error so consumers (rabit's dispatch-updates owner-routing)
        // get concrete coordinates instead of an empty string. Variants
        // without a path return "" — same as before.
        let inline_path = e.structured_path().to_string();
        let output = ValidationJsonOutput {
            success: true,
            valid: false,
            errors: Some(vec![ValidationJsonError {
                code: "INLINE_ERROR".to_string(),
                path: inline_path,
                message: format!("Failed to resolve external references: {}", e),
                suggestion: Some("Check `uses:` imports resolve to known behaviors".to_string()),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
            }]),
            warnings: None,
            stats: None,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Collect stats
    // Note: Traits are now only defined inline within orbitals, not at schema level.
    let stats = ValidationStats {
        orbitals: schema.orbitals.len(),
        entities: schema.orbitals.len(), // Each orbital has one entity
        traits: schema
            .orbitals
            .iter()
            .map(|o| o.traits.len())
            .sum::<usize>(),
        pages: schema
            .orbitals
            .iter()
            .filter_map(|o| o.pages.as_ref())
            .map(|p| p.len())
            .sum(),
    };

    // Step 2: Run comprehensive schema validation (with language awareness)
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => orbital_compiler::SchemaLanguage::NonLatin(lang),
        _ => orbital_compiler::SchemaLanguage::English,
    };
    let comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);

    let mut all_errors: Vec<ValidationJsonError> = Vec::new();
    let mut all_warnings: Vec<ValidationJsonWarning> = Vec::new();

    // Convert comprehensive validation errors
    for error in &comprehensive_result.errors {
        all_errors.push(ValidationJsonError {
            code: error.code.to_string(),
            path: error.path.clone(),
            message: error.message.clone(),
            suggestion: error.suggestion.clone(),
            fix_guidance: error.fix_guidance.clone(),
            expected_shape: error.expected_shape.clone(),
            valid_values: error.valid_values.clone(),
        });
    }

    for warning in &comprehensive_result.warnings {
        all_warnings.push(ValidationJsonWarning {
            code: warning.code.to_string(),
            path: warning.path.clone(),
            message: warning.message.clone(),
            suggestion: warning.suggestion.clone(),
        });
    }

    // Step 3: Try to compile to OIR for additional validation
    let options = CompilerOptions::default();
    if let Ok(module) = compile_schema(schema, &options) {
        let validator = OirValidator::new(&module);
        let oir_result = validator.validate();

        for error in &oir_result.errors {
            if error.severity == ValidationSeverity::Error {
                all_errors.push(ValidationJsonError {
                    code: "OIR_ERROR".to_string(),
                    path: error.path.clone(),
                    message: error.message.clone(),
                    suggestion: None,
                    fix_guidance: None,
                    expected_shape: None,
                    valid_values: None,
                });
            } else {
                all_warnings.push(ValidationJsonWarning {
                    code: "OIR_WARNING".to_string(),
                    path: error.path.clone(),
                    message: error.message.clone(),
                    suggestion: None,
                });
            }
        }
    }

    let output = ValidationJsonOutput {
        success: true,
        valid: all_errors.is_empty(),
        errors: if all_errors.is_empty() {
            None
        } else {
            Some(all_errors)
        },
        warnings: if all_warnings.is_empty() {
            None
        } else {
            Some(all_warnings)
        },
        stats: Some(stats),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Native Rust schema validation (no kflow dependency)
fn validate_schema_native(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    use orbital_compiler::{
        compile_schema, validate_comprehensive_full, CompilerOptions, SchemaLanguage,
    };
    use orbital_compiler::phases::{inline_schema, enrich_schema};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    // Step 1: Parse schema (auto-detect LOLO vs JSON syntax)
    let content_raw = std::fs::read_to_string(path)?;
    // Detect i18n language BEFORE translation for validation context (GAP-V001)
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => SchemaLanguage::NonLatin(lang),
        _ => SchemaLanguage::English,
    };
    let mut schema = parse_schema_auto(&content_raw)?;

    // Step 1b: Inline + enrich before validation so that `uses`-based trait/entity/page
    // references are resolved and default collection names are present. Without this,
    // validate_comprehensive_full raises false ORB_T_UNDEFINED_TRAIT / ORB_E_MISSING_COLLECTION
    // errors for any schema that uses `uses` declarations.
    let default_opts = CompilerOptions::default();
    // Best-effort: inline errors (e.g. missing std behavior) surface in Step 3 compile.
    let _ = inline_schema(&mut schema, &default_opts.inline, Some(path));
    enrich_schema(&mut schema, &default_opts.enrichment);

    // Step 2: Run comprehensive schema validation (all semantic checks, on resolved schema)
    let comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);

    let mut has_errors = false;

    if !comprehensive_result.is_valid() {
        eprintln!("Schema validation errors:");
        for error in &comprehensive_result.errors {
            eprintln!("  ❌ [{}] {}: {}", error.code, error.path, error.message);
            if let Some(suggestion) = &error.suggestion {
                eprintln!("     💡 {}", suggestion);
            }
        }
        has_errors = true;
    }

    if comprehensive_result.has_warnings() {
        println!("Schema validation warnings:");
        for warning in &comprehensive_result.warnings {
            println!(
                "  ⚠️  [{}] {}: {}",
                warning.code, warning.path, warning.message
            );
            if let Some(suggestion) = &warning.suggestion {
                println!("     💡 {}", suggestion);
            }
        }
    }

    // Step 3: Compile to OIR (validates references)
    // Skip inline + enrich since Step 1b already ran them on this schema.
    let mut options = CompilerOptions::default();
    options.inline.skip_inline = true;
    let module = match compile_schema(schema, &options) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Compilation error: {}", e);
            anyhow::bail!("Schema compilation failed: {}", e);
        }
    };

    // Step 4: Validate OIR (post-resolution semantic validation)
    let mut validator = OirValidator::new(&module);
    let oir_result = validator.validate();

    if !oir_result.is_valid() {
        eprintln!("OIR validation errors:");
        for error in &oir_result.errors {
            eprintln!("  ❌ {}", error);
        }
        has_errors = true;
    } else if oir_result.has_warnings() {
        println!("OIR warnings:");
        for warning in oir_result
            .errors
            .iter()
            .filter(|e| e.severity == ValidationSeverity::Warning)
        {
            println!("  ⚠️  {}", warning);
        }
    }

    if has_errors {
        anyhow::bail!(
            "Schema validation failed with {} error(s)",
            comprehensive_result.error_count() + oir_result.error_count()
        )
    } else {
        Ok(())
    }
}

/// Simulate render output — trace every reachable state and report what render-ui effects fire.
/// This is the compile-time equivalent of running the app and inspecting what renders.
fn simulate_render(path: &PathBuf, patterns_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    use orbital_core::schema::{SExpression, TraitReference};
    use std::collections::{HashMap, HashSet, VecDeque};

    let content_raw = std::fs::read_to_string(path)?;
    let schema = parse_schema_auto(&content_raw)?;

    println!("\n🔍 Render Simulation");
    println!("{}", "=".repeat(60));

    for orbital in &schema.orbitals {
        // Get entity name
        let entity_name = match &orbital.entity {
            orbital_core::EntityRef::Inline(e) => e.name.as_str(),
            orbital_core::EntityRef::Reference(r) => r.as_str(),
            // Phase 3.5.G.A.5: `Extends` is rewritten to `Inline` by the
            // inline phase before this CLI command sees the schema. If one
            // somehow leaks through, fall through with the upstream
            // `extends` reference, mirroring the bare `Reference` arm.
            orbital_core::EntityRef::Extends(e) => e.reference.as_str(),
        };

        // Get entity field names for checking
        let entity_fields: Vec<&str> = match &orbital.entity {
            orbital_core::EntityRef::Inline(e) => {
                e.fields.iter().filter_map(|f| f.name.as_deref()).collect()
            }
            _ => vec![],
        };

        // Process pages
        if let Some(pages) = &orbital.pages {
            for page_ref in pages {
                let page = match page_ref.as_inline() {
                    Some(p) => p,
                    None => continue,
                };

                println!("\nPage: {} ({})", page.name, page.path);

                // Collect trait definitions referenced by this page
                let page_trait_names: HashSet<String> = page
                    .traits
                    .as_ref()
                    .map(|t| {
                        t.iter()
                            .filter_map(|tr| match tr {
                                TraitReference::Inline(td) => Some(td.name.clone()),
                                TraitReference::Reference {
                                    reference, name, ..
                                } => reference.clone().or_else(|| name.clone()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Process each trait on this page
                for trait_ref in &orbital.traits {
                    let trait_def = match trait_ref {
                        TraitReference::Inline(td) => td,
                        _ => continue,
                    };

                    if !page_trait_names.contains(&trait_def.name) {
                        continue;
                    }

                    // Find initial state
                    let initial_state = trait_def
                        .state_machine
                        .states
                        .iter()
                        .find(|s| s.is_initial == Some(true))
                        .map(|s| s.name.as_str())
                        .unwrap_or("(unknown)");

                    // BFS through reachable states
                    let mut visited: HashSet<&str> = HashSet::new();
                    let mut queue: VecDeque<&str> = VecDeque::new();
                    queue.push_back(initial_state);
                    visited.insert(initial_state);

                    while let Some(state) = queue.pop_front() {
                        let is_initial = state == initial_state;
                        println!(
                            "  State: {}{}",
                            state,
                            if is_initial { " (initial)" } else { "" }
                        );

                        // Find transitions FROM this state
                        for transition in &trait_def.state_machine.transitions {
                            let from_matches = match &transition.from {
                                orbital_core::schema::StringOrArray::Single(s) => s == state,
                                orbital_core::schema::StringOrArray::Multiple(v) => {
                                    v.iter().any(|s| s == state)
                                }
                            };

                            if !from_matches {
                                continue;
                            }

                            let has_guard = transition.guard.is_some();
                            println!(
                                "    {} transition{}:",
                                transition.event,
                                if has_guard { " (guarded)" } else { "" }
                            );

                            // Collect effects
                            let mut has_fetch = false;
                            let mut fetch_entities: Vec<String> = Vec::new();
                            let mut render_effects: Vec<(String, String, Vec<String>)> = Vec::new(); // (slot, pattern_type, props)

                            if let Some(effects) = &transition.effects {
                                for effect in effects {
                                    if let SExpression::Array(arr) = effect {
                                        if arr.is_empty() {
                                            continue;
                                        }
                                        let op = match &arr[0] {
                                            SExpression::String(s) => s.as_str(),
                                            _ => continue,
                                        };

                                        match op {
                                            "fetch" => {
                                                has_fetch = true;
                                                if let Some(SExpression::String(e)) = arr.get(1) {
                                                    fetch_entities.push(e.clone());
                                                }
                                            }
                                            "render-ui" | "render_ui" => {
                                                let slot = match arr.get(1) {
                                                    Some(SExpression::String(s)) => s.clone(),
                                                    _ => "?".to_string(),
                                                };
                                                let (pattern_type, props) = match arr.get(2) {
                                                    Some(SExpression::Object(obj)) => {
                                                        let pt = match obj.get("type") {
                                                            Some(SExpression::String(t)) => {
                                                                t.clone()
                                                            }
                                                            _ => "?".to_string(),
                                                        };
                                                        let ps: Vec<String> = obj
                                                            .keys()
                                                            .filter(|k| *k != "type")
                                                            .cloned()
                                                            .collect();
                                                        (pt, ps)
                                                    }
                                                    Some(SExpression::Null) => {
                                                        ("null (clear)".to_string(), vec![])
                                                    }
                                                    _ => ("?".to_string(), vec![]),
                                                };
                                                render_effects.push((slot, pattern_type, props));
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            // Report fetches
                            if has_fetch {
                                for fe in &fetch_entities {
                                    println!("      📦 fetch \"{}\"", fe);
                                }
                            }

                            // Report render-ui
                            for (slot, pattern_type, props) in &render_effects {
                                let props_str = if props.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", props.join(", "))
                                };
                                let status = if pattern_type == "null (clear)" {
                                    "🗑️"
                                } else {
                                    "✅"
                                };
                                println!(
                                    "      {} Slot \"{}\" ← [{}]{}",
                                    status, slot, pattern_type, props_str
                                );

                                // Check for entity prop without fetch
                                if !has_fetch && props.contains(&"entity".to_string()) {
                                    println!(
                                        "         ⚠️  WARNING: entity prop present but no fetch in this transition"
                                    );
                                }
                            }

                            if render_effects.is_empty() && !has_fetch {
                                println!("      (no render-ui effects)");
                            }

                            // Enqueue target state
                            let to = transition.to.as_str();
                            if !visited.contains(to) {
                                visited.insert(to);
                                queue.push_back(to);
                            }
                        }
                    }

                    // Report unreachable states
                    let all_states: HashSet<&str> = trait_def
                        .state_machine
                        .states
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    let unreachable: Vec<&&str> = all_states.difference(&visited).collect();
                    if !unreachable.is_empty() {
                        println!("  ⚠️  Unreachable states: {:?}", unreachable);
                    }
                }
            }
        }
    }

    println!("\n{}", "=".repeat(60));
    println!("Simulation complete.\n");

    Ok(())
}

/// Parse schema and display information
fn parse_schema(path: &PathBuf) -> anyhow::Result<()> {
    println!("Parsing schema: {}", path.display());

    let content_raw = std::fs::read_to_string(path)?;
    let schema = parse_schema_auto(&content_raw)?;

    println!("\nSchema: {}", schema.name);
    if let Some(version) = &schema.version {
        println!("Version: {}", version);
    }
    if let Some(description) = &schema.description {
        println!("Description: {}", description);
    }

    println!("\nOrbitals ({}):", schema.orbitals.len());
    for orbital in &schema.orbitals {
        // Handle entity being either inline or reference
        let (entity_name, field_count) = match &orbital.entity {
            orbital_core::EntityRef::Inline(entity) => (entity.name.as_str(), entity.fields.len()),
            orbital_core::EntityRef::Reference(ref_str) => (ref_str.as_str(), 0),
            // Phase 3.5.G.A.5: `Extends` is rewritten to `Inline` before
            // this CLI command runs. Fall through with the upstream
            // `extends` reference and an unknown field count, mirroring
            // how the `Reference` arm reports a bare reference.
            orbital_core::EntityRef::Extends(extends) => (extends.reference.as_str(), 0),
        };
        println!("  - {} (entity: {})", orbital.name, entity_name);
        println!("    Fields: {}", field_count);
        println!("    Traits: {}", orbital.traits.len());
        if let Some(pages) = &orbital.pages {
            println!("    Pages: {}", pages.len());
        }
    }

    // Note: Traits are now only defined inline within orbitals, not at schema level.

    Ok(())
}

/// Resolve a schema: run Inline + Compose + Enrich, then serialize the
/// resulting `OrbitalSchema` to JSON. Emits to `output` (if given) or
/// stdout. Produces the same artifact `orbital compile` writes as
/// `resolved.orb` alongside generated code — useful for tooling that
/// wants the bundled schema view without running full codegen.
fn resolve_schema_cli(
    path: &std::path::Path,
    output: Option<&std::path::Path>,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::{parse_file, resolve_schema_with_path, CompilerOptions};

    let options = CompilerOptions {
        verbose,
        ..CompilerOptions::default()
    };

    let schema = parse_file(path)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    let resolved = resolve_schema_with_path(schema, &options, Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    let json = serde_json::to_string_pretty(&resolved)
        .map_err(|e| anyhow::anyhow!("Failed to serialize resolved schema: {}", e))?;

    match output {
        Some(out) => {
            std::fs::write(out, &json)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?;
            if verbose {
                eprintln!("Wrote resolved schema to {}", out.display());
            }
        }
        None => {
            println!("{}", json);
        }
    }
    Ok(())
}

fn format_schema(path: &PathBuf, write: bool) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Could not read {}: {}", path.display(), e))?;

    let formatted = match detect_syntax(&content) {
        InputSyntax::Lolo => {
            let program = orbital_lolo::parse_str(&content)
                .map_err(|e| anyhow::anyhow!("Parse error in {}: {:?}", path.display(), e))?;
            orbital_lolo::format_program(&program)
        }
        InputSyntax::OrbJson => {
            let value: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Invalid JSON in {}: {}", path.display(), e))?;
            serde_json::to_string_pretty(&value)?
        }
    };

    if write {
        std::fs::write(path, &formatted)?;
        println!("Formatted {} ({} bytes)", path.display(), formatted.len());
    } else {
        println!("{}", formatted);
    }

    Ok(())
}

/// Compile and serve a full-stack app from a .orb file using bundled Bun.
///
/// Steps:
/// 1. Compile .orb -> Hono TypeScript app (into .orbital/ directory)
/// 2. bun install
/// 3. Build shared package (tsup)
/// 4. Build client (vite via bun)
/// 5. Serve with Bun (Hono API + static client on single port)
fn run_serve(
    path: &PathBuf,
    port: u16,
    open: bool,
    no_build: bool,
    patterns_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let bun = find_bun()?;

    // Output directory: .orbital/ next to the schema file
    let schema_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let output = schema_dir.join(".orbital");

    // Step 1: Compile with Hono backend
    if !no_build || !output.exists() {
        println!("Compiling {}...", path.display());
        compile_schema(
            path,
            &output,
            ShellType::Typescript,
            ShellMode::Full,
            None, // frontend
            None, // backend
            None, // backend_url
            patterns_path,
            false, // verbose
            false, // dev_logs
            ServerFrameworkArg::Hono,
            None, // emit_orb
        )?;
    }

    // Step 2: Install dependencies
    println!("Installing dependencies...");
    let install_status = Command::new(&bun)
        .args(["install"])
        .current_dir(&output)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !install_status.success() {
        anyhow::bail!("bun install failed");
    }

    if !no_build {
        // Step 3: Build shared package
        let shared_dir = output.join("packages/shared");
        if shared_dir.join("tsup.config.ts").exists() {
            println!("Building shared package...");
            let shared_status = Command::new(&bun)
                .args(["run", "build"])
                .current_dir(&shared_dir)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            if !shared_status.success() {
                anyhow::bail!("shared build failed");
            }
        }

        // Step 4: Build client
        let client_dir = output.join("packages/client");
        println!("Building client...");
        let client_status = Command::new(&bun)
            .args(["run", "vite", "build"])
            .current_dir(&client_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        if !client_status.success() {
            anyhow::bail!("client build failed");
        }
    }

    // Step 5: Serve
    let server_entry = output.join("packages/server/src/serve.ts");
    if !server_entry.exists() {
        anyhow::bail!(
            "serve.ts not found at {}. The Hono shell template may be outdated.",
            server_entry.display()
        );
    }

    println!("\nStarting server on http://localhost:{}...\n", port);

    if open {
        // Open browser in background
        let url = format!("http://localhost:{}", port);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = Command::new("xdg-open")
                .arg(&url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }

    let serve_status = Command::new(&bun)
        .args(["run", "packages/server/src/serve.ts"])
        .current_dir(&output)
        .env("PORT", port.to_string())
        .env("NODE_ENV", "development")
        .env("USE_MOCK_DATA", "true")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !serve_status.success() {
        anyhow::bail!("Server exited with error");
    }

    Ok(())
}

/// Compile schema to generated code

fn compile_schema(
    path: &PathBuf,
    output: &PathBuf,
    shell: ShellType,
    mode: ShellMode,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
    backend_url: Option<String>,
    patterns_path: Option<&std::path::Path>,
    verbose: bool,
    dev_logs: bool,
    server_framework: ServerFrameworkArg,
    emit_orb: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use orbital_compiler::shell::{
        mobile_shell, python_shell, typescript_shell, typescript_shell_hono,
    };

    use orbital_compiler::{generate_from_file, GenerateOptions};
    use orbital_shell_mobile::MobileBackend;
    use orbital_shell_python::PythonBackend;
    use orbital_shell_typescript::TypeScriptBackend;
    use std::path::PathBuf;

    // Get the shells directory using runtime detection
    let shells_dir = find_shells_dir();

    // Handle mixed compilation (--frontend + --backend)
    if frontend.is_some() || backend.is_some() {
        return compile_mixed(path, output, frontend, backend, &shells_dir, verbose);
    }

    println!("Compiling schema: {}", path.display());
    println!("Output directory: {}", output.display());
    println!("Target shell: {:?}", shell);
    println!("Mode: {:?}", mode);

    // Syntax dispatch: detect LOLO vs Orb JSON syntax.
    // Explicit .lolo extension → always LOLO (no detection needed).
    // .orb files → content-based detection (first non-whitespace char).
    let source_content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let is_lolo_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("lolo"))
        .unwrap_or(false);

    let is_lolo = is_lolo_ext
        || matches!(detect_syntax(&source_content), InputSyntax::Lolo);

    let compile_path: PathBuf = if is_lolo {
        let schema = if is_lolo_ext {
            // .lolo extension: go straight to the parser, no detection
            orbital_lolo::parse_and_lower(path)
                .map_err(|e| anyhow::anyhow!("Failed to compile .lolo source: {}", e.render()))?
        } else {
            // .orb file with LOLO syntax: parse from the already-read content
            orbital_lolo::parse_and_lower_str(&source_content)
                .map_err(|e| anyhow::anyhow!("Failed to compile LOLO source: {}", e.render()))?
        };
        if verbose {
            println!("Detected LOLO syntax{}", if is_lolo_ext { " (.lolo file)" } else { " in .orb file" });
        }
        // Round-trip through serde_json::Value and strip nulls so the
        // emitted .orb matches the sparse hand-authored shape used in
        // existing fixtures (no `"foo": null` clutter).
        let value = serde_json::to_value(&schema)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let cleaned = strip_json_nulls(value);
        let json = serde_json::to_string_pretty(&cleaned)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let target = if let Some(emit) = emit_orb {
            emit.to_path_buf()
        } else {
            std::env::temp_dir().join("_orbital_lolo_lowered.orb")
        };
        std::fs::write(&target, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write lowered .orb: {}", e))?;
        if verbose {
            println!("Lowered to {}", target.display());
        }
        target
    } else {
        // Orb JSON path — translate i18n schemas to English before compilation
        let content_translated = translate_schema_content(&source_content);
        let translated_path = if content_translated != source_content {
            // Schema was translated — write to temp file for compilation
            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join("_orbital_translated_schema.orb");
            std::fs::write(&temp_path, &content_translated)
                .map_err(|e| anyhow::anyhow!("Failed to write translated schema: {}", e))?;
            temp_path
        } else {
            path.clone()
        };

        // If --emit-orb was passed for an .orb file, just copy it through.
        if let Some(emit) = emit_orb {
            std::fs::copy(&translated_path, emit)
                .map_err(|e| anyhow::anyhow!("Failed to copy schema to --emit-orb path: {}", e))?;
        }
        translated_path
    };

    // i18n: re-read content for the language detection branch below
    // (the lolo branch produces JSON which contains no Arabic/Slovenian).
    let content_raw = std::fs::read_to_string(&compile_path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let options = GenerateOptions {
        copy_template: true,
        clean: false,
        verbose,
        mode: match mode {
            ShellMode::Full => orbital_compiler::ShellMode::Full,
            ShellMode::Client => orbital_compiler::ShellMode::Client,
            ShellMode::Server => orbital_compiler::ShellMode::Server,
        },
        ..Default::default()
    };

    match shell {
        ShellType::Typescript => {
            let sf = match server_framework {
                ServerFrameworkArg::Express => orbital_shell_typescript::ServerFramework::Express,
                ServerFrameworkArg::Hono => orbital_shell_typescript::ServerFramework::Hono,
            };
            let ts_shell = match sf {
                orbital_shell_typescript::ServerFramework::Hono => {
                    typescript_shell_hono(&shells_dir)
                }
                orbital_shell_typescript::ServerFramework::Express => typescript_shell(&shells_dir),
            };
            // Load project-specific component mappings for code generation
            let extra_mappings = load_project_component_mappings(path, patterns_path);
            let detected_lang = detect_language(&content_raw).unwrap_or_else(|| "en".to_string());
            let backend = TypeScriptBackend {
                extra_mappings,
                schema_language: detected_lang,
                dev_logs,
                server_framework: sf,
                ..TypeScriptBackend::default()
            };
            let result = generate_from_file(&compile_path, output, &ts_shell, &backend, &options)?;

            // Copy project locales into output so the generated App.tsx import resolves
            copy_locales(path, output);

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            // Write .env file with backend URL if provided
            if let Some(url) = &backend_url {
                let env_path = output.join(".env");
                let env_content =
                    format!("# Backend API URL (auto-generated)\nVITE_API_URL={}\n", url);
                // Append to existing .env or create new
                if env_path.exists() {
                    let existing = std::fs::read_to_string(&env_path)?;
                    if !existing.contains("VITE_API_URL=") {
                        std::fs::write(&env_path, format!("{}\n{}", existing.trim(), env_content))?;
                    } else {
                        // Update existing VITE_API_URL
                        let updated: String = existing
                            .lines()
                            .map(|line| {
                                if line.starts_with("VITE_API_URL=") {
                                    format!("VITE_API_URL={}", url)
                                } else {
                                    line.to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        std::fs::write(&env_path, updated)?;
                    }
                } else {
                    std::fs::write(&env_path, env_content)?;
                }
                println!("🔧 Configured backend URL: {}", url);
            }
        }
        ShellType::Python => {
            let py_shell = python_shell(&shells_dir);
            let backend = PythonBackend::new();
            let result = generate_from_file(&compile_path, output, &py_shell, &backend, &options)?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  python -m venv .venv && source .venv/bin/activate");
            println!("  pip install -e .");
            println!("  python -m orbital_app.main");
        }
        ShellType::Mobile => {
            let mobile_shell = mobile_shell(&shells_dir);
            let mobile_backend = MobileBackend;
            let result = generate_from_file(
                &compile_path,
                output,
                &mobile_shell,
                &mobile_backend,
                &options,
            )?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  npm install");
            println!("  npx expo start");
        }
    }

    Ok(())
}

/// Compile with mixed frontend/backend shells
fn compile_mixed(
    path: &PathBuf,
    output: &PathBuf,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
    shells_dir: &PathBuf,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::shell::{mobile_shell, python_shell, typescript_shell};

    use orbital_compiler::{generate_from_file, GenerateOptions};
    use orbital_shell_mobile::MobileBackend;
    use orbital_shell_python::PythonBackend;
    use orbital_shell_typescript::TypeScriptBackend;

    println!("Compiling schema: {}", path.display());
    println!("Output directory: {}", output.display());
    println!("Mixed mode compilation:");

    // Auto-detect LOLO vs Orb JSON syntax, handle i18n translation
    let content_raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let compile_path = if matches!(detect_syntax(&content_raw), InputSyntax::Lolo) {
        // LOLO syntax in .orb file — lower to JSON temp file
        let schema = orbital_lolo::parse_and_lower_str(&content_raw)
            .map_err(|e| anyhow::anyhow!("Failed to compile LOLO source: {}", e.render()))?;
        let value = serde_json::to_value(&schema)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let cleaned = strip_json_nulls(value);
        let json = serde_json::to_string_pretty(&cleaned)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("_orbital_lolo_lowered_mixed.orb");
        std::fs::write(&temp_path, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write lowered .orb: {}", e))?;
        temp_path
    } else {
        // Orb JSON — apply i18n translation
        let content_translated = translate_schema_content(&content_raw);
        if content_translated != content_raw {
            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join("_orbital_translated_schema_mixed.orb");
            std::fs::write(&temp_path, &content_translated)
                .map_err(|e| anyhow::anyhow!("Failed to write translated schema: {}", e))?;
            temp_path
        } else {
            path.clone()
        }
    };

    let mut total_files = 0;

    // Compile frontend
    if let Some(frontend_shell) = frontend {
        let frontend_output = output.join("frontend");
        println!(
            "  Frontend: {:?} -> {}",
            frontend_shell,
            frontend_output.display()
        );

        let options = GenerateOptions {
            copy_template: true,
            clean: false,
            verbose,
            mode: orbital_compiler::ShellMode::Client,
            ..Default::default()
        };

        let detected_lang_mixed = detect_language(&content_raw).unwrap_or_else(|| "en".to_string());
        match frontend_shell {
            ShellType::Typescript => {
                let ts_shell = typescript_shell(shells_dir);
                let backend = TypeScriptBackend {
                    schema_language: detected_lang_mixed.clone(),
                    ..TypeScriptBackend::new()
                };
                let result = generate_from_file(
                    &compile_path,
                    &frontend_output,
                    &ts_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Python => {
                let py_shell = python_shell(shells_dir);
                let backend = PythonBackend::new();
                let result = generate_from_file(
                    &compile_path,
                    &frontend_output,
                    &py_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Mobile => {
                let mob_shell = mobile_shell(shells_dir);
                let mob_backend = MobileBackend;
                let result = generate_from_file(
                    &compile_path,
                    &frontend_output,
                    &mob_shell,
                    &mob_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
        }
    }

    // Compile backend
    if let Some(backend_shell) = backend {
        let backend_output = output.join("backend");
        println!(
            "  Backend: {:?} -> {}",
            backend_shell,
            backend_output.display()
        );

        let options = GenerateOptions {
            copy_template: true,
            clean: false,
            verbose,
            mode: orbital_compiler::ShellMode::Server,
            ..Default::default()
        };

        match backend_shell {
            ShellType::Typescript => {
                let ts_shell = typescript_shell(shells_dir);
                let backend = TypeScriptBackend::new();
                let result = generate_from_file(
                    &compile_path,
                    &backend_output,
                    &ts_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Python => {
                let py_shell = python_shell(shells_dir);
                let backend = PythonBackend::new();
                let result = generate_from_file(
                    &compile_path,
                    &backend_output,
                    &py_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Mobile => {
                return Err(anyhow::anyhow!("Mobile shell cannot be used as a backend"));
            }
        }
    }

    println!("\n✅ Generated {} total files", total_files);

    // Generate docker-compose for mixed deployment
    if frontend.is_some() && backend.is_some() {
        let docker_compose = generate_docker_compose(frontend, backend);
        let compose_path = output.join("docker-compose.yaml");
        std::fs::write(&compose_path, docker_compose)?;
        println!("📦 Generated docker-compose.yaml");

        // Generate .env file for frontend with API URL
        let backend_url = match backend {
            Some(ShellType::Python) => "http://localhost:8000",
            Some(ShellType::Typescript) => "http://localhost:3030",
            Some(ShellType::Mobile) => "http://localhost:8000",
            None => "http://localhost:8000",
        };

        let env_content = match frontend {
            Some(ShellType::Typescript) => format!(
                "# API URL for backend server\nVITE_API_URL={}\n",
                backend_url
            ),
            Some(ShellType::Python) => {
                format!("# API URL for backend server\nAPI_URL={}\n", backend_url)
            }
            Some(ShellType::Mobile) => {
                format!(
                    "# API URL for backend server\nEXPO_PUBLIC_API_URL={}\n",
                    backend_url
                )
            }
            None => String::new(),
        };

        if !env_content.is_empty() {
            let env_path = output.join("frontend").join(".env");
            std::fs::write(&env_path, &env_content)?;
            println!("🔧 Generated frontend/.env with API_URL");
        }

        println!("\nNext steps:");
        println!("  cd {}", output.display());
        println!("  docker-compose up");
        println!("\nOr run separately:");
        println!("  # Terminal 1: Backend");
        println!("  cd backend && python -m venv .venv && source .venv/bin/activate && pip install -e . && python -m orbital_app.main");
        println!("  # Terminal 2: Frontend");
        println!("  cd frontend && npm install && npm run dev");
    }

    Ok(())
}

/// Generate docker-compose.yaml for mixed deployment
fn generate_docker_compose(frontend: Option<ShellType>, backend: Option<ShellType>) -> String {
    let mut compose = String::from("version: '3.8'\n\nservices:\n");

    if let Some(backend_shell) = backend {
        match backend_shell {
            ShellType::Python => {
                compose.push_str("  backend:\n");
                compose.push_str("    build: ./backend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"8000:8000\"\n");
                compose.push_str("    environment:\n");
                compose.push_str("      - CORS_ORIGINS=http://localhost:3000\n");
                compose.push_str("\n");
            }
            ShellType::Typescript => {
                compose.push_str("  backend:\n");
                compose.push_str("    build: ./backend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3030:3030\"\n");
                compose.push_str("    environment:\n");
                compose.push_str("      - CORS_ORIGINS=http://localhost:3000\n");
                compose.push_str("\n");
            }
            ShellType::Mobile => {
                // Mobile apps don't have a backend component
            }
        }
    }

    if let Some(frontend_shell) = frontend {
        // API URL from browser perspective (localhost, not internal docker network)
        let backend_url = match backend {
            Some(ShellType::Python) => "http://localhost:8000",
            Some(ShellType::Typescript) => "http://localhost:3030",
            Some(ShellType::Mobile) => "http://localhost:8000",
            None => "http://localhost:8000",
        };

        match frontend_shell {
            ShellType::Typescript => {
                compose.push_str("  frontend:\n");
                compose.push_str("    build:\n");
                compose.push_str("      context: ./frontend\n");
                compose.push_str("      args:\n");
                compose.push_str(&format!("        VITE_API_URL: {}\n", backend_url));
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3000:3000\"\n");
                compose.push_str("    depends_on:\n");
                compose.push_str("      - backend\n");
            }
            ShellType::Python => {
                compose.push_str("  frontend:\n");
                compose.push_str("    build: ./frontend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"8080:8080\"\n");
                compose.push_str("    environment:\n");
                compose.push_str(&format!("      - API_URL={}\n", backend_url));
                compose.push_str("    depends_on:\n");
                compose.push_str("      - backend\n");
            }
            ShellType::Mobile => {
                // Mobile apps don't run in Docker - they run on device/simulator
                // Just document the backend dependency
                compose.push_str("  # Mobile app - run locally with:\n");
                compose.push_str("  #   cd frontend && npm install && npx expo start\n");
                compose.push_str("  # Backend URL for mobile app:\n");
                compose.push_str(&format!("  #   EXPO_PUBLIC_API_URL={}\n", backend_url));
            }
        }
    }

    compose
}

/// Detect whether a graphical display is available
fn has_display() -> bool {
    // X11
    if std::env::var("DISPLAY").is_ok() {
        return true;
    }
    // Wayland
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("WAYLAND_SOCKET").is_ok() {
        return true;
    }
    // macOS / Windows always have a display when running interactively
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return true;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    false
}

/// Run development mode: HTTP server + optional GUI
// ============================================================================
// Convert: spawn npx almadar-converter and stream output
// ============================================================================

fn run_convert(
    path: Option<PathBuf>,
    output: &str,
    crawl: Option<String>,
    crawl_facts: Option<PathBuf>,
    ai: bool,
    verify: bool,
    facts_only: bool,
    headed: bool,
    depth: u32,
    verbose: bool,
) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut args = vec!["almadar-converter".to_string()];

    // Source path (positional arg)
    if let Some(p) = &path {
        args.push(p.display().to_string());
    }

    // Output
    args.push("-o".to_string());
    args.push(output.to_string());

    // Flags
    if let Some(url) = &crawl {
        args.push("--crawl".to_string());
        args.push(url.clone());
    }
    if let Some(cf) = &crawl_facts {
        args.push("--crawl-facts".to_string());
        args.push(cf.display().to_string());
    }
    if ai {
        args.push("--ai".to_string());
    }
    if verify {
        args.push("--verify".to_string());
    }
    if facts_only {
        args.push("--facts-only".to_string());
    }
    if headed {
        args.push("--headed".to_string());
    }
    args.push("--depth".to_string());
    args.push(depth.to_string());
    args.push("--json".to_string());
    if verbose {
        args.push("--verbose".to_string());
    }

    if verbose {
        eprintln!("Running: npx {}", args.join(" "));
    }

    // Spawn npx almadar-converter --json and stream output
    let mut child = Command::new("npx")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn npx: {}. Is Node.js installed?", e))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
        let line = line?;
        // Parse JSON progress events and render them as CLI output
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(gate) = event.get("gate").and_then(|g| g.as_str()) {
                let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let message = event.get("message").and_then(|m| m.as_str()).unwrap_or("");
                match status {
                    "start" => eprint!("[{}] {}...", gate, message),
                    "complete" => eprintln!(" {}", message),
                    "progress" => eprintln!("\n[{}] {}", gate, message),
                    "error" => eprintln!(" ERROR: {}", message),
                    _ => {}
                }
            } else if let Some(success) = event.get("success").and_then(|s| s.as_bool()) {
                // Final result line
                if success {
                    if let Some(out) = event.get("output").and_then(|o| o.as_str()) {
                        eprintln!("\nOutput: {}", out);
                    }
                } else if let Some(err) = event.get("error").and_then(|e| e.as_str()) {
                    anyhow::bail!("Conversion failed: {}", err);
                }
            }
        } else if verbose {
            eprintln!("{}", line);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!(
            "almadar-converter exited with code {}",
            status.code().unwrap_or(-1)
        );
    }

    Ok(())
}

///
/// In headless environments (no DISPLAY/WAYLAND_DISPLAY), the GUI is skipped
/// and only the API server is started. Access the schema at the printed URL.
fn run_dev_mode(path: &PathBuf, port: u16, force_headless: bool) -> anyhow::Result<()> {
    use std::thread;

    let display_available = !force_headless && has_display();

    println!("Starting Almadar dev server on http://127.0.0.1:{}", port);
    if !display_available {
        println!("No display detected — running in headless API-only mode.");
        println!("Access the schema API at http://127.0.0.1:{}/api", port);
        println!("(Set DISPLAY or WAYLAND_DISPLAY to enable the desktop GUI)");
    }

    let path_clone = path.clone();

    // Start server in background thread (always uses mock data in dev mode)
    let server_handle =
        thread::spawn(move || orbital_server::run(&path_clone, "127.0.0.1", port, true));

    if display_available {
        // Give server a moment to start before opening GUI
        thread::sleep(std::time::Duration::from_millis(500));

        // Start GUI client pointing to local server
        let server_url = format!("http://127.0.0.1:{}", port);
        orbital_client::run_desktop(path, Some(&server_url))?;
    } else {
        // Headless: block on the server thread so the process stays alive
        server_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Server thread panicked"))??;
    }

    Ok(())
}

// ============================================================================
// New Command - Project Scaffolding
// ============================================================================

/// Template metadata
struct Template {
    name: &'static str,
    display_name: &'static str,
    description: &'static str,
    schema: &'static str,
    readme: &'static str,
}

/// Embedded templates
const TEMPLATES: &[Template] = &[
    Template {
        name: "blank",
        display_name: "Blank Project",
        description: "Minimal schema with a single orbital - start from scratch",
        schema: include_str!("../../../templates/blank/schema.orb"),
        readme: include_str!("../../../templates/blank/README.md"),
    },
    Template {
        name: "crud",
        display_name: "CRUD Application",
        description: "Full CRUD with list view, forms, and detail panels",
        schema: include_str!("../../../templates/crud/schema.orb"),
        readme: include_str!("../../../templates/crud/README.md"),
    },
    Template {
        name: "wizard",
        display_name: "Multi-Step Wizard",
        description: "Multi-step form flow with progress indicator",
        schema: include_str!("../../../templates/wizard/schema.orb"),
        readme: include_str!("../../../templates/wizard/README.md"),
    },
    Template {
        name: "dashboard",
        display_name: "Dashboard",
        description: "Multi-entity dashboard with tabs, stats, and charts",
        schema: include_str!("../../../templates/dashboard/schema.orb"),
        readme: include_str!("../../../templates/dashboard/README.md"),
    },
    Template {
        name: "saas",
        display_name: "SaaS Starter",
        description: "User onboarding, subscription plans, and billing",
        schema: include_str!("../../../templates/saas/schema.orb"),
        readme: include_str!("../../../templates/saas/README.md"),
    },
    Template {
        name: "game",
        display_name: "Game",
        description: "Game loop with canvas, player entity, and tick updates",
        schema: include_str!("../../../templates/game/schema.orb"),
        readme: include_str!("../../../templates/game/README.md"),
    },
    Template {
        name: "ai",
        display_name: "AI/ML Application",
        description: "Neural network training, model management, inference",
        schema: include_str!("../../../templates/ai/schema.orb"),
        readme: include_str!("../../../templates/ai/README.md"),
    },
    Template {
        name: "embedded",
        display_name: "Embedded/IoT",
        description: "Sensors, actuators, device lifecycle, power management",
        schema: include_str!("../../../templates/embedded/schema.orb"),
        readme: include_str!("../../../templates/embedded/README.md"),
    },
];

/// List available templates
fn list_templates() -> anyhow::Result<()> {
    println!("Available templates:\n");
    println!("{:<12} {:<25} {}", "NAME", "DISPLAY NAME", "DESCRIPTION");
    println!("{}", "=".chars().cycle().take(80).collect::<String>());

    for template in TEMPLATES {
        println!(
            "{:<12} {:<25} {}",
            template.name, template.display_name, template.description
        );
    }

    println!("\nUsage: orb new <project-name> --template <template-name>");
    println!("\nExample:");
    println!("  orb new my-app --template crud");

    Ok(())
}

/// Create a new project from a template
fn create_new_project(
    name: &str,
    template_name: &str,
    shell: ShellType,
    no_git: bool,
    no_install: bool,
) -> anyhow::Result<()> {
    use std::fs;

    // Find template
    let template = TEMPLATES
        .iter()
        .find(|t| t.name == template_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Template '{}' not found. Use --list to see available templates.",
                template_name
            )
        })?;

    println!("Creating new project: {}", name);
    println!("Template: {} ({})", template.display_name, template.name);
    println!();

    // Create project directory
    let project_dir = PathBuf::from(name);
    if project_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", name);
    }
    fs::create_dir_all(&project_dir)?;

    // Process template variables
    let schema_content = template
        .schema
        .replace("{{projectName}}", name)
        .replace("{{entityName}}", "Item")
        .replace("{{wizardName}}", "Onboarding")
        .replace("{{entityName | lowercase}}", "item")
        .replace("{{wizardName | lowercase}}", "onboarding");

    let readme_content = template
        .readme
        .replace("{{projectName}}", name)
        .replace("{{entityName}}", "Item")
        .replace("{{wizardName}}", "Onboarding")
        .replace("{{entityName | lowercase}}", "item")
        .replace("{{wizardName | lowercase}}", "onboarding");

    // Write schema.orb
    let schema_path = project_dir.join("schema.orb");
    fs::write(&schema_path, &schema_content)?;
    println!("  Created schema.orb");

    // Write README.md
    let readme_path = project_dir.join("README.md");
    fs::write(&readme_path, &readme_content)?;
    println!("  Created README.md");

    // Compile the schema
    println!();
    println!("Compiling schema...");

    let app_dir = project_dir.join("app");
    match compile_schema(
        &schema_path,
        &app_dir,
        shell,
        ShellMode::Full,
        None,
        None,
        None,
        None,
        false,
        false,
        ServerFrameworkArg::Express,
        None,
    ) {
        Ok(()) => {}
        Err(e) => {
            // Shell template not found is a known distribution issue — give clear guidance
            let msg = e.to_string();
            if msg.contains("Shell not found") || msg.contains("not found") {
                eprintln!();
                eprintln!("Warning: Could not compile schema — shell template not found.");
                eprintln!("  Your schema.orb has been created successfully.");
                eprintln!();
                eprintln!("  To compile manually, install the shell template first:");
                eprintln!("    npm install -g @almadar/shell");
                eprintln!("  Then run:");
                eprintln!("    ORBITAL_SHELLS_DIR=$(npm root -g)/@almadar \\");
                eprintln!(
                    "      orb compile {}/schema.orb --output {}/app",
                    name, name
                );
                eprintln!();
                eprintln!("  Or set ORBITAL_SHELLS_DIR permanently in your shell profile.");
            } else {
                return Err(e);
            }
        }
    }

    // Initialize git
    if !no_git {
        println!();
        println!("Initializing git repository...");
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&project_dir)
            .status();

        if status.is_ok() {
            // Create .gitignore
            let gitignore = "\
# Turbo\n\
.turbo/\n\
\n\
# Node\n\
node_modules/\n\
*.log\n\
\n\
# Build output\n\
dist/\n\
\n\
# Environment\n\
.env\n\
.env.local\n\
.env.*.local\n\
\n\
# Editor\n\
.vscode/\n\
.idea/\n\
*.swp\n\
*.swo\n\
\n\
# OS\n\
.DS_Store\n\
Thumbs.db\n\
\n\
# Test coverage\n\
coverage/\n\
.nyc_output/\n\
\n\
# Lock files (regenerated on install)\n\
package-lock.json\n";
            fs::write(project_dir.join(".gitignore"), gitignore)?;
            println!("  Initialized git repository");
        } else {
            println!("  Warning: Could not initialize git repository");
        }
    }

    // Install dependencies
    if !no_install {
        println!();
        match shell {
            ShellType::Typescript => {
                println!("Installing dependencies (npm install)...");
                let status = Command::new("npm")
                    .args(["install"])
                    .current_dir(&app_dir)
                    .status();

                if status.is_ok() {
                    println!("  Dependencies installed");
                } else {
                    println!(
                        "  Warning: Could not install dependencies. Run 'npm install' manually."
                    );
                }
            }
            ShellType::Python => {
                println!("To install dependencies, run:");
                println!("  cd {}/app", name);
                println!("  python -m venv .venv");
                println!("  source .venv/bin/activate");
                println!("  pip install -e .");
            }
            ShellType::Mobile => {
                println!("Installing dependencies (npm install)...");
                let status = Command::new("npm")
                    .args(["install"])
                    .current_dir(&app_dir)
                    .status();

                if status.is_ok() {
                    println!("  Dependencies installed");
                } else {
                    println!(
                        "  Warning: Could not install dependencies. Run 'npm install' manually."
                    );
                }
            }
        }
    }

    // Print next steps
    println!();
    println!("✅ Project created successfully!");
    println!();
    println!("Next steps:");
    println!("  cd {}", name);
    match shell {
        ShellType::Typescript => {
            if no_install {
                println!("  cd app && npm install");
            }
            println!("  cd app && npm run dev");
        }
        ShellType::Python => {
            println!("  cd app");
            println!("  python -m venv .venv && source .venv/bin/activate");
            println!("  pip install -e .");
            println!("  python -m orbital_app.main");
        }
        ShellType::Mobile => {
            if no_install {
                println!("  cd app && npm install");
            }
            println!("  cd app && npx expo start");
        }
    }
    println!();
    println!("To modify your schema, edit schema.orb and run:");
    println!("  orb compile schema.orb --output app");

    Ok(())
}

// ============================================================================
// Test Command - Exhaustive State Machine Testing
// ============================================================================

/// Run exhaustive state machine tests
///
/// Uses the ExhaustiveTestGenerator from orbital-compiler to generate test cases.
/// Currently generates and displays test cases; future versions will execute against runtime.
fn run_tests(
    path: &PathBuf,
    orbital_filter: Option<String>,
    trait_filter: Option<String>,
    report: &str,
    execute: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::{compile_file, CompilerOptions, ExhaustiveTestGenerator};

    println!("Loading schema: {}", path.display());

    // Compile schema to OIR
    let options = CompilerOptions::default();
    let module =
        compile_file(path, &options).map_err(|e| anyhow::anyhow!("Compilation failed: {}", e))?;

    if verbose {
        println!("Compiled module: {}", module.name);
        println!("Traits found: {}", module.traits.len());
    }

    // Generate exhaustive tests
    let generator = ExhaustiveTestGenerator::new(&module);
    let mut results = generator.generate_all_tests();

    // Apply filters
    if let Some(ref orbital) = orbital_filter {
        results.retain(|r| {
            r.trait_name
                .to_lowercase()
                .contains(&orbital.to_lowercase())
        });
    }
    if let Some(ref trait_name) = trait_filter {
        results.retain(|r| {
            r.trait_name
                .to_lowercase()
                .contains(&trait_name.to_lowercase())
        });
    }

    if results.is_empty() {
        println!("No traits found matching the filter criteria.");
        return Ok(());
    }

    // If --execute, run headless execution against the state machine
    if execute {
        return execute_tests_headless(&module, &mut results, report, verbose);
    }

    // Output results based on report format
    match report {
        "json" => {
            let json = serde_json::to_string_pretty(&results)?;
            println!("{}", json);
        }
        _ => {
            // Text format
            let mut total_tests = 0;
            for result in &results {
                println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!("Trait: {}", result.trait_name);
                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!("Generated {} test cases:", result.tests.len());
                total_tests += result.tests.len();

                // Group tests by category
                let mut by_category: std::collections::HashMap<_, Vec<_>> =
                    std::collections::HashMap::new();
                for test in &result.tests {
                    by_category.entry(test.category).or_default().push(test);
                }

                for (category, tests) in &by_category {
                    println!("\n  {} ({} tests):", category, tests.len());
                    for test in tests {
                        if verbose {
                            println!("    • {} ", test.name);
                            println!(
                                "      from: {} → event: {} → to: {}",
                                test.from_state, test.event, test.expected_state
                            );
                            if !test.setup_path.is_empty() {
                                let path_str: Vec<_> = test
                                    .setup_path
                                    .iter()
                                    .map(|s| format!("{}→{}", s.event, s.expected_state))
                                    .collect();
                                println!("      setup: {}", path_str.join(" "));
                            }
                        } else {
                            println!("    • {}", test.name);
                        }
                    }
                }
            }

            println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!(
                "Total: {} traits, {} test cases generated",
                results.len(),
                total_tests
            );
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("\nHint: Use --execute to run tests headlessly against the state machine.");
            println!("Use --report json to get test cases for external test runners.");
        }
    }

    Ok(())
}

// ============================================================================
// Headless State Machine Executor (RCG-09)
// ============================================================================

/// Execute generated test cases headlessly against the OIR state machine.
///
/// For each test case:
/// 1. Start at the initial state
/// 2. Walk the setup_path (fire events to reach the from_state)
/// 3. Fire the test event
/// 4. Compare the resulting state to expected_state
fn execute_tests_headless(
    module: &orbital_core::ir::OirModule,
    results: &mut Vec<orbital_compiler::TraitTestResult>,
    report: &str,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::TestCategory;
    use orbital_core::ir::{OirBinaryOp, OirBindingRoot, OirExpr, OirTrait, OirTransitionFrom};

    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    let mut total_skipped = 0usize;

    for result in results.iter_mut() {
        // Find the OIR trait
        let trait_def = match module.traits.iter().find(|t| t.name == result.trait_name) {
            Some(t) => t,
            None => {
                if verbose {
                    println!(
                        "⚠ Trait '{}' not found in module, skipping",
                        result.trait_name
                    );
                }
                continue;
            }
        };

        let initial_state = trait_def
            .states
            .iter()
            .find(|s| s.is_initial)
            .map(|s| s.name.as_str())
            .or_else(|| trait_def.states.first().map(|s| s.name.as_str()))
            .unwrap_or("idle");

        let mut trait_passed = 0usize;
        let mut trait_failed = 0usize;

        for test in &result.tests {
            // Skip journey tests — they encode compound events in a single string
            if test.category == TestCategory::Journey {
                total_skipped += 1;
                continue;
            }

            // Step 1: Walk setup_path to reach from_state
            let mut current_state = initial_state.to_string();
            let mut setup_ok = true;

            for step in &test.setup_path {
                match execute_event_headless(
                    trait_def,
                    &current_state,
                    &step.event,
                    step.payload.as_ref(),
                    test.entity_data.as_ref(),
                ) {
                    Some(new_state) => {
                        current_state = new_state;
                    }
                    None => {
                        // Setup path might go through guarded transitions — skip test
                        setup_ok = false;
                        break;
                    }
                }
            }

            if !setup_ok {
                total_skipped += 1;
                if verbose {
                    println!("  ⊘ {} (setup path blocked)", test.name);
                }
                continue;
            }

            // Step 2: Verify we're at the expected from_state
            if current_state != test.from_state {
                total_skipped += 1;
                if verbose {
                    println!(
                        "  ⊘ {} (setup reached '{}', expected '{}')",
                        test.name, current_state, test.from_state
                    );
                }
                continue;
            }

            // Step 3: Fire the test event
            let after_state = execute_event_headless(
                trait_def,
                &current_state,
                &test.event,
                test.payload.as_ref(),
                test.entity_data.as_ref(),
            )
            .unwrap_or_else(|| current_state.clone());

            // Step 4: Compare to expected_state
            let passed = after_state == test.expected_state;

            if passed {
                trait_passed += 1;
                total_passed += 1;
                if verbose {
                    println!("  ✓ {}", test.name);
                }
            } else {
                trait_failed += 1;
                total_failed += 1;
                println!(
                    "  ✗ {} — expected '{}', got '{}'",
                    test.name, test.expected_state, after_state
                );
            }
        }

        result.passed = trait_passed;
        result.failed = trait_failed;
    }

    // Output summary
    match report {
        "json" => {
            let json = serde_json::to_string_pretty(&results)?;
            println!("{}", json);
        }
        _ => {
            println!("\n{}", "=".repeat(60));
            println!("Headless Execution Results");
            println!("{}", "=".repeat(60));

            for result in results.iter() {
                let status = if result.failed == 0 { "✓" } else { "✗" };
                println!(
                    "  {} {} — {}/{} passed{}",
                    status,
                    result.trait_name,
                    result.passed,
                    result.passed + result.failed,
                    if result.failed > 0 {
                        format!(" ({} failed)", result.failed)
                    } else {
                        String::new()
                    }
                );
            }

            println!("{}", "-".repeat(60));
            println!(
                "Total: {} passed, {} failed, {} skipped",
                total_passed, total_failed, total_skipped
            );
            println!("{}", "=".repeat(60));

            if total_failed > 0 {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Execute a single event against a trait's state machine headlessly.
///
/// Finds matching transition, evaluates guard (if any), returns new state.
/// Returns None if no transition matches or guard blocks.
fn execute_event_headless(
    trait_def: &orbital_core::ir::OirTrait,
    current_state: &str,
    event: &str,
    payload: Option<&serde_json::Value>,
    entity_data: Option<&serde_json::Value>,
) -> Option<String> {
    use orbital_core::ir::{OirBinaryOp, OirBindingRoot, OirExpr, OirTransitionFrom};

    // Find matching transition
    let transition = trait_def
        .transitions
        .iter()
        .find(|t| t.event == event && t.from.matches(current_state))?;

    // Evaluate guard if present
    if let Some(ref guard) = transition.guard {
        let empty = serde_json::Value::Object(serde_json::Map::new());
        let p = payload.unwrap_or(&empty);
        let e = entity_data.unwrap_or(&empty);
        if !evaluate_guard_headless(guard, p, e) {
            return None;
        }
    }

    Some(transition.to.clone())
}

/// Headless guard evaluator for OIR expressions.
///
/// Handles common guard patterns: comparisons, boolean logic,
/// payload/entity field access. For complex guards (function calls, etc.)
/// defaults to allowing the transition (permissive mode).
fn evaluate_guard_headless(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> bool {
    use orbital_core::ir::{OirBinaryOp, OirBindingRoot, OirExpr};

    match expr {
        OirExpr::Bool { value } => *value,
        OirExpr::BinaryOp { op, left, right } => {
            match op {
                OirBinaryOp::And => {
                    evaluate_guard_headless(left, payload, entity_data)
                        && evaluate_guard_headless(right, payload, entity_data)
                }
                OirBinaryOp::Or => {
                    evaluate_guard_headless(left, payload, entity_data)
                        || evaluate_guard_headless(right, payload, entity_data)
                }
                OirBinaryOp::Eq => {
                    let l = resolve_expr_value(left, payload, entity_data);
                    let r = resolve_expr_value(right, payload, entity_data);
                    l == r
                }
                OirBinaryOp::Ne => {
                    let l = resolve_expr_value(left, payload, entity_data);
                    let r = resolve_expr_value(right, payload, entity_data);
                    l != r
                }
                OirBinaryOp::Gt => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a > b,
                        _ => false, // strict: unresolvable operands block the transition
                    }
                }
                OirBinaryOp::Ge => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a >= b,
                        _ => false,
                    }
                }
                OirBinaryOp::Lt => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a < b,
                        _ => false,
                    }
                }
                OirBinaryOp::Le => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a <= b,
                        _ => false,
                    }
                }
                _ => true, // Permissive for unsupported ops
            }
        }
        OirExpr::UnaryOp {
            op: orbital_core::ir::OirUnaryOp::Not,
            operand,
        } => !evaluate_guard_headless(operand, payload, entity_data),
        // For anything else (function calls, complex expressions), be permissive
        _ => true,
    }
}

/// Resolve an OIR expression to a JSON value for comparison.
fn resolve_expr_value(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> serde_json::Value {
    use orbital_core::ir::{OirBindingRoot, OirExpr};

    match expr {
        OirExpr::Null => serde_json::Value::Null,
        OirExpr::Bool { value } => serde_json::Value::Bool(*value),
        OirExpr::Number { value } => serde_json::json!(*value),
        OirExpr::String { value } => serde_json::Value::String(value.clone()),
        OirExpr::Binding { binding } => {
            let source = match &binding.root {
                OirBindingRoot::Payload => payload,
                OirBindingRoot::Entity => entity_data,
                // State/config/user not available in headless mode
                _ => return serde_json::Value::Null,
            };
            // Walk the path into the source object
            let mut current = source;
            for key in &binding.path {
                current = match current.get(key) {
                    Some(v) => v,
                    None => return serde_json::Value::Null,
                };
            }
            current.clone()
        }
        _ => serde_json::Value::Null,
    }
}

/// Resolve an OIR expression to an f64 for numeric comparison.
fn resolve_expr_to_f64(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> Option<f64> {
    let val = resolve_expr_value(expr, payload, entity_data);
    match val {
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Find shells/templates directory using runtime detection
///
/// Priority:
/// 1. ORBITAL_SHELLS_DIR environment variable
/// 2. Relative to executable location (covers npm global installs and local builds)
///    - npm global: <prefix>/bin/orb → <prefix>/lib/node_modules/@almadar/shell
///    - pnpm/yarn global: similar patterns
///    - local monorepo build: relative paths to packages/ or templates/
/// 3. Compile-time path (for development in the monorepo)
///
/// Note: Supports "almadar-shell" (current) and "typescript-shell" (legacy)
fn find_shells_dir() -> PathBuf {
    // 1. Check ORBITAL_SHELLS_DIR environment variable (trust it if set)
    if let Ok(shells_dir) = std::env::var("ORBITAL_SHELLS_DIR") {
        return PathBuf::from(shells_dir);
    }

    // 2. Check monorepo first: if cwd has packages/almadar-shell, use it.
    //    This ensures local development always uses the monorepo shell
    //    (which matches the locally-built @almadar/ui), not a stale npm-installed one.
    if let Ok(cwd) = std::env::current_dir() {
        let monorepo_packages = cwd.join("packages");
        if monorepo_packages.join("almadar-shell").exists() {
            return monorepo_packages;
        }
        // Walk up from cwd to find monorepo root (handles running from subdirectories)
        let mut dir = cwd.as_path();
        for _ in 0..5 {
            if let Some(parent) = dir.parent() {
                let packages_path = parent.join("packages");
                if packages_path.join("almadar-shell").exists() {
                    return packages_path;
                }
                dir = parent;
            }
        }
    }

    // 3. Check compile-time path (for development when cwd doesn't resolve)
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo_root) = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
    {
        let packages_path = repo_root.join("packages");
        if packages_path.exists() {
            return packages_path;
        }
    }

    // 4. Check relative to executable location (npm global installs)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // npm global install layout:
            //   {prefix}/bin/orb
            //   {prefix}/lib/node_modules/@almadar/shell
            for nm_rel in [
                "../lib/node_modules",
                "../node_modules",
                "../../node_modules",
            ] {
                let npm_shell = exe_dir.join(nm_rel).join("@almadar/shell");
                if npm_shell.exists() {
                    if let Some(parent) = npm_shell.parent() {
                        return parent.to_path_buf();
                    }
                }
            }

            // Local build / CI / install layout: look for templates/ or shells/ nearby
            for dir_name in ["templates", "shells"] {
                for prefix in ["", "../", "../../", "../../../"] {
                    let path = exe_dir.join(format!("{}{}", prefix, dir_name));
                    if path.join("almadar-shell").exists() || path.join("typescript-shell").exists()
                    {
                        return path;
                    }
                }
            }
        }
    }

    // Check orbital-rust relative paths
    if let Some(parent) = manifest_dir.parent().and_then(|p| p.parent()) {
        let templates_path = parent.join("templates");
        if templates_path.exists() {
            return templates_path;
        }
        let shells_path = parent.join("shells");
        if shells_path.exists() {
            return shells_path;
        }
    }

    // Default fallback — will produce a clear "Shell not found" error with guidance
    PathBuf::from("packages")
}

/// Evaluate an S-expression and print the result.
///
/// The expression is parsed from JSON, evaluated in a fresh context (with optional
/// @entity and @payload bindings), and the result is printed according to `output_format`:
/// - "value"  — human-readable via Display (default)
/// - "json"   — compact JSON
/// - "pretty" — indented JSON
fn evaluate_sexpr(
    expr_src: Option<&str>,
    entity_json: Option<&str>,
    payload_json: Option<&str>,
    output_format: &str,
) -> anyhow::Result<()> {
    use orbital_core::Value;
    use orbital_core::{BindingContext, Evaluator, SExpression};
    use std::io::Read;

    // Read source: argument or stdin
    let src = if let Some(s) = expr_src {
        s.to_string()
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let src = src.trim();
    if src.is_empty() {
        anyhow::bail!("No S-expression provided. Pass it as an argument or via stdin.");
    }

    // Parse expression
    let expr: SExpression =
        serde_json::from_str(src).map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;

    // Build context
    let mut ctx = BindingContext::new();

    if let Some(json) = entity_json {
        let raw: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| anyhow::anyhow!("--entity JSON parse error: {}", e))?;
        ctx.set_entity(json_to_value(&raw));
    }

    if let Some(json) = payload_json {
        let raw: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| anyhow::anyhow!("--payload JSON parse error: {}", e))?;
        ctx.set_payload(json_to_value(&raw));
    }

    // Evaluate
    let evaluator = Evaluator::new();
    let result = evaluator
        .evaluate(&expr, &ctx)
        .map_err(|e| anyhow::anyhow!("Evaluation error: {}", e))?;

    // Output
    match output_format {
        "json" => {
            println!("{}", serde_json::to_string(&result)?);
        }
        "pretty" => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        _ => {
            // "value": human-readable — strings without quotes, numbers as integers when whole
            println!("{}", result.to_string_value());
        }
    }

    Ok(())
}

/// Convert serde_json::Value to orbital_core::Value
fn json_to_value(json: &serde_json::Value) -> orbital_core::Value {
    use orbital_core::Value;

    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::Array(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}
