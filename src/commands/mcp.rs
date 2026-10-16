//! `orb mcp`: the language's own MCP server over stdio. Every tool calls the
//! same function its `orb` subcommand does.

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::Path;

/// How a client starts the server: through npx, from the published `@almadar/orb` package.
const NPX_ARGS: [&str; 3] = ["-y", "@almadar/orb", "mcp"];

const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Orb is a programming language. A program is a finite graph of states and events: \
written as .lolo text, held as .orb data, compiled to a full-stack app. You build by composing \
installed behaviors (atoms own their state machine; you override data, event names and config), \
not by writing every state machine from scratch.\n\
Loop: (1) `behaviors` with no scopes lists the installed packages and topics; list a topic, then \
`describe_behavior` and `read_behavior` the ones that fit. (2) Write the .lolo file, importing them \
with `uses`. `reference` is the language reference; `operators`, `patterns` and `complete` tell you what the language accepts. \
(3) `validate` until it reports 0 errors and 0 warnings; every finding carries a code, a path and a \
fix. (4) `play` events through the machine to check it does what was asked. (5) Run it with \
`npx -y @almadar/orb serve <file>`.";

struct Tool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    call: fn(&Map<String, Value>) -> anyhow::Result<Value>,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "validate",
        description: "Validate a .lolo or .orb program with the full compiler pipeline. The bar is 0 errors and 0 warnings; each finding has a code, a path and a suggested fix.",
        schema: || {
            object(
                json!({
                    "path": { "type": "string", "description": "The .lolo or .orb file" },
                    "final": { "type": "boolean", "description": "Also run the finished-app checks" }
                }),
                &["path"],
            )
        },
        call: |a| {
            let report = crate::commands::validate::validation_report(
                Path::new(str_arg(a, "path")?),
                None,
                None,
                true,
                bool_arg(a, "final"),
            )?;
            Ok(serde_json::to_value(report)?)
        },
    },
    Tool {
        name: "complete",
        description: "What is valid at a character offset in a .lolo file: keywords, effect operators, pattern tags and props, installed behaviors' import paths, traits, orbitals and typed config keys.",
        schema: || {
            object(
                json!({
                    "path": { "type": "string", "description": "The .lolo file" },
                    "offset": { "type": "integer", "minimum": 0, "description": "Byte offset of the cursor" }
                }),
                &["path", "offset"],
            )
        },
        call: |a| {
            let source = std::fs::read_to_string(str_arg(a, "path")?)?;
            let offset = a
                .get("offset")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("missing required argument `offset`"))?;
            crate::completions(&source, usize::try_from(offset)?)
        },
    },
    Tool {
        name: "format",
        description: "Pretty-print a .lolo or .orb file in the syntax it is written in. Returns the text; `write: true` also rewrites the file.",
        schema: || {
            object(
                json!({
                    "path": { "type": "string" },
                    "write": { "type": "boolean", "description": "Rewrite the file in place" }
                }),
                &["path"],
            )
        },
        call: |a| {
            let path = Path::new(str_arg(a, "path")?);
            let source = crate::commands::parse::formatted_source(path)?;
            if bool_arg(a, "write") {
                std::fs::write(path, &source)?;
            }
            Ok(json!({ "source": source }))
        },
    },
    Tool {
        name: "operators",
        description: "The language's operators (the only functions and effects a program can call). With `name`: that operator's full entry (params, return type, examples). Without: every operator's name, category and description, optionally for one `category`.",
        schema: || {
            object(
                json!({ "name": { "type": "string" }, "category": { "type": "string" } }),
                &[],
            )
        },
        call: |a| {
            let data = orbital_core::language_data::operator_data()?;
            let table: Value = serde_json::from_str(&data.operators)?;
            lookup(&table["operators"], a, "operators", &["category", "description"])
        },
    },
    Tool {
        name: "patterns",
        description: "The UI patterns `render-ui` accepts. With `name`: that pattern's full entry (every prop with its types and enum values). Without: every pattern's name, category and description, optionally for one `category`.",
        schema: || {
            object(
                json!({ "name": { "type": "string" }, "category": { "type": "string" } }),
                &[],
            )
        },
        call: |a| {
            let data = orbital_core::language_data::language_data()?;
            let table: Value = serde_json::from_str(&data.patterns)?;
            lookup(&table["patterns"], a, "patterns", &["category", "description"])
        },
    },
    Tool {
        name: "behaviors",
        description: "The installed behaviors you compose with `uses`. With no `scopes`: each installed package's prefix and its topics with a count. With `scopes` (e.g. `almadar-std/ui/core` or a `<prefix>/<topic>`): every behavior there with its description, capabilities, level and traits.",
        schema: || {
            object(
                json!({
                    "scopes": { "type": "array", "items": { "type": "string" } },
                    "exposure": { "type": "array", "items": { "type": "string", "enum": ["app", "experience", "palette", "both", "internal"] } }
                }),
                &[],
            )
        },
        call: |a| {
            let scopes = str_list_arg(a, "scopes")?;
            let exposure = str_list_arg(a, "exposure")?;
            if scopes.is_empty() {
                return installed_topics();
            }
            Ok(json!({ "behaviors": catalog_without_vectors(&scopes, &exposure)? }))
        },
    },
    Tool {
        name: "describe_behavior",
        description: "One behavior's contract: its knobs (typed config), events, transitions and pages. `spec` is `<prefix>/<name>`, `<prefix>/<name>.traits.<Trait>` or `<prefix>/<name>.orbitals.<Orbital>`.",
        schema: || object(json!({ "spec": { "type": "string" } }), &["spec"]),
        call: |a| {
            crate::commands::behaviors_catalog::describe(
                crate::commands::behaviors_catalog::DescribeTarget::Spec(
                    str_arg(a, "spec")?.to_string(),
                ),
            )
        },
    },
    Tool {
        name: "read_behavior",
        description: "A behavior's source file, to read as a worked example of the language. `spec` as in `describe_behavior`.",
        schema: || object(json!({ "spec": { "type": "string" } }), &["spec"]),
        call: |a| {
            let path = crate::commands::behaviors_catalog::behavior_source_path(str_arg(a, "spec")?)?;
            let source = std::fs::read_to_string(&path)?;
            Ok(json!({ "path": path, "source": source }))
        },
    },
    Tool {
        name: "play",
        description: "Fire events through a program's state machines in the real kernel, in order. Each step names a `trait` and an `event`, with an optional `payload` and an optional `from` state to start that trait in. Returns, per step, every trait that moved: the transition, the guards tried, the effects run and the events emitted, plus each trait's state after the step.",
        schema: || {
            object(
                json!({
                    "path": { "type": "string" },
                    "steps": { "type": "array", "items": object(json!({
                        "trait": { "type": "string" },
                        "event": { "type": "string" },
                        "payload": { "type": "object" },
                        "from": { "type": "string" }
                    }), &["trait", "event"]) }
                }),
                &["path", "steps"],
            )
        },
        call: |a| {
            let steps = a
                .get("steps")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `steps`"))?;
            crate::commands::execute::run_cascade(Path::new(str_arg(a, "path")?), steps)
        },
    },
    Tool {
        name: "test",
        description: "The exhaustive walk: every transition of every trait, generated from the graph and played headlessly from the initial state. Returns passed/failed/skipped counts and each case's outcome; a failure names the state it expected and the one it got.",
        schema: || object(json!({ "path": { "type": "string" } }), &["path"]),
        call: |a| crate::commands::test::exhaustive_test_report(Path::new(str_arg(a, "path")?)),
    },
    Tool {
        name: "reference",
        description: "The language reference, generated from the compiler. With no `topic`: every construct, the vocabulary (keywords, tags, sigils, effects, types) and the validation-code topics. With a construct `topic` (e.g. `Trait`, `Entity`, `Listen`, `Page`): its description, its fields and real examples from the installed std. Also `vocabulary` and `codes/<category>` (every code with what it rejects).",
        schema: || object(json!({ "topic": { "type": "string" } }), &[]),
        call: |a| match a.get("topic").and_then(Value::as_str) {
            None => Ok(crate::commands::reference::index()),
            Some(topic) => crate::commands::reference::topic(topic, std_programs()?),
        },
    },
    Tool {
        name: "evaluate",
        description: "Evaluate one S-expression (as JSON, e.g. [\"+\", 2, 3]) with the language's evaluator, with optional `entity` and `payload` bindings for @entity and @payload.",
        schema: || {
            object(
                json!({
                    "expr": { "description": "The S-expression as JSON" },
                    "entity": { "type": "object" },
                    "payload": { "type": "object" }
                }),
                &["expr"],
            )
        },
        call: |a| {
            let expr = a
                .get("expr")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `expr`"))?;
            let value =
                crate::utils::evaluate::evaluate_value(expr, a.get("entity"), a.get("payload"))?;
            Ok(json!({ "value": value }))
        },
    },
];

/// The installed std's programs, parsed once per server.
fn std_programs() -> anyhow::Result<&'static [orbital_lolo::ast::Program]> {
    static PROGRAMS: std::sync::OnceLock<Vec<orbital_lolo::ast::Program>> =
        std::sync::OnceLock::new();
    if let Some(p) = PROGRAMS.get() {
        return Ok(p);
    }
    let loaded = crate::commands::reference::std_programs()?;
    Ok(PROGRAMS.get_or_init(|| loaded))
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required argument `{key}`"))
}

fn bool_arg(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn str_list_arg(args: &Map<String, Value>, key: &str) -> anyhow::Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("`{key}` must be a list of strings"))
            })
            .collect(),
        Some(_) => anyhow::bail!("`{key}` must be a list of strings"),
    }
}

/// One entry of a language table by `name`, or the table's index (name + `fields`).
fn lookup(
    table: &Value,
    args: &Map<String, Value>,
    noun: &str,
    fields: &[&str],
) -> anyhow::Result<Value> {
    let entries = table
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("the installed {noun} table is not an object"))?;
    if let Some(name) = args.get("name").and_then(Value::as_str) {
        let entry = entries
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no {noun} entry named `{name}`"))?;
        return Ok(json!({ "name": name, "entry": entry }));
    }
    let category = args.get("category").and_then(Value::as_str);
    let index: Vec<Value> = entries
        .iter()
        .filter(|(_, e)| category.is_none_or(|c| e["category"] == c))
        .map(|(name, e)| {
            let mut row = Map::new();
            row.insert("name".into(), Value::String(name.clone()));
            for f in fields {
                row.insert((*f).into(), e[*f].clone());
            }
            Value::Object(row)
        })
        .collect();
    Ok(json!({ noun: index }))
}

/// A catalog for an agent: the embedding vectors are for search by meaning, not for reading.
fn catalog_without_vectors(scopes: &[String], exposure: &[String]) -> anyhow::Result<Value> {
    let entries = crate::commands::behaviors_catalog::catalog_with(
        scopes,
        exposure,
        crate::commands::behaviors_catalog::Vectors::Omit,
        None,
    )?;
    let mut value = serde_json::to_value(entries)?;
    for entry in value.as_array_mut().into_iter().flatten() {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("vectors");
            obj.remove("model");
        }
    }
    Ok(value)
}

fn installed_topics() -> anyhow::Result<Value> {
    let packages = crate::commands::npm_project::discover_from(&std::env::current_dir()?)?;
    let mut out = Vec::new();
    for pkg in packages {
        let entries = catalog_without_vectors(std::slice::from_ref(&pkg.prefix), &[])?;
        let mut topics = std::collections::BTreeMap::<String, u64>::new();
        for e in entries.as_array().into_iter().flatten() {
            if let Some(t) = e["topic"].as_str() {
                *topics.entry(t.to_string()).or_default() += 1;
            }
        }
        out.push(json!({ "prefix": pkg.prefix, "package": pkg.name, "topics": topics }));
    }
    Ok(topics_reply(out))
}

/// The command a user (or agent) runs to install the standard library into the user store.
const SETUP_COMMAND: &str = "npx -y @almadar/orb update --global";

/// An empty catalog names the fix instead of answering with nothing to compose.
fn topics_reply(packages: Vec<Value>) -> Value {
    if packages.is_empty() {
        json!({
            "packages": [],
            "hint": format!("No behavior packages are installed, so there is nothing to compose yet. Run `{SETUP_COMMAND}` to install @almadar/std (the standard library) into ~/.orb, then call `behaviors` again.")
        })
    } else {
        json!({ "packages": packages })
    }
}

/// `orb mcp install`: register the server, then set the language up now, so the client's first
/// tool call does not wait on the download. A failed setup is a warning naming the fix.
pub(crate) fn install(
    dir: &Path,
    client: McpClient,
    set_up: impl FnOnce() -> Result<(), String>,
) -> anyhow::Result<(std::path::PathBuf, Option<String>)> {
    let path = install_config(dir, client)?;
    let warning = set_up().err().map(|e| {
        format!("the language is not set up yet ({e}); run `{SETUP_COMMAND}`, or orb retries on the first tool call")
    });
    Ok((path, warning))
}

fn tool_result(outcome: anyhow::Result<Value>) -> Value {
    match outcome {
        Ok(value) => {
            let structured = match value {
                Value::Object(_) => value,
                other => json!({ "value": other }),
            };
            json!({
                "content": [{ "type": "text", "text": structured.to_string() }],
                "structuredContent": structured,
                "isError": false
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("{e:#}") }],
            "isError": true
        }),
    }
}

/// Whether the language has been set up for this server process.
static LANGUAGE_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs `setup` until it succeeds once, then never again: `orb mcp` is often a fresh user's
/// first command, and a failed setup (offline) is retried on the next call, never cached.
pub(crate) fn language_gate(
    ready: &std::sync::atomic::AtomicBool,
    setup: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if ready.load(Ordering::SeqCst) {
        return Ok(());
    }
    setup()?;
    ready.store(true, Ordering::SeqCst);
    Ok(())
}

fn call_tool(params: &Value) -> Value {
    call_tool_with(params, || {
        language_gate(&LANGUAGE_READY, || {
            crate::utils::patterns::ensure_language_ready().map_err(|e| format!("{}: {e}", e.code()))
        })
    })
}

/// A tool runs once the language is set up; a setup that failed is the tool's answer.
pub(crate) fn call_tool_with(params: &Value, ready: impl FnOnce() -> Result<(), String>) -> Value {
    if let Err(e) = ready() {
        return tool_result(Err(anyhow::anyhow!("the language is not set up: {e}")));
    }
    let name = params["name"].as_str().unwrap_or_default();
    let empty = Map::new();
    let args = params["arguments"].as_object().unwrap_or(&empty);
    tool_result(match TOOLS.iter().find(|t| t.name == name) {
        Some(tool) => (tool.call)(args),
        None => Err(anyhow::anyhow!("orb has no tool `{name}`")),
    })
}

fn build_prompt(params: &Value) -> Result<Value, (i64, String)> {
    if params["name"] != "build" {
        return Err((-32602, format!("unknown prompt {}", params["name"])));
    }
    let app = params["arguments"]["app"]
        .as_str()
        .unwrap_or("the app the user asked for");
    Ok(json!({
        "description": "Build an app in Orb",
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": format!("Build {app} in Orb.\n\n{INSTRUCTIONS}") }
        }]
    }))
}

/// One JSON-RPC message in, at most one out (a notification gets none).
pub(crate) fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let outcome: Result<Value, (i64, String)> = match message["method"].as_str().unwrap_or_default()
    {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {}, "prompts": {} },
                "serverInfo": { "name": "orb", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": TOOLS.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)()
            })).collect::<Vec<_>>()
        })),
        "tools/call" => Ok(call_tool(&params)),
        "prompts/list" => Ok(json!({
            "prompts": [{
                "name": "build",
                "description": "Build an app in Orb, from finding behaviors to a program that validates 0/0",
                "arguments": [{ "name": "app", "description": "What to build", "required": false }]
            }]
        })),
        "prompts/get" => build_prompt(&params),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

/// The MCP clients `orb mcp install` writes a project config for.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum McpClient {
    /// Claude Code (`.mcp.json`)
    Claude,
    /// Cursor (`.cursor/mcp.json`)
    Cursor,
    /// VS Code (`.vscode/mcp.json`)
    Vscode,
}

/// Register `orb mcp` in `dir`'s project config for `client`, keeping every other entry.
pub(crate) fn install_config(dir: &Path, client: McpClient) -> anyhow::Result<std::path::PathBuf> {
    let (path, servers_key, entry) = match client {
        McpClient::Claude => (
            dir.join(".mcp.json"),
            "mcpServers",
            json!({ "command": "npx", "args": NPX_ARGS }),
        ),
        McpClient::Cursor => (
            dir.join(".cursor").join("mcp.json"),
            "mcpServers",
            json!({ "command": "npx", "args": NPX_ARGS }),
        ),
        McpClient::Vscode => (
            dir.join(".vscode").join("mcp.json"),
            "servers",
            json!({ "type": "stdio", "command": "npx", "args": NPX_ARGS }),
        ),
    };
    let mut config: Map<String, Value> = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?).map_err(|e| {
            anyhow::anyhow!(
                "{} is not a JSON object ({e}); left unchanged",
                path.display()
            )
        })?
    } else {
        Map::new()
    };
    let servers = config
        .entry(servers_key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`{servers_key}` in {} is not an object", path.display()))?;
    servers.insert("orb".to_string(), entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&config)? + "\n")?;
    Ok(path)
}

/// `orb mcp`: newline-delimited JSON-RPC on stdin/stdout until stdin closes.
pub(crate) fn run_mcp() -> anyhow::Result<()> {
    super::npm_project::mark_stdout_as_protocol();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message),
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            })),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::handle;
    use serde_json::{json, Value};

    fn request(method: &str, params: Value) -> Value {
        handle(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }))
            .expect("a request gets a response")
    }

    fn call(name: &str, arguments: Value) -> Value {
        request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )["result"]
            .clone()
    }

    /// `orb mcp` is often a fresh user's first command (the orb.almadar.io MCP route): an
    /// empty user store has to be set up before a tool reads it, and a failed setup says so.
    #[test]
    fn a_tool_reports_a_failed_language_setup_instead_of_an_empty_answer() {
        let r = super::call_tool_with(
            &json!({ "name": "behaviors", "arguments": {} }),
            || Err("ORB_LANGUAGE_DATA_NOT_INSTALLED: offline".to_string()),
        );
        assert_eq!(r["isError"], true, "{r}");
        assert!(r["content"][0]["text"].as_str().unwrap().contains("offline"), "{r}");
        let ok = super::call_tool_with(&json!({ "name": "behaviors", "arguments": {} }), || Ok(()));
        assert_eq!(ok["isError"], false, "control: a ready language runs the tool: {ok}");
    }

    #[test]
    fn behaviors_with_nothing_installed_names_the_fix() {
        let empty = super::topics_reply(vec![]);
        assert_eq!(empty["packages"], json!([]));
        assert!(empty["hint"].as_str().unwrap().contains("npx -y @almadar/orb update --global"), "{empty}");
        let some = super::topics_reply(vec![json!({ "prefix": "almadar-std" })]);
        assert!(some.get("hint").is_none(), "control: an installed catalog carries no hint: {some}");
    }

    #[test]
    fn mcp_install_sets_the_language_up_and_a_failure_names_the_fix() {
        let dir = std::env::temp_dir().join(format!("orb-mcp-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, warning) = super::install(&dir, super::McpClient::Claude, || Err("offline".to_string())).unwrap();
        assert!(path.exists(), "the config is written even when setup fails");
        let warning = warning.expect("a failed setup warns");
        assert!(warning.contains("offline") && warning.contains("npx -y @almadar/orb update --global"), "{warning}");
        let (_, ok) = super::install(&dir, super::McpClient::Claude, || Ok(())).unwrap();
        assert!(ok.is_none(), "control: a set-up language warns nothing");
    }

    #[test]
    fn language_setup_runs_until_it_succeeds_then_never_again() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let ready = AtomicBool::new(false);
        let runs = AtomicUsize::new(0);
        let attempt = |ok: bool| {
            super::language_gate(&ready, || {
                runs.fetch_add(1, Ordering::SeqCst);
                if ok { Ok(()) } else { Err("offline".to_string()) }
            })
        };
        assert!(attempt(false).is_err());
        assert!(attempt(true).is_ok());
        assert!(attempt(false).is_ok(), "once ready it is not run again");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn initialize_names_orb_and_offers_tools_and_prompts() {
        let r = request("initialize", json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["serverInfo"]["name"], "orb");
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"]["capabilities"]["prompts"].is_object());
        assert!(r["result"]["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("validate")));
    }

    #[test]
    fn initialize_answers_an_unknown_protocol_version_with_its_own() {
        let r = request("initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn a_notification_gets_no_response() {
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none()
        );
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let r = request("resources/subscribe", json!({}));
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn ping_answers_empty() {
        assert_eq!(request("ping", json!({}))["result"], json!({}));
    }

    #[test]
    fn tools_list_carries_every_tool_with_an_object_schema() {
        let r = request("tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().expect("tools");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in [
            "validate",
            "complete",
            "format",
            "operators",
            "patterns",
            "behaviors",
            "describe_behavior",
            "read_behavior",
            "play",
            "test",
            "evaluate",
            "reference",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}: {names:?}"
            );
        }
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_not_a_protocol_error() {
        let r = call("make_coffee", json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("make_coffee"));
    }

    #[test]
    fn a_missing_required_argument_is_a_tool_error() {
        let r = call("validate", json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn evaluate_runs_the_one_evaluator() {
        let r = call("evaluate", json!({ "expr": ["+", 2, 3] }));
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["value"], 5.0);
    }

    #[test]
    fn evaluate_binds_entity_and_payload() {
        let r = call(
            "evaluate",
            json!({ "expr": [">", "@payload.amount", "@entity.total"], "entity": { "total": 3 }, "payload": { "amount": 4 } }),
        );
        assert_eq!(r["structuredContent"]["value"], true, "{r}");
    }

    #[test]
    fn format_returns_the_pretty_printed_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.orb");
        std::fs::write(&path, r#"{"b":1,"a":2}"#).unwrap();
        let r = call("format", json!({ "path": path }));
        assert_eq!(r["isError"], false, "{r}");
        let text = r["structuredContent"]["source"].as_str().unwrap();
        assert!(text.find("\"a\"").unwrap() < text.find("\"b\"").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"b":1,"a":2}"#);
    }

    #[test]
    fn format_with_write_rewrites_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.orb");
        std::fs::write(&path, r#"{"b":1,"a":2}"#).unwrap();
        let r = call("format", json!({ "path": path, "write": true }));
        assert_eq!(r["isError"], false, "{r}");
        assert!(std::fs::read_to_string(&path).unwrap().contains('\n'));
    }

    fn door_program(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("door.orb");
        std::fs::write(
            &path,
            json!({ "name": "Door", "version": "1.0.0", "orbitals": [{
                "name": "DoorOrbital",
                "entity": { "name": "Door", "persistence": "runtime", "fields": [{ "name": "id", "type": "string", "required": true }] },
                "traits": [{ "name": "DoorLife", "linkedEntity": "Door", "category": "interaction", "stateMachine": {
                    "states": [{ "name": "closed", "isInitial": true }, { "name": "open" }],
                    "events": [{ "key": "INIT", "name": "INIT" }, { "key": "OPEN", "name": "OPEN" }, { "key": "CLOSE", "name": "CLOSE" }],
                    "transitions": [
                        { "from": "closed", "to": "closed", "event": "INIT" },
                        { "from": "closed", "to": "open", "event": "OPEN" },
                        { "from": "open", "to": "closed", "event": "CLOSE" }
                    ]
                } }],
                "pages": []
            }] })
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn test_walks_every_generated_case_and_reports_each() {
        let dir = tempfile::tempdir().unwrap();
        let r = call("test", json!({ "path": door_program(dir.path()) }));
        assert_eq!(r["isError"], false, "{r}");
        let report = &r["structuredContent"];
        assert_eq!(report["failed"], 0, "{report}");
        assert!(report["passed"].as_u64().unwrap() > 0, "{report}");
        let outcomes = report["outcomes"].as_array().unwrap();
        assert!(outcomes
            .iter()
            .any(|o| o["status"] == "passed" && o["traitName"] == "DoorLife"));
    }

    #[test]
    fn test_on_a_missing_file_is_a_tool_error() {
        let r = call("test", json!({ "path": "/nonexistent/door.orb" }));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn install_writes_a_new_claude_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::install_config(dir.path(), super::McpClient::Claude).unwrap();
        assert_eq!(path, dir.path().join(".mcp.json"));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({ "mcpServers": { "orb": { "command": "npx", "args": ["-y", "@almadar/orb", "mcp"] } } })
        );
    }

    #[test]
    fn install_keeps_the_other_servers_in_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".cursor")).unwrap();
        let existing = dir.path().join(".cursor").join("mcp.json");
        std::fs::write(
            &existing,
            r#"{"mcpServers":{"other":{"command":"x"}},"note":1}"#,
        )
        .unwrap();
        super::install_config(dir.path(), super::McpClient::Cursor).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&existing).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"], json!({ "command": "x" }));
        assert_eq!(
            written["mcpServers"]["orb"]["args"],
            json!(["-y", "@almadar/orb", "mcp"])
        );
        assert_eq!(written["note"], 1);
    }

    #[test]
    fn install_writes_the_vscode_servers_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::install_config(dir.path(), super::McpClient::Vscode).unwrap();
        assert_eq!(path, dir.path().join(".vscode").join("mcp.json"));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({ "servers": { "orb": { "type": "stdio", "command": "npx", "args": ["-y", "@almadar/orb", "mcp"] } } })
        );
    }

    #[test]
    fn install_refuses_to_overwrite_a_config_that_is_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join(".mcp.json");
        std::fs::write(&existing, "not json").unwrap();
        assert!(super::install_config(dir.path(), super::McpClient::Claude).is_err());
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "not json");
    }

    #[test]
    fn reference_without_a_topic_is_the_index() {
        let r = call("reference", json!({}));
        assert_eq!(r["isError"], false, "{r}");
        assert!(
            r["structuredContent"]["constructs"]
                .as_array()
                .unwrap()
                .len()
                > 10
        );
    }

    #[test]
    fn reference_codes_topic_names_each_code() {
        let r = call("reference", json!({ "topic": "codes/circuit" }));
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["category"], "circuit");
    }

    #[test]
    fn prompts_list_and_get_the_build_loop() {
        let list = request("prompts/list", json!({}));
        assert_eq!(list["result"]["prompts"][0]["name"], "build");
        let got = request(
            "prompts/get",
            json!({ "name": "build", "arguments": { "app": "a chat app" } }),
        );
        let text = got["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("a chat app"));
        assert!(text.contains("validate"));
    }

    #[test]
    fn prompts_get_an_unknown_prompt_is_invalid_params() {
        let r = request("prompts/get", json!({ "name": "nope" }));
        assert_eq!(r["error"]["code"], -32602);
    }
}
