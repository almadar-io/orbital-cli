//! The native host of the language trio's server effects (`program/read|print|validate`, `behavior/catalog|describe|
//! source|resolve`): the kernel's `SubstrateHandler` answered by `orb`'s own functions, in process. Its JS twin is
//! `runProgramEffect` in `@almadar/runtime` (`host/program`), which runs these same functions through the `orb`
//! commands (`emit orb`, `emit lolo`, `stamp` + `validate --json --deny-warnings`, `behaviors catalog|describe|source
//! --json`, `resolve`); each answer here is the value those commands print, staged the same way.

use crate::reflect::{self, DescribeTarget};
use orbital_core::effects::executor::{SubstrateFailure, SubstrateHandler};
use orbital_core::Value;
use std::path::{Path, PathBuf};

/// The program host of the project `orb` runs in: behavior specifiers and relative imports resolve from the process's
/// working directory, as the `orb` commands resolve them.
pub(crate) struct ProgramHost;

pub(super) fn failure(message: impl Into<String>) -> SubstrateFailure {
    SubstrateFailure {
        message: message.into(),
        errors: Vec::new(),
    }
}

fn parsed_text(text: &str, path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    crate::utils::parse_schema_auto(text, &path.display().to_string()).map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))
}

/// The program text at `path`, parsed (`.lolo`, an unresolved `.orb` or a resolved one).
fn parsed(path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let content = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    crate::utils::parse_schema_auto(&content, &path.display().to_string()).map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum PrintTarget {
    Orb,
    Ts,
    Py,
    Lolo,
    Manifest,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PrintFormat {
    Stream,
    Markdown,
    Json,
    Raw,
}

struct PrintRequest {
    target: PrintTarget,
    path: PathBuf,
    section: Option<String>,
    output: Option<PathBuf>,
    format: Option<PrintFormat>,
    only: Option<String>,
    map: bool,
    unit: Option<String>,
    out: Option<PathBuf>,
    signer: Option<(PathBuf, String)>,
}

#[derive(Default)]
struct Printed {
    stdout: String,
    stderr: String,
}

impl Printed {
    fn note(&mut self, line: &str) {
        self.stderr.push_str(line);
        self.stderr.push('\n');
    }

    fn write(&mut self, text: &str, file: Option<&Path>) -> anyhow::Result<()> {
        match file {
            Some(file) => std::fs::write(file, text).map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", file.display())),
            None => {
                self.stdout.push_str(text);
                if !text.ends_with('\n') {
                    self.stdout.push('\n');
                }
                Ok(())
            }
        }
    }
}

/// The lowered program of `source` as a value, keys in declaration order.
fn orb_value(source: &str, source_path: &str) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::to_value(crate::utils::parse_schema_auto(source, source_path)?)?)
}

/// The program as `.lolo` text.
fn lolo_text(source: &str, source_path: &str) -> anyhow::Result<String> {
    orbital_lolo::schema_to_lolo(&crate::utils::parse_schema_auto(source, source_path)?).map_err(|e| anyhow::anyhow!("{source_path}: {e}"))
}

impl PrintRequest {
    fn print(&self, printed: &mut Printed) -> anyhow::Result<()> {
        let source = std::fs::read_to_string(&self.path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", self.path.display()))?;
        let source_path = self.path.display().to_string();
        match self.target {
            PrintTarget::Orb => self.print_orb(&source, &source_path, printed),
            PrintTarget::Lolo => Ok(printed.write(&lolo_text(&source, &source_path)?, self.output.as_deref())?),
            PrintTarget::Manifest => self.print_manifest(&source, &source_path, printed),
            PrintTarget::Ts | PrintTarget::Py => self.print_code(&source, &source_path, printed),
        }
    }

    fn print_orb(&self, source: &str, source_path: &str, printed: &mut Printed) -> anyhow::Result<()> {
        let prior = self
            .output
            .as_deref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| orbital_core::schema::parse_schema(&crate::utils::translate_schema_content(&s)).ok());
        let schema = crate::utils::parse_schema_auto_anchored(source, source_path, prior.as_ref())?;
        printed.write(&orbital_lolo::program::canonical_orb_json(&schema)?, self.output.as_deref())?;
        let (Some(out), true) = (self.output.as_deref(), schema.locales.as_ref().is_some_and(|l| !l.is_empty())) else { return Ok(()) };
        let program_dir = Path::new(source_path).parent().unwrap_or(Path::new("."));
        let out_dir = out.parent().unwrap_or(Path::new("."));
        for locale in schema.locales.iter().flatten() {
            let file = Path::new("locales").join(locale).join(format!("{}.json", schema.name));
            let (from, to) = (program_dir.join(&file), out_dir.join(&file));
            if from.exists() && from.canonicalize().ok() != to.canonicalize().ok() {
                std::fs::create_dir_all(to.parent().unwrap_or(out_dir))?;
                std::fs::copy(&from, &to).map_err(|e| anyhow::anyhow!("Failed to copy {} to {}: {e}", from.display(), to.display()))?;
            }
        }
        Ok(())
    }

    fn print_manifest(&self, source: &str, source_path: &str, printed: &mut Printed) -> anyhow::Result<()> {
        use orbital_core::sandbox_manifest as manifest;
        if self.output.is_some() {
            anyhow::bail!("`orb emit manifest` writes one file per unit: use --out <dir>, not --output");
        }
        let signer = match &self.signer {
            Some((path, key_id)) => {
                let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;
                let seed = manifest::parse_seed(&text).ok_or_else(|| anyhow::anyhow!("{}: the seed must be 64 hex characters", path.display()))?;
                Some((seed, key_id.as_str()))
            }
            None => None,
        };
        crate::utils::require_language_data()?;
        let schema = crate::utils::parse_schema_auto(source, source_path)?;
        let digest = manifest::sha256_hex(orbital_lolo::program::canonical_orb_json(&schema)?.as_bytes());
        let (resolved, _) = orbital_compiler::phases::lazy::resolve_program_with_path(
            schema,
            &orbital_compiler::CompilerOptions::from_env(),
            Some(Path::new(source_path)),
        )
        .map_err(|e| anyhow::anyhow!("resolve orb schema: {e}"))?;
        let units = match &self.unit {
            Some(unit) => vec![unit.clone()],
            None => manifest::unit_names(&resolved),
        };
        if units.is_empty() {
            anyhow::bail!("{source_path}: `deployment` declares no units");
        }
        if self.out.is_none() && units.len() > 1 {
            anyhow::bail!("{source_path} has {} units ({}): pass --out <dir> or --unit <name>", units.len(), units.join(", "));
        }
        for unit in &units {
            let built = manifest::Manifest::build(&resolved, unit, &digest).map_err(|e| anyhow::anyhow!("{source_path}: {e}"))?;
            let rendered = match signer {
                Some((seed, key_id)) => manifest::sign_manifest(&built.to_unsigned_json(), key_id, &seed).map_err(|e| anyhow::anyhow!("{e}"))?,
                None => serde_json::to_string_pretty(&built.to_unsigned_json())?,
            };
            match &self.out {
                Some(dir) => {
                    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("Failed to create {}: {e}", dir.display()))?;
                    printed.write(&rendered, Some(&dir.join(format!("{unit}.manifest.json"))))?;
                }
                None => printed.write(&rendered, None)?,
            }
            if signer.is_none() {
                printed.note(&format!("orb: manifest for unit `{unit}` is UNSIGNED (no --sign-key); the io-gate refuses it until `io-gate sign` signs it"));
            }
        }
        Ok(())
    }

    fn print_code(&self, source: &str, source_path: &str, printed: &mut Printed) -> anyhow::Result<()> {
        use orbital_lolo::program::generated::{self, CodeTarget, Format, Only, Section};
        crate::utils::require_language_data()?;
        let schema = crate::utils::parse_schema_auto(source, source_path)?;
        let mut compiler_options = orbital_compiler::CompilerOptions::from_env();
        compiler_options.service_runtimes = Some(crate::utils::load_services_registry(None)?.runtimes);
        let unresolved = schema.clone();
        let module = orbital_compiler::phases::compile_schema(schema, &compiler_options).map_err(|e| anyhow::anyhow!("Schema compilation failed: {e}"))?;
        let options = orbital_compiler::GenerateOptions { compiler: compiler_options, ..Default::default() };
        let program_path = Path::new(source_path);
        let (target, output) = if self.target == PrintTarget::Ts {
            let (resolved, _) = orbital_compiler::phases::lazy::resolve_program_with_path(
                unresolved,
                &orbital_compiler::CompilerOptions::from_env(),
                Some(program_path),
            )
            .map_err(|e| anyhow::anyhow!("Failed to resolve schema for the browser store: {e}"))?;
            (CodeTarget::Ts, crate::utils::shell_programs::emit("typescript", &module, Some(resolved), None, program_path, &options, Default::default())?)
        } else {
            (CodeTarget::Py, crate::utils::shell_programs::emit("python", &module, None, None, program_path, &options, Default::default())?)
        };
        let section = self.section.as_deref().map(Section::parse).transpose().map_err(|e| anyhow::anyhow!(e))?.unwrap_or(Section::All);
        let only = self.only.as_deref().map(Only::parse_list).transpose().map_err(|e| anyhow::anyhow!(e))?;
        let files = generated::select(&output.files, &section, only.as_deref(), target);
        if files.is_empty() {
            printed.note(&format!("warning: selector {} matched zero generated files", self.section.as_deref().unwrap_or("(default)")));
        }
        let format = match (self.format, self.output.as_deref().and_then(|p| p.extension()).and_then(|e| e.to_str())) {
            (Some(PrintFormat::Stream), _) => Format::Stream,
            (Some(PrintFormat::Markdown), _) => Format::Markdown,
            (Some(PrintFormat::Json), _) => Format::Json,
            (Some(PrintFormat::Raw), _) => Format::Raw,
            (None, Some("md" | "markdown")) => Format::Markdown,
            (None, Some("json")) => Format::Json,
            (None, _) => Format::Stream,
        };
        let rendered = generated::render(&files, format, target)?;
        if self.map && matches!(format, Format::Stream | Format::Markdown | Format::Raw) {
            self.print_map(source, source_path, target, &files, &rendered, printed)?;
        }
        printed.write(&rendered, self.output.as_deref())?;
        Ok(())
    }

    fn print_map(
        &self,
        orb_source: &str,
        orb_path: &str,
        target: orbital_lolo::program::generated::CodeTarget,
        files: &[&orbital_core::generate::GeneratedFile],
        rendered: &str,
        printed: &mut Printed,
    ) -> anyhow::Result<()> {
        use base64::Engine;
        let spans = orbital_lolo::orb_json::OrbSpans::scan(orb_source);
        let anchor = files.iter().find_map(|f| orbital_lolo::program::generated::anchor(f, target).and_then(|(kind, name)| spans.lookup(kind, &name)));
        match (self.output.as_deref(), anchor) {
            (Some(out), Some(range)) => {
                let mut map_path = out.as_os_str().to_owned();
                map_path.push(".map");
                let map_path = PathBuf::from(map_path);
                std::fs::write(&map_path, orbital_lolo::orb_json::source_map_v3(rendered, orb_source, orb_path, &range))
                    .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", map_path.display()))?;
                printed.note(&format!("source map: {}", map_path.display()));
            }
            (Some(_), None) => printed.note("warning: --map: no tracked node found in selected files; map skipped"),
            (None, Some(range)) if target == orbital_lolo::program::generated::CodeTarget::Ts => {
                let map = orbital_lolo::orb_json::source_map_v3(rendered, orb_source, orb_path, &range);
                let b64 = base64::engine::general_purpose::STANDARD.encode(map.as_bytes());
                printed.stdout.push_str(&format!("//# sourceMappingURL=data:application/json;base64,{b64}\n"));
            }
            (None, _) => {}
        }
        Ok(())
    }
}

/// `content` pretty-printed in the syntax it is written in: `.lolo` through the authored-AST formatter (comments kept),
/// `.orb` JSON with sorted keys. `label` names it in errors.
pub(crate) fn formatted_text(content: &str, label: &str) -> anyhow::Result<String> {
    use crate::utils::{detect_syntax, InputSyntax};
    Ok(match detect_syntax(content) {
        InputSyntax::Lolo => {
            let program = orbital_lolo::parse_str(content).map_err(|e| anyhow::anyhow!("Parse error in {}: {:?}", label, e))?;
            orbital_lolo::format_program(&program)
        }
        InputSyntax::OrbJson => {
            let value: serde_json::Value = serde_json::from_str(content).map_err(|e| anyhow::anyhow!("Invalid JSON in {}: {}", label, e))?;
            serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))?
        }
    })
}

/// What `program/read { stamp }` answers for the `.orb` at `path`: ids and ledger minted by the one `stamp_identity`
/// authority (`prior` anchors id reuse; `imports_dir` resolves cross-file id edges), keys sorted.
fn stamped_json(path: &Path, prior_path: Option<&Path>, imports_dir: Option<&Path>) -> anyhow::Result<String> {
    use orbital_compiler::loader::{ExternalOrbitalLoader, LoaderConfig};
    let load = |path: &Path, what: &str| -> anyhow::Result<orbital_core::OrbitalSchema> {
        let content = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {what}{}: {e}", path.display()))?;
        orbital_core::schema::parse_schema(&crate::utils::translate_schema_content(&content))
            .map_err(|e| anyhow::anyhow!("Failed to parse {what}{}: {e}", path.display()))
    };
    let mut schema = load(path, "")?;
    let prior = prior_path.map(|p| load(p, "prior ")).transpose()?;
    orbital_lolo::stamp_schema_identity(&mut schema, prior.as_ref(), &Default::default());
    if let Some(dir) = imports_dir {
        let packages = discover_packages(dir)?;
        let mut loader = ExternalOrbitalLoader::new(LoaderConfig { base_path: dir.to_path_buf(), packages, ..Default::default() });
        orbital_compiler::resolve_cross_file_id_edges(&mut schema, &mut loader)
            .map_err(|e| anyhow::anyhow!("Failed to resolve cross-file id edges: {}", e))?;
    }
    let value = serde_json::to_value(&schema).map_err(|e| anyhow::anyhow!("Failed to serialize stamped schema: {}", e))?;
    Ok(serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))?)
}

/// The behavior packages installed for `dir` (its own, then the user store).
pub(crate) fn discover_packages(dir: &Path) -> anyhow::Result<Vec<orbital_core::behavior_packages::BehaviorPackage>> {
    let user_store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| dir.to_path_buf());
    orbital_core::behavior_packages::discover(dir, &user_store).map_err(|e| anyhow::anyhow!("failed to discover behavior packages: {e}"))
}

/// The file and owning package a behavior specifier resolves to from the working directory.
fn which(spec: &str) -> anyhow::Result<Result<(PathBuf, String), String>> {
    use orbital_compiler::loader::{ExternalOrbitalLoader, LoaderConfig};
    let cwd = std::env::current_dir()?;
    let mut loader = ExternalOrbitalLoader::new(LoaderConfig { packages: discover_packages(&cwd)?, base_path: cwd, ..Default::default() });
    Ok(loader.resolve_owned(spec).map(|(path, owner)| (path, owner.to_string())).map_err(|e| e.to_string()))
}

/// The sidecar fetcher: `ureq`, with the request url (a capability token) never in an error.
pub(crate) struct UreqFetcher;

impl orbital_core::sidecar_store::SidecarFetcher for UreqFetcher {
    fn fetch(&self, url: &str, sink: &mut dyn std::io::Write) -> Result<(), String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(30))
            .timeout_read(std::time::Duration::from_secs(300))
            .build();
        let response = agent.get(url).call().map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            ureq::Error::Transport(t) => format!("transport error: {}", t.kind()),
        })?;
        std::io::copy(&mut response.into_reader(), sink).map(|_| ()).map_err(|e| format!("download interrupted: {:?}", e.kind()))
    }
}

/// Where a package's resolved `orb.vectors` snapshot sits relative to a fixed anchor.
struct VectorsPlace {
    kind: &'static str,
    rel: Option<String>,
}

fn slash_path(path: &Path) -> String {
    path.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// The anchor `vectors` lies under: `<cwd>/node_modules` by its real path (a linked package whose real path is
/// elsewhere, or a `node_modules` higher up, is outside), or the default sidecar store `~/.almadar/sidecars` when
/// `cache` is that directory.
fn vectors_place(vectors: &Path, cwd: &Path, cache: &Path) -> VectorsPlace {
    let outside = VectorsPlace { kind: "outside", rel: None };
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let home_store = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".almadar").join("sidecars"));
    if home_store.is_some_and(|store| real(&store) == real(cache)) {
        if let Ok(rel) = real(vectors).strip_prefix(real(cache)) {
            return VectorsPlace { kind: "sidecars", rel: Some(slash_path(rel)) };
        }
    }
    match real(vectors).strip_prefix(real(&cwd.join("node_modules"))) {
        Ok(rel) => VectorsPlace { kind: "node-modules", rel: Some(slash_path(rel)) },
        Err(_) => outside,
    }
}

/// The readable path of the package's `orb.vectors` snapshot: a package file as-is, a lock-resolved one from the cache,
/// downloaded and verified first when absent. `None` when none is declared.
pub(crate) fn resolve_vectors(
    pkg: &orbital_core::behavior_packages::BehaviorPackage,
    fetcher: &dyn orbital_core::sidecar_store::SidecarFetcher,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = &pkg.vectors else { return Ok(None) };
    match pkg.sidecars.iter().find(|s| &s.cache_path == path) {
        Some(sidecar) => orbital_core::sidecar_store::ensure_sidecar(&pkg.name, &pkg.version, sidecar, fetcher).map(Some).map_err(anyhow::Error::from),
        None => Ok(Some(path.clone())),
    }
}

/// The resolved program at `path` and each `uses lazy` behavior split off it, as `{ program, lazy: [{ orbital, program }] }`,
/// keys in declaration order; nothing is written, so a program with lazy behaviors or locales is not refused.
pub(crate) fn resolved_value(path: &Path) -> anyhow::Result<Value> {
    use orbital_compiler::phases::lazy::resolved_value as canonical;
    use orbital_core::value_serialize::to_value;
    let entry = super::dev::resolved_file(path)?;
    let (resolved, lazy) = (entry.schema(), entry.lazy());
    let lazy = lazy
        .iter()
        .map(|child| {
            Ok(Value::object(orbital_core::ValueMap::from_iter([
                ("orbital".into(), to_value(&child.orbital)?),
                ("program".into(), canonical(&child.schema).map_err(anyhow::Error::msg)?),
            ])))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Value::object(orbital_core::ValueMap::from_iter([
        ("program".into(), canonical(resolved).map_err(anyhow::Error::msg)?),
        ("lazy".into(), Value::array(lazy)),
    ])))
}

/// The program at `path` as it reads before resolution, and every trait and orbital reference its resolution resolves,
/// each naming the upstream declaration it resolved to inside its imported file's orbitals, held once per file
/// (`program/read { references: true }`).
fn referenced_program(path: &Path) -> anyhow::Result<(Value, orbital_compiler::phases::inline::ReferenceAnswer)> {
    let mut options = orbital_compiler::CompilerOptions::from_env();
    let session = orbital_compiler::phases::inline::InlineSession::default();
    session.collect_references();
    options.inline.session = Some(session.clone());
    let mut schema = parsed(path)?;
    let unresolved = orbital_core::value_serialize::to_canonical_value(&schema).map_err(|e| anyhow::anyhow!("Failed to serialize {}: {}", path.display(), e))?;
    let failed = |e: orbital_compiler::CompilerError| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e);
    orbital_compiler::phases::lazy::split_lazy_and_resolve(&mut schema, &options, Some(path)).map_err(failed)?;
    session.take_references();
    orbital_compiler::resolve_schema_with_path(schema, &options, Some(path)).map_err(failed)?;
    let root = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no directory", path.display()))?;
    let root = std::fs::canonicalize(root).map_err(|e| anyhow::anyhow!("Failed to read {}: {}", root.display(), e))?;
    Ok((unresolved, session.take_reference_answer(&root)))
}

/// What reading the program at `path` as resolved answers (`program/read { resolve }`).
pub(crate) struct ResolvedRead {
    pub(crate) text: String,
    /// The source carried the `resolved` stamp when it was loaded (EXEC-LOAD-1).
    pub(crate) loaded_resolved: bool,
    /// Where the resolved program lives on disk: the source when it was already resolved, `output` when it was
    /// resolved and written; absent when it was resolved in memory only.
    pub(crate) program_path: Option<PathBuf>,
}

/// `orb resolve`: the text of the program at `path` resolved (carrying its messages), written to `output` with its lazy children.
/// A resolved source is answered as it was read and never written out again (EXEC-LOAD-1).
pub(crate) fn resolved_read(path: &Path, output: Option<&Path>) -> anyhow::Result<ResolvedRead> {
    use crate::utils::{detect_syntax, InputSyntax};
    #[derive(serde::Deserialize)]
    struct Stamp {
        resolved: Option<orbital_core::schema::ResolvedMark>,
    }
    let content = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let mark = match detect_syntax(&content) {
        InputSyntax::OrbJson => serde_json::from_str::<Stamp>(&content)
            .map_err(|e| anyhow::anyhow!("Failed to read the resolved stamp of {}: {}", path.display(), e))?
            .resolved,
        InputSyntax::Lolo => None,
    };
    if let Some(mark) = mark {
        orbital_compiler::phases::check_resolved_mark(&mark, path.display().to_string())
            .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
        return Ok(ResolvedRead { text: content, loaded_resolved: true, program_path: Some(path.to_path_buf()) });
    }
    drop(content);
    let entry = super::dev::resolved_file(path)?;
    let (resolved, lazy) = (entry.schema(), entry.lazy());
    let text = orbital_compiler::phases::lazy::resolved_json(resolved).map_err(anyhow::Error::msg)?;
    match output {
        Some(out) => {
            std::fs::write(out, &text).map_err(|e| anyhow::anyhow!("Failed to write {}: {}", out.display(), e))?;
            orbital_compiler::phases::lazy::write_resolved_lazy(out.parent().unwrap_or(Path::new(".")), lazy)
                .map_err(|e| anyhow::anyhow!("Failed to write lazy behaviors beside {}: {}", out.display(), e))?;
        }
        None if !lazy.is_empty() => anyhow::bail!(
            "{} imports {} behavior(s) with `uses lazy`; each resolves to its own .orb — pass --output so they can be written beside it",
            path.display(),
            lazy.len()
        ),
        None => {}
    }
    Ok(ResolvedRead { text, loaded_resolved: false, program_path: output.map(Path::to_path_buf) })
}

fn to_json(value: &Value) -> serde_json::Value {
    orbital_core::json_value::value_to_json(value)
}

/// A whole program is read or composed as several transient forms; give their freed pages back before the next step
/// holds its own, so the process peaks at the largest step and not at the sum of the pages the steps left mapped.
fn release_transients() {
    orbital_core::runtime::memory_release::release_freed_memory();
}

fn serialized<T: serde::Serialize + ?Sized>(value: &T) -> Result<Value, SubstrateFailure> {
    orbital_core::value_serialize::to_value(value).map_err(|e| failure(e.to_string()))
}

fn from_json(value: serde_json::Value) -> Result<Value, SubstrateFailure> {
    serde_json::from_value(value).map_err(|e| failure(e.to_string()))
}

/// The `program/read` modes `options` turns on, in the order they are documented.
fn read_modes(options: Option<&Value>) -> Vec<&'static str> {
    ["stamp", "inspect", "embeds", "resolve", "references", "findings"].into_iter().filter(|mode| option_flag(options, mode) == Some(true)).collect()
}

/// A boolean option of an op's options argument; absent or not a boolean is `None`.
fn option_flag(options: Option<&Value>, key: &str) -> Option<bool> {
    options
        .map(to_json)
        .and_then(|o| o.get(key).and_then(serde_json::Value::as_bool))
}

/// An op's options object: typed reads by key, and a refusal of any key the op's form does not take.
struct Options<'a> {
    op: &'a str,
    map: &'a serde_json::Map<String, serde_json::Value>,
}

impl<'a> Options<'a> {
    fn of(op: &'a str, value: &'a serde_json::Value) -> Result<Self, SubstrateFailure> {
        value
            .as_object()
            .map(|map| Self { op, map })
            .ok_or_else(|| failure(format!("{op}: options must be an object")))
    }

    fn only(&self, form: &str, keys: &[&str]) -> Result<(), SubstrateFailure> {
        match self.map.keys().find(|k| !keys.contains(&k.as_str())) {
            Some(other) => Err(failure(format!("{}: unsupported option \"{other}\" with `{form}`", self.op))),
            None => Ok(()),
        }
    }

    fn text(&self, key: &str) -> Result<Option<String>, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(failure(format!("{}: `{key}` must be text", self.op))),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(failure(format!("{}: `{key}` must be a boolean", self.op))),
        }
    }

    fn number(&self, key: &str) -> Result<Option<u64>, SubstrateFailure> {
        match self.map.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| failure(format!("{}: `{key}` must be a whole number", self.op))),
        }
    }

    fn path(&self, key: &str) -> Result<Option<PathBuf>, SubstrateFailure> {
        Ok(self.text(key)?.map(PathBuf::from))
    }

    fn absolute(&self, key: &str, form: &str) -> Result<PathBuf, SubstrateFailure> {
        self.path(key)?
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure(format!("{}: `{form}` needs the program's absolute `{key}`", self.op)))
    }

    fn choice<T: clap::ValueEnum>(&self, key: &str) -> Result<Option<T>, SubstrateFailure> {
        self.text(key)?
            .map(|name| {
                T::from_str(&name, false)
                    .map_err(|_| failure(format!("{}: `{key}` \"{name}\" is not one of the accepted values", self.op)))
            })
            .transpose()
    }
}

fn has_option(options: Option<&Value>, key: &str) -> bool {
    options
        .map(to_json)
        .is_some_and(|o| o.get(key).is_some_and(|v| !v.is_null()))
}

/// `program/print`'s `{ source, node | nodes }` options: format authored `.lolo` text (comments
/// kept, as `orb format`) instead of the quoted program, whole or at a node path.
struct PrintSource {
    source: String,
    node: Option<String>,
    nodes: Option<String>,
}

impl PrintSource {
    fn of(options: Option<&Value>) -> Result<Option<Self>, SubstrateFailure> {
        let Some(options) = options.map(to_json) else { return Ok(None) };
        let text = |key: &str| -> Result<Option<String>, SubstrateFailure> {
            match options.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(failure(format!("program/print: `{key}` must be text"))),
            }
        };
        let (node, nodes) = (text("node")?, text("nodes")?);
        let Some(source) = text("source")? else {
            return match node.or(nodes) {
                Some(_) => Err(failure("program/print: `node` and `nodes` read the authored `source`, which is missing")),
                None => Ok(None),
            };
        };
        if node.is_some() && nodes.is_some() {
            return Err(failure("program/print: give `node` or `nodes`, not both"));
        }
        Ok(Some(PrintSource { source, node, nodes }))
    }

    fn print(self) -> Result<Value, SubstrateFailure> {
        let at = |path: &str| -> Result<Vec<String>, SubstrateFailure> {
            let program = orbital_lolo::parse_str(&self.source).map_err(|e| failure(format!("program/print: parse error: {e:?}")))?;
            orbital_lolo::format_path(&program, path).map_err(|e| failure(format!("program/print: {e}")))
        };
        match (&self.node, &self.nodes) {
            (Some(path), _) if path.contains("[*]") => Err(failure("program/print: `node` names one construct; use `nodes` for a `[*]` path")),
            (Some(path), _) => Ok(Value::string(at(path)?.remove(0))),
            (None, Some(path)) => from_json(serde_json::Value::from(at(path)?)),
            (None, None) => formatted_text(&self.source, "source")
                .map(Value::string)
                .map_err(|e| failure(format!("program/print: {e}"))),
        }
    }
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(prefix: &str) -> Result<Self, SubstrateFailure> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| failure(e.to_string()))?;
        Ok(Self(dir))
    }

    fn write(&self, name: &str, content: &str) -> Result<PathBuf, SubstrateFailure> {
        let path = self.0.join(name);
        std::fs::write(&path, content).map_err(|e| failure(e.to_string()))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn program_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A program argument: an object with a `name` and `orbitals`, every orbital value in it standing for its import
/// orbital (as `@almadar/core` `parseProgram` reads a program held as data).
fn program_value_arg(op: &str, value: Option<&Value>) -> Result<Value, SubstrateFailure> {
    match value {
        Some(program @ Value::Object(map))
            if map.get("name").is_some_and(Value::is_string) && map.contains_key("orbitals") =>
        {
            orbital_core::schema::behavior_value::into_program_with_orbital_imports(program.clone())
                .map_err(|e| failure(format!("{op}: {e}")))
        }
        _ => Err(failure(format!("{op}: not a program"))),
    }
}

fn program_arg(op: &str, value: Option<&Value>) -> Result<serde_json::Value, SubstrateFailure> {
    program_value_arg(op, value).map(|program| to_json(&program))
}

/// A program stamped (ids minted against `prior`) and validated: the findings, and the stamped program text when
/// stamping succeeded.
struct Checked {
    valid: bool,
    errors: Vec<serde_json::Value>,
    warnings: Vec<serde_json::Value>,
    stamped: Option<String>,
}

fn stamp_and_validate(
    op: &str,
    program: &serde_json::Value,
    prior: Option<&serde_json::Value>,
    final_program: bool,
) -> Result<Checked, SubstrateFailure> {
    if final_program {
        return Err(failure(format!("{op}: `final` is retired from the compiler (its checks were archived with the validators); pass a std lint's results as `findings`")));
    }
    let name = program
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let issue =
        |code: &str, message: String| serde_json::json!({ "code": code, "message": message });
    let refused = |error: serde_json::Value| Checked {
        valid: false,
        errors: vec![error],
        warnings: Vec::new(),
        stamped: None,
    };
    if !program_name_ok(&name) {
        return Ok(refused(issue(
            "PROGRAM_NAME_INVALID",
            format!("program name \"{name}\" is not a file name"),
        )));
    }
    let scratch = Scratch::new("almadar-validate")?;
    let input = scratch.write(&format!("{name}.orb"), &program.to_string())?;
    let prior_path = prior
        .map(|p| scratch.write("prior.orb", &p.to_string()))
        .transpose()?;
    let stamped_text =
        match stamped_json(&input, prior_path.as_deref(), None) {
            Ok(json) => json,
            Err(e) => return Ok(refused(issue("PROGRAM_STAMP_FAILED", e.to_string()))),
        };
    let stamped = scratch.write(&format!("{name}.stamped.orb"), &stamped_text)?;
    let report =
        crate::utils::validation::validation_report(&stamped, None, None, true, &[])
            .map_err(|e| failure(format!("{op}: {e}")))?;
    let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
    let (errors, warnings) = (issues(&report, "errors"), issues(&report, "warnings"));
    let valid = report
        .get("valid")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && errors.is_empty()
        && warnings.is_empty();
    Ok(Checked {
        valid,
        errors,
        warnings,
        stamped: Some(stamped_text),
    })
}

/// The issues of a validation report, as the `{ code, message, path?, line?, column? }` findings both hosts answer.
/// The shell's prebuilt web client copied to `<output>/web-client` with the program's runtime files (the client-mode
/// `packages/client/public`) laid over it: the folder `orb serve` serves.
fn copy_web_client(built: Option<&Path>, output: &Path) -> Result<PathBuf, SubstrateFailure> {
    let built = built.ok_or_else(|| {
        failure("program/eval: the typescript shell ships no prebuilt web client: run `orb update` (in a shell checkout, build it with `node tooling/scripts/build-web-client.mjs`)")
    })?;
    if !built.join("index.html").is_file() {
        return Err(failure(format!("program/eval: {} holds no index.html: it is not a built web client", built.display())));
    }
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let public = output.join("packages/client/public");
    if !public.join("client.json").is_file() {
        return Err(failure("program/eval: the client-mode generate wrote no client.json for the web client to read: generate with `mode: client`"));
    }
    let target = output.join("web-client");
    let copied = (if target.exists() { std::fs::remove_dir_all(&target) } else { Ok(()) })
        .and_then(|()| copy(built, &target))
        .and_then(|()| copy(&public, &target));
    copied.map_err(|e| failure(format!("program/eval: laying out the web client in {}: {e}", target.display())))?;
    Ok(target)
}

/// `webClient` is the copied prebuilt web client, null when the generate asked for none.
fn generate_answer(report: &orbital_compiler::mixed::CompileReport, web_client: Option<&Path>) -> serde_json::Value {
    serde_json::json!({
        "lines": report.lines,
        "files": report.files,
        "warnings": report.warnings,
        "nextSteps": report.next_steps,
        "webClient": web_client.map(|dir| dir.display().to_string()),
    })
}

/// The program at `path`, parsed and resolved the way a cascade runs it.
fn resolve_program(path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    resolve_parsed(parsed_text(&text, path)?, path)
}

fn resolve_parsed(schema: orbital_core::OrbitalSchema, path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    use orbital_compiler::{phases::lazy::resolve_program_with_path, CompilerOptions};
    let (resolved, _lazy) = resolve_program_with_path(schema, &CompilerOptions::from_env(), Some(path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", path.display(), e))?;
    Ok(resolved)
}

/// The program at `path` resolved with each step's `config` knob values written over its trait's defaults, as
/// [`run_cascade`] runs it.
fn resolve_with_step_config(path: &Path, steps: &[serde_json::Value]) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;
    let text = orbital_core::step_replay::with_step_config(&text, steps).map_err(|e| anyhow::anyhow!(e))?.unwrap_or(text);
    resolve_parsed(parsed_text(&text, path)?, path)
}

/// Replays `steps` on the program at `path` (`orb execute --steps`); `live` answers their os/* and http asks on the
/// native program host instead of leaving keyed ones pending (G-ORB-459).
/// `keep_states` keeps every trait's state and every executed trait's entity on each step, which only the raw report prints.
fn run_cascade(path: &Path, steps: &serde_json::Value, live: bool, cost: bool, keep_states: bool) -> anyhow::Result<orbital_core::step_replay::StepReport> {
    use orbital_core::runtime::{EntitySource, InMemoryEntityCache, RuntimeKernel};
    use orbital_core::step_replay::{play_steps, replay_dispatch, trait_definitions, with_step_config, StepAnswers, StepClock, StepCredentials};
    let steps = steps.as_array().ok_or_else(|| anyhow::anyhow!("--steps must be a JSON array"))?;
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;
    let text = with_step_config(&text, steps).map_err(|e| anyhow::anyhow!(e))?.unwrap_or(text);
    let program = parsed_text(&text, path)?;
    let resolved = resolve_parsed(program.clone(), path)?;
    let defs = trait_definitions(&resolved);
    let clock = StepClock::new();
    let live_host = if live {
        let files = crate::commands::dev::resolve_schema_to_temp(program, path)?;
        let config = orbital_shell_rust::host::HostConfig {
            substrate: Some(orbital_shell_rust::host::SubstratePort(std::sync::Arc::new(ProgramHost))),
            clock: Some(clock.dispatch_clock()),
            ..orbital_shell_rust::host::HostConfig::default()
        };
        let host = orbital_shell_rust::host::boot::boot_file_with(&files.path(), &std::env::vars().collect(), false, config)?;
        Some((host, files))
    } else {
        None
    };
    let replay_kernel;
    let kernel: &RuntimeKernel = match &live_host {
        Some((host, _files)) => host.kernel(),
        None => {
            replay_kernel = RuntimeKernel::new(
                orbital_core::RuntimeEnvironment::Server,
                std::sync::Arc::new(InMemoryEntityCache::new()) as std::sync::Arc<dyn EntitySource>,
                orbital_core::KernelConfig { clock: Some(clock.dispatch_clock()), ..Default::default() },
            );
            replay_kernel.register_schema(resolved).map_err(|e| anyhow::anyhow!("Failed to register {}: {e}", path.display()))?;
            &replay_kernel
        }
    };
    let credentials = StepCredentials::new();
    let answers = StepAnswers::new();
    if cost && live_host.is_none() {
        kernel.set_cost_meter(orbital_core::alloc_meter::native_meter());
    }
    if cost && kernel.cost_meter().is_none() {
        tracing::warn!(target: orbital_core::logging::ns::CLI, "program/step: cost was asked but this orb counts no allocations (no counting allocator), so no step carries a cost");
    }
    let _cost = cost.then(|| orbital_core::runtime::RuntimeKernel::cost_scope());
    play_steps(kernel, &clock, &credentials, &answers, &defs, steps, live, keep_states, &mut |trait_name, event, payload, settle_ms| match &live_host {
        Some((host, _files)) => {
            host.tap_completions();
            let played = host.dispatch(trait_name, orbital_core::RUNTIME_SINGLETON, event, payload).map_err(|e| e.to_string());
            settle_open_calls(host, settle_ms);
            let completions = host.take_completions();
            played.and_then(|dispatched| orbital_core::step_replay::with_completions(dispatched, completions))
        }
        None => replay_dispatch(kernel, &credentials, &answers, trait_name, event, payload),
    })
    .map_err(|refusal| anyhow::anyhow!(refusal.error))
}

/// A live step waits for its open keyed calls to end; with `settleMs`, one still open after it is cancelled.
fn settle_open_calls(host: &orbital_shell_rust::host::Host, settle_ms: Option<f64>) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    match settle_ms {
        Some(ms) => {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms.max(0.0) as u64));
                let _ = stop.send(());
            });
            orbital_shell_rust::host::run::hold(host, &stopped);
        }
        None => {
            orbital_shell_rust::host::run::hold(host, &stopped);
            drop(stop);
        }
    }
}

fn issues(report: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    report
        .get(key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

impl ProgramHost {
    /// `program/prove program { path? }`: every spec proposition (`@invariant`, `@holds`, `@ensures`) the program cannot be
    /// proved to keep, as `{ findings: [{ verdict, reason, counterexample?, site, proposition }] }`. `verdict` is
    /// `violated` (a counterexample the evaluator confirmed) or `unproved` (the prover could not decide, `reason` says why
    /// and `counterexample` is the possible one when it found one); a proved proposition is no finding. With `path` (absolute)
    /// the file is resolved first and the program argument is not read; otherwise the program argument is proved as given.
    fn prove(args: &[Value]) -> Result<Value, SubstrateFailure> {
        let op = "program/prove";
        let options = to_json(args.get(1).unwrap_or(&Value::Null));
        let given: orbital_core::OrbitalSchema;
        let entry;
        let schema: &orbital_core::OrbitalSchema = if options.is_null() || options.as_object().is_some_and(|o| !o.contains_key("path")) {
            let program = program_arg(op, args.first())?;
            if let Some(o) = options.as_object() {
                Options { op, map: o }.only("program", &[])?;
            }
            given = serde_json::from_value(program).map_err(|e| failure(format!("{op}: not a program: {e}")))?;
            &given
        } else {
            let options = Options::of(op, &options)?;
            options.only("path", &["path"])?;
            let path = options.absolute("path", "path")?;
            entry = super::dev::resolved_file(&path).map_err(|e| failure(format!("{op}: {e}")))?;
            entry.schema()
        };
        serialized(&orbital_core::proof::prove_report(schema))
    }

    /// `program/os-grants program { path?, final? }`: the static `os/` grant check of the lowered program as typed findings
    /// `{ findings: [{ code, orbital?, call?, entry?, site, grant?, app?, argument?, via? }] }` (`code` is `missing`, `unused`,
    /// `entry-invalid` or `target-not-static`). With `final` the finished program is checked, where a config-fed target is
    /// a literal; otherwise the program as written. With `path` (absolute) the file is resolved first.
    fn os_grants(args: &[Value]) -> Result<Value, SubstrateFailure> {
        let op = "program/os-grants";
        let options = to_json(args.get(1).unwrap_or(&Value::Null));
        let given: orbital_core::OrbitalSchema;
        let entry;
        let (schema, pass): (&orbital_core::OrbitalSchema, _) = if options.is_null() {
            let program = program_arg(op, args.first())?;
            given = serde_json::from_value(program).map_err(|e| failure(format!("{op}: not a program: {e}")))?;
            (&given, orbital_core::runtime::os_grants::OsGrantPass::Written)
        } else {
            let options = Options::of(op, &options)?;
            options.only("path", &["path", "final"])?;
            let pass = if options.flag("final")? { orbital_core::runtime::os_grants::OsGrantPass::Final } else { orbital_core::runtime::os_grants::OsGrantPass::Written };
            if options.map.contains_key("path") {
                let path = options.absolute("path", "path")?;
                entry = super::dev::resolved_file(&path).map_err(|e| failure(format!("{op}: {e}")))?;
                (entry.schema(), pass)
            } else {
                let program = program_arg(op, args.first())?;
                given = serde_json::from_value(program).map_err(|e| failure(format!("{op}: not a program: {e}")))?;
                (&given, pass)
            }
        };
        let findings = orbital_core::runtime::os_grants::os_grant_findings(schema, pass).map_err(|e| failure(format!("{op}: {}: {e}", e.code())))?;
        serialized(&orbital_core::runtime::os_grants::OsGrantsReport { findings })
    }

    /// `program/validate { report: true, path, final?, denyWarnings?, findings? }`: the report `orb validate --json` prints
    /// for the file at the absolute `path` (lowering, types, every validator finding with its source position); each of
    /// `findings` (a std lint's) joins the errors or warnings by its severity, positioned the same way.
    fn validation_report(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let op = "program/validate";
        let path = options
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::Path::new)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure(format!("{op}: `report` needs the program's absolute `path`")))?;
        let flag = |key: &str| match options.get(key) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(failure(format!("{op}: `{key}` must be a boolean"))),
        };
        for key in options.as_object().into_iter().flat_map(|o| o.keys()) {
            if !["report", "path", "final", "denyWarnings", "patterns", "services", "findings"].contains(&key.as_str()) {
                return Err(failure(format!("{op}: unsupported option \"{key}\" with `report`")));
            }
        }
        let file = |key: &str| match options.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(p)) => Ok(Some(std::path::PathBuf::from(p))),
            Some(_) => Err(failure(format!("{op}: `{key}` must be a path"))),
        };
        let (patterns, services) = (file("patterns")?, file("services")?);
        let findings: Vec<orbital_compiler::phases::validation::report::ProgramFinding> = match options.get("findings") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(list) => serde_json::from_value(list.clone())
                .map_err(|e| failure(format!("{op}: `findings` must be a list of {{ code, path, message, suggestion, severity: \"error\" | \"warning\" }}: {e}")))?,
        };
        if flag("final")? {
            return Err(failure(format!("{op}: `final` is retired from the compiler (its checks were archived with the validators); pass a std lint's results as `findings`")));
        }
        let report = crate::utils::validation::validation_report(
            path,
            patterns.as_deref(),
            services.as_deref(),
            flag("denyWarnings")?,
            &findings,
        )
            .map_err(|e| failure(format!("{op}: {e}")))?;
        serialized(&report)
    }

    /// `program/read { resolve: true, path, output? }`: what `orb resolve <path> [--output]` prints
    /// and writes, as `{ program, text, loadedResolved, programPath?, catalogs? }` (`loadedResolved`: the source carried the `resolved` stamp, EXEC-LOAD-1, and is answered as it was read, `output` unwritten; `programPath`: the source when it was resolved, `output` when written, absent in memory); the file at the absolute `path` is read, not `text`. A program that declares locales and has no `output` answers each locale's merged catalog in `catalogs` (locale to qualified key to message) and writes nothing.
    /// `text: false` without `output` answers `{ program, lazy: [{ orbital, program }] }` and writes nothing, so a program with lazy
    /// behaviors or locales is not refused (the lint input reads programs this way); with `output` it is the same read without `text` in the answer.
    fn read_resolved(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let path = options
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::Path::new)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| failure("program/read: `resolve` needs the program's absolute `path`"))?;
        let output = match options.get("output") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(out)) => Some(std::path::PathBuf::from(out)),
            Some(_) => return Err(failure("program/read: `output` must be a path")),
        };
        let with_text = match options.get("text") {
            None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(true)) => true,
            Some(serde_json::Value::Bool(false)) if output.is_none() => {
                return resolved_value(path)
                    .map_err(|e| failure(format!("program/read: {e}")));
            }
            Some(serde_json::Value::Bool(false)) => false,
            Some(_) => return Err(failure("program/read: `text` must be a boolean")),
        };
        let read = resolved_read(path, output.as_deref())
            .map_err(|e| failure(format!("program/read: {e}")))?;
        let program: Value = serde_json::from_str(&read.text).map_err(|e| failure(e.to_string()))?;
        let mut fields = orbital_core::ValueMap::default();
        fields.insert("program".into(), program);
        fields.insert("loadedResolved".into(), Value::Bool(read.loaded_resolved));
        if let Some(program_path) = &read.program_path {
            fields.insert("programPath".into(), Value::String(program_path.display().to_string().into()));
        }
        if with_text {
            fields.insert("text".into(), Value::String(read.text.into()));
        }
        let answer = Value::object(fields);
        release_transients();
        Ok(answer)
    }

    /// `program/read { stamp: true, path, prior?, imports?, output? }`: what `orb stamp` prints, as `{ program, text }`;
    /// with `output` the stamped text is written to that file (as `--output`) and printed by nobody.
    fn read_stamped(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let options = Options::of("program/read", options)?;
        options.only("stamp", &["stamp", "path", "prior", "imports", "output"])?;
        let path = options.absolute("path", "stamp")?;
        let prior = options.path("prior")?;
        let imports = options.path("imports")?;
        let output = options.path("output")?;
        let text = stamped_json(&path, prior.as_deref(), imports.as_deref())
            .map_err(|e| failure(format!("program/read: {e}")))?;
        if let Some(out) = &output {
            std::fs::write(out, &text)
                .map_err(|e| failure(format!("program/read: Failed to write {}: {e}", out.display())))?;
        }
        let program: serde_json::Value = serde_json::from_str(&text).map_err(|e| failure(e.to_string()))?;
        from_json(serde_json::json!({ "program": program, "text": text }))
    }

    /// `program/read { references: true, path }`: `{ program, files, references }`, the program at the absolute `path` as
    /// it reads before resolution, each imported file's orbitals once under its key (the file's path relative to the
    /// program's directory, `#2`, `#3`, ... for a file imported again under differing `config`), and each reference as
    /// `{ kind, alias, name, path, callSite, declaration }`: the reference as written, and its declaration addressed as
    /// `{ file, index }` (an orbital), `{ file, orbital, trait }` (a trait of that file's orbital) or, for a `uses`, the
    /// import's app-level config.
    fn read_references(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let options = Options::of("program/read", options)?;
        options.only("references", &["references", "path"])?;
        let path = options.absolute("path", "references")?;
        let (program, answer) = referenced_program(&path)
            .map_err(|e| failure(format!("program/read: {e}")))?;
        let files = orbital_core::value_serialize::to_value(&answer.files).map_err(|e| failure(e.to_string()))?;
        let references = orbital_core::value_serialize::to_value(&answer.references).map_err(|e| failure(e.to_string()))?;
        Ok(Value::object(orbital_core::ValueMap::from_iter([
            ("program".into(), program),
            ("files".into(), files),
            ("references".into(), references),
        ])))
    }

    /// `program/print { target, path, section?, format?, only?, map?, output?, inspectSource?, documentVersion?, unit?,
    /// out?, signKey?, keyId? }`: what `orb emit <target> <path>` prints, as `{ text, stderr }`; the files `orb emit`
    /// writes (`output`, the source map, `out`) are written.
    fn print_target(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        let options = Options::of("program/print", options)?;
        options.only(
            "target",
            &[
                "target", "path", "section", "format", "only", "map", "output", "inspectSource", "documentVersion",
                "unit", "out", "signKey", "keyId",
            ],
        )?;
        let target: PrintTarget = options
            .choice("target")?
            .ok_or_else(|| failure("program/print: `target` must be orb, lolo, ts, py or manifest"))?;
        let path = options.absolute("path", "target")?;
        let (sign_key, key_id) = (options.path("signKey")?, options.text("keyId")?);
        if sign_key.is_some() != key_id.is_some() {
            return Err(failure("program/print: `signKey` and `keyId` go together"));
        }
        let inspect_source = options.path("inspectSource")?;
        let document_version = options.number("documentVersion")?;
        if document_version.is_some() && inspect_source.is_none() {
            return Err(failure("program/print: `documentVersion` needs `inspectSource`"));
        }
        let request = PrintRequest {
            target,
            path,
            section: options.text("section")?,
            output: options.path("output")?,
            format: options.choice::<PrintFormat>("format")?,
            only: options.text("only")?,
            map: options.flag("map")?,
            unit: options.text("unit")?,
            out: options.path("out")?,
            signer: sign_key.zip(key_id),
        };
        let mut printed = Printed::default();
        match inspect_source {
            Some(original) => {
                if target != PrintTarget::Orb {
                    return Err(failure("program/print: --inspect-source requires the orb target"));
                }
                let source = std::fs::read_to_string(&request.path).map_err(|e| failure(format!("program/print: Failed to read {}: {e}", request.path.display())))?;
                let envelope = super::program_inspect::inspect(&source, &serde_json::json!({ "path": original, "version": document_version }))?;
                printed.write(&envelope.to_string(), request.output.as_deref()).map_err(|e| failure(format!("program/print: {e}")))?;
            }
            None => request.print(&mut printed).map_err(|e| failure(format!("program/print: {e}")))?,
        }
        from_json(serde_json::json!({ "text": printed.stdout, "stderr": printed.stderr }))
    }

    /// `program/eval { generate: { path, output, shell, mode?, frontend?, backend?, backendUrl?, renderer?, patterns?, devLogs?,
    /// host?, listenHost?, port?, emitOrb?, webClient?, source? } }`: what `orb compile` does and prints, as `{ lines, files,
    /// warnings, nextSteps, webClient }`; `webClient: true` also copies the shell's prebuilt web client into the output. The program is validated before anything is generated, as the command does.
    fn eval_generate(options: &serde_json::Value) -> Result<Value, SubstrateFailure> {
        use crate::commands::program_compile::{compile_run, CompileRequest};
        let op = "program/eval";
        Options::of(op, options)?.only("generate", &["generate"])?;
        let generate = options.get("generate").unwrap_or(&serde_json::Value::Null);
        let g = Options::of(op, generate)?;
        g.only(
            "generate",
            &[
                "path", "output", "shell", "mode", "frontend", "backend", "backendUrl", "renderer", "patterns", "devLogs", "host",
                "emitOrb", "webClient", "source",
            ],
        )?;
        let web_client = g.flag("webClient")?;
        let source = match g.path("source")? {
            Some(source) if !source.is_absolute() => {
                return Err(failure(format!("{op}: `generate` needs an absolute `source`, not {}", source.display())))
            }
            source => source,
        };
        let path = g.absolute("path", "generate")?;
        let output = g.absolute("output", "generate")?;
        let shell: crate::ShellType = g
            .choice("shell")?
            .ok_or_else(|| failure(format!("{op}: `generate` needs the `shell` to generate for")))?;
        let (patterns, emit_orb) = (g.path("patterns")?, g.path("emitOrb")?);
        let report = compile_run(CompileRequest {
            path: &path,
            output: &output,
            shell,
            mode: g.choice("mode")?.unwrap_or_default(),
            frontend: g.choice("frontend")?,
            backend: g.choice("backend")?,
            backend_url: g.text("backendUrl")?,
            renderer: g.choice("renderer")?,
            patterns_path: patterns.as_deref(),
            dev_logs: g.flag("devLogs")?,
            host: g.choice("host")?.unwrap_or_default(),
            emit_orb: emit_orb.as_deref(),
            source: source.as_deref(),
        })
        .map_err(|e| failure(format!("{op}: {e}")))?;
        let web_client = if web_client {
            let shell_program = crate::utils::shell_programs::locate(shell.program()).map_err(|e| failure(format!("{op}: {e}")))?;
            Some(copy_web_client(shell_program.description.web_client.as_deref(), &output)?)
        } else {
            None
        };
        from_json(generate_answer(&report, web_client.as_deref()))
    }

    fn read(&self, text: &str) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let path = scratch.write("program.lolo", text)?;
        match orb_value(text, &path.display().to_string()) {
            Ok(json) => from_json(json),
            Err(lowering) => {
                let report =
                    crate::utils::validation::validation_report(&path, None, None, true, &[])
                        .map_err(|e| failure(format!("program/read: {e}")))?;
                let report = serde_json::to_value(&report).map_err(|e| failure(e.to_string()))?;
                let found: Vec<serde_json::Value> = issues(&report, "errors")
                    .into_iter()
                    .chain(issues(&report, "warnings"))
                    .collect();
                if found.is_empty() {
                    return Err(failure(format!("program/read: {lowering}")));
                }
                let messages: Vec<&str> = found
                    .iter()
                    .filter_map(|i| i.get("message").and_then(serde_json::Value::as_str))
                    .collect();
                Err(SubstrateFailure {
                    message: format!(
                        "program/read: {} validation issue(s): {}",
                        found.len(),
                        messages.join("; ")
                    ),
                    errors: found.into_iter().map(from_json).collect::<Result<_, _>>()?,
                })
            }
        }
    }

    fn print(&self, program: serde_json::Value) -> Result<Value, SubstrateFailure> {
        let scratch = Scratch::new("almadar-program")?;
        let text = program.to_string();
        let path = scratch.write("program.orb", &text)?;
        lolo_text(&text, &path.display().to_string())
            .map(Value::string)
            .map_err(|e| failure(format!("program/print: {e}")))
    }

    fn validate(
        &self,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let mut final_program = false;
        let mut prior: Option<serde_json::Value> = None;
        match options.map(to_json) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("final", serde_json::Value::Bool(b)) => final_program = b,
                        ("final", _) => {
                            return Err(failure("program/validate: final must be a boolean"))
                        }
                        ("prior", p) => {
                            prior = Some(program_arg(
                                "program/validate prior",
                                from_json(p).ok().as_ref(),
                            )?)
                        }
                        (other, _) => {
                            return Err(failure(format!(
                                "program/validate: unsupported option \"{other}\""
                            )))
                        }
                    }
                }
            }
            Some(_) => return Err(failure("program/validate: options must be an object")),
        }
        let checked =
            stamp_and_validate("program/validate", &program, prior.as_ref(), final_program)?;
        from_json(
            serde_json::json!({ "valid": checked.valid, "errors": checked.errors, "warnings": checked.warnings }),
        )
    }

    fn describe_target(
        op: &str,
        value: Option<&Value>,
    ) -> Result<DescribeTarget, SubstrateFailure> {
        use DescribeTarget;
        let json = value.map(to_json).unwrap_or(serde_json::Value::Null);
        let behavior = json
            .get("behavior")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match (
            behavior,
            json.get("trait"),
            json.get("orbital").and_then(serde_json::Value::as_str),
        ) {
            (Some(_), Some(_), _) => Ok(DescribeTarget::Value(json)),
            (Some(behavior), None, Some(orbital)) => Ok(DescribeTarget::Spec(format!(
                "{behavior}.orbitals.{orbital}"
            ))),
            (Some(behavior), None, None) => Ok(DescribeTarget::Spec(behavior)),
            (None, _, _) => Err(failure(format!(
                "{op}: expected {{ behavior }}, an orbital value or a trait value"
            ))),
        }
    }

    fn behavior_of(op: &str, value: Option<&Value>) -> Result<String, SubstrateFailure> {
        value
            .map(to_json)
            .and_then(|v| {
                v.get("behavior")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| failure(format!("{op}: expected a value naming its behavior")))
    }

    /// `behavior/catalog`: the listing as rows, each with no vectors (`vectors: {}`, `model: ""`): the shipped vectors and the
    /// topics view are std's (`std-orb-behaviors`, `std-orb-vectors`). `texts: true` adds the facts the
    /// embedding texts read: `name`, `rank` and `textOrder` (the package's first-mention place and the row's path order), `defaultEntityName`, `connectableEvents` and the raw `capabilitiesText`.
    fn catalog(scope: Option<&Value>, options: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let json = scope.map(to_json).unwrap_or(serde_json::Value::Null);
        let strings = |key: &str| -> Option<Vec<String>> {
            json.get(key).and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        };
        let Some(paths) = strings("paths") else {
            return Err(failure("behavior/catalog: paths: expected a list of registry folders"));
        };
        let exposure = strings("exposure").unwrap_or_default();
        let with_traits = option_flag(options, "traits") != Some(false);
        let with_texts = option_flag(options, "texts") == Some(true);
        let started = std::time::Instant::now();
        // A plain listing needs only path and header facts; traits and the embedding texts need the lowered program.
        let depth = if with_traits || with_texts { reflect::RowDepth::Program } else { reflect::RowDepth::Header };
        let rows = reflect::catalog_rows(&paths, &exposure, depth, &mut |prefix, name, error| {
            tracing::warn!(target: orbital_core::logging::ns::CLI, "skipped {prefix}/{name}: {error}");
        })
        .map_err(|e| failure(format!("behavior/catalog: {e}")))?;
        let entries: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| {
                let key = format!("{}/{}", r.prefix, r.row.name);
                let mut entry = serde_json::json!({
                    "behavior": key,
                    "package": r.package,
                    "topic": r.row.topic,
                    "level": r.row.level,
                    "description": r.row.description,
                    "capabilities": r.row.capabilities.as_deref().into_iter().flat_map(|s| s.split(',')).map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>(),
                    "value": { "behavior": key },
                    "vectors": {},
                    "model": "",
                });
                if let Some(e) = r.row.exposure {
                    entry["exposure"] = e.into();
                }
                if let Some(p) = r.row.pattern {
                    entry["pattern"] = p.into();
                }
                if with_traits {
                    entry["traits"] = r.row.traits.iter().map(|t| serde_json::json!({ "behavior": key, "trait": t })).collect();
                }
                if with_texts {
                    entry["name"] = r.row.name.into();
                    entry["rank"] = r.rank.into();
                    entry["textOrder"] = (r.rank * 1_000_000 + r.order).into();
                    entry["defaultEntityName"] = r.row.default_entity_name.into();
                    entry["connectableEvents"] = r.row.connectable_events.into();
                    if let Some(c) = r.row.capabilities {
                        entry["capabilitiesText"] = c.into();
                    }
                }
                entry
            })
            .collect();
        tracing::debug!(target: orbital_core::logging::ns::CLI, total_ms = started.elapsed().as_millis() as u64, "behavior/catalog answered");
        from_json(serde_json::Value::Array(entries))
    }

    /// `behavior/describe`: a description without vectors; the knob vectors and the forwarded controls are std's to attach.
    fn describe(op: &str, value: Option<&Value>) -> Result<Value, SubstrateFailure> {
        let described = reflect::describe(Self::describe_target(op, value)?, None).map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(described)
    }

    /// `behavior/packages`: each installed behavior package's `{ root, prefix, vectors, vectorsRel, vectorsIn }`,
    /// `vectors` being the `orb.vectors` snapshot the lock resolves (the cached sidecar when locked), `null` when none
    /// is declared. `vectorsIn` is a fixed anchor a program spells as a literal and `vectorsRel` the snapshot relative
    /// to it: `"node-modules"` (`<cwd>/node_modules`, rel starting at the package name), `"sidecars"` (the default
    /// `~/.almadar/sidecars`, rel the cached name) or `"outside"` (anything else, rel `null`).
    fn packages() -> Result<Value, SubstrateFailure> {
        let cwd = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        let packages = discover_packages(&cwd)
            .map_err(|e| failure(format!("behavior/packages: {e}")))?;
        let mut out = Vec::new();
        for pkg in &packages {
            let vectors = resolve_vectors(
                pkg,
                &UreqFetcher,
            )
            .map_err(|e| failure(format!("behavior/packages: {e}")))?;
            let place = vectors.as_deref().map(|path| vectors_place(path, &cwd, &orbital_core::behavior_packages::sidecar_cache_dir()));
            out.push(serde_json::json!({
                "root": pkg.root.to_string_lossy(),
                "prefix": pkg.prefix,
                "vectors": vectors.as_ref().map(|v| v.to_string_lossy().into_owned()),
                "vectorsRel": place.as_ref().and_then(|p| p.rel.clone()),
                "vectorsIn": place.as_ref().map(|p| p.kind),
            }));
        }
        from_json(serde_json::Value::Array(out))
    }

    /// `behavior/source`: the behavior as a program; with `options.file: true`, its source file as `{ path, source }`
    /// (the file text), what `orb mcp`'s `read_behavior` answers.
    fn source(
        op: &str,
        value: Option<&Value>,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let behavior = Self::behavior_of(op, value)?;
        if option_flag(options, "file") == Some(true) {
            let path = reflect::behavior_source_path(&behavior)
                .map_err(|e| failure(format!("{op}: {e}")))?;
            let source = std::fs::read_to_string(&path)
                .map_err(|e| failure(format!("{op}: {}: {e}", path.display())))?;
            return from_json(serde_json::json!({ "path": path, "source": source }));
        }
        let source = reflect::source(&behavior)
            .map_err(|e| failure(format!("{op}: {e}")))?;
        from_json(source)
    }

    fn resolve_behavior(&self, behavior: &str) -> Result<Value, SubstrateFailure> {
        let path = reflect::behavior_source_path(behavior)
            .map_err(|e| failure(format!("behavior/resolve: {e}")))?;
        let entry = crate::commands::dev::resolved_file(&path)
            .map_err(|e| failure(format!("behavior/resolve: {e}")))?;
        let mut program = serde_json::to_value(entry.schema()).map_err(|e| failure(e.to_string()))?;
        with_client_metadata(&mut program);
        from_json(program)
    }
}

/// A resolved program is mounted by a browser kernel, which has no installed core: it carries the client metadata
/// (`orbital_core::effects::runs_on::client_program_metadata`) beside whatever `_metadata` it already has.
fn with_client_metadata(program: &mut serde_json::Value) {
    let metadata = orbital_core::effects::runs_on::client_program_metadata(program.get("orbitals").unwrap_or(&serde_json::Value::Null));
    let Some(root) = program.as_object_mut() else {
        return;
    };
    let slot = root.entry("_metadata").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(existing) = slot.as_object_mut() {
        existing.extend(metadata);
    }
}

/// `importsOf`: each `uses` of the program's orbitals as `(behavior, specifier)`, once per behavior, the behavior
/// being the specifier's trailing name (the loader's rule).
fn imported_behaviors(program: &serde_json::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for orbital in program
        .get("orbitals")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        for u in orbital
            .get("uses")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(from) = u.get("from").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let behavior = from.rsplit('/').next().unwrap_or(from).to_string();
            if !out.iter().any(|(b, _)| *b == behavior) {
                out.push((behavior, from.to_string()));
            }
        }
    }
    out
}

fn behavior_names(imports: &[(String, String)]) -> Vec<String> {
    imports.iter().map(|(b, _)| b.clone()).collect()
}

impl ProgramHost {
    /// `behavior/dependencies`: each orbital's direct imports, then every behavior they reach, once, with its layer
    /// (`primitive` for a UI pattern factory, else its package's tier) and its own imports. Each import is resolved
    /// by the loader the program's `uses` go through, so only the behaviors reached are read.
    fn dependencies(&self, program: serde_json::Value) -> Result<Value, SubstrateFailure> {
        let op = "behavior/dependencies";
        let cwd = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        let packages = discover_packages(&cwd)
            .map_err(|e| failure(format!("{op}: {e}")))?;
        let mut loader = orbital_compiler::loader::ExternalOrbitalLoader::new(
            orbital_compiler::loader::LoaderConfig {
                base_path: cwd,
                packages: packages.clone(),
                ..Default::default()
            },
        );
        let mut orbitals = serde_json::Map::new();
        let mut queue: std::collections::VecDeque<(String, String, Option<PathBuf>)> =
            std::collections::VecDeque::new();
        for orbital in program
            .get("orbitals")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = orbital
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let own = imported_behaviors(&serde_json::json!({ "orbitals": [orbital] }));
            queue.extend(own.iter().map(|(b, spec)| (b.clone(), spec.clone(), None)));
            orbitals.insert(name, serde_json::json!(behavior_names(&own)));
        }
        let mut behaviors = serde_json::Map::new();
        while let Some((name, spec, from)) = queue.pop_front() {
            if behaviors.contains_key(&name) {
                continue;
            }
            let imported = reflect::resolve_import(
                &mut loader,
                &packages,
                &spec,
                from.as_deref(),
            )
            .map_err(|e| failure(format!("{op}: {e}")))?;
            let (layer, imports) = match imported {
                Some(b) => {
                    let layer = match (&b.pattern, b.package.as_ref().map(|p| p.name.as_str())) {
                        (Some(_), _) => "primitive",
                        (None, Some("@almadar/std")) => "std",
                        (None, Some("@almadar-io/behaviors")) => "io",
                        (None, _) => "unknown",
                    };
                    let imports = imported_behaviors(&b.program);
                    queue.extend(imports.iter().map(|(n, s)| (n.clone(), s.clone(), Some(b.path.clone()))));
                    (layer, behavior_names(&imports))
                }
                None => ("unknown", Vec::new()),
            };
            behaviors.insert(
                name,
                serde_json::json!({ "layer": layer, "imports": imports }),
            );
        }
        from_json(serde_json::json!({ "orbitals": orbitals, "behaviors": behaviors }))
    }

    /// `program/eval`: the program stamped and validated at the 0/0 bar, then written into the project's
    /// `orbitals/` folder; the answer is the written behavior and its traits as values.
    fn eval(
        &self,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let project = std::env::current_dir().map_err(|e| failure(e.to_string()))?;
        self.eval_in(&project, program, options)
    }

    fn eval_in(
        &self,
        project: &std::path::Path,
        program: serde_json::Value,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let (mut prior, mut final_program) = (None, false);
        match options.map(to_json) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("final", serde_json::Value::Bool(b)) => final_program = b,
                        ("final", _) => return Err(failure("program/eval: final must be a boolean")),
                        ("prior", p) => prior = Some(program_arg("program/eval prior", from_json(p).ok().as_ref())?),
                        ("workspace", serde_json::Value::String(id)) if !id.is_empty() => {
                            return Err(SubstrateFailure {
                                message: "program/eval: 1 validation issue(s): no workspace host is installed on this host".to_string(),
                                errors: vec![from_json(serde_json::json!({ "code": "PROGRAM_NO_WORKSPACE_HOST", "message": "no workspace host is installed on this host" }))?],
                            })
                        }
                        ("workspace", _) => return Err(failure("program/eval: workspace must be a non-empty id")),
                        (other, _) => return Err(failure(format!("program/eval: unsupported option \"{other}\""))),
                    }
                }
            }
            Some(_) => return Err(failure("program/eval: options must be an object")),
        }
        let checked = stamp_and_validate("program/eval", &program, prior.as_ref(), final_program)?;
        let issues: Vec<serde_json::Value> = checked
            .errors
            .iter()
            .chain(checked.warnings.iter())
            .cloned()
            .collect();
        let stamped = match checked.stamped {
            Some(text) if checked.valid && issues.is_empty() => text,
            _ => {
                let messages: Vec<String> = issues
                    .iter()
                    .filter_map(|i| {
                        i.get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect();
                return Err(SubstrateFailure {
                    message: format!(
                        "program/eval: {} validation issue(s): {}",
                        issues.len(),
                        messages.join("; ")
                    ),
                    errors: issues
                        .into_iter()
                        .map(from_json)
                        .collect::<Result<_, _>>()?,
                });
            }
        };
        let written: serde_json::Value = serde_json::from_str(&stamped)
            .map_err(|e| failure(format!("program/eval: stamped program does not parse: {e}")))?;
        let name = written
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target = project.join("orbitals");
        std::fs::create_dir_all(&target).map_err(|e| failure(e.to_string()))?;
        std::fs::write(
            target.join(format!("{name}.orb")),
            serde_json::to_string_pretty(&written).map_err(|e| failure(e.to_string()))?,
        )
        .map_err(|e| failure(e.to_string()))?;
        let behavior = format!("./orbitals/{name}");
        let traits: Vec<serde_json::Value> = written
            .get("orbitals")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|o| {
                o.get("traits")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|t| {
                t.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(|n| serde_json::json!({ "behavior": behavior, "trait": n }))
            })
            .collect();
        from_json(
            serde_json::json!({ "behavior": behavior, "traits": traits, "value": { "behavior": behavior } }),
        )
    }

    /// `program/step`: each request `{orbital, trait, from, event, payload}` played against the program in one
    /// kernel run, from its own `from` state; each answered as asked with the circuit it ran. `options.render`
    /// adds each request's rendered slots, walked by the render probe against the desktop renderer.
    fn step(
        &self,
        program: serde_json::Value,
        requests: Option<&Value>,
        options: Option<&Value>,
    ) -> Result<Value, SubstrateFailure> {
        let op = "program/step";
        let requests = match requests.map(to_json) {
            Some(serde_json::Value::Array(requests)) => requests,
            _ => return Err(failure(format!("{op}: expected a list of requests"))),
        };
        let raw = match options.map(to_json).as_ref().and_then(|o| o.get("report")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::String(r)) if r == "raw" => true,
            Some(other) => return Err(failure(format!("{op}: options.report must be \"raw\", not {other}"))),
        };
        // A raw request is an `orb execute --steps` step: the kernel dispatch from the held state, no `from` to check.
        let required: &[&str] = if raw { &["trait", "event"] } else { &["orbital", "trait", "from", "event"] };
        for r in &requests {
            for key in required.iter().copied() {
                if !r.get(key).is_some_and(serde_json::Value::is_string) {
                    return Err(failure(format!("{op}: each request needs a {key}")));
                }
            }
            if r.get("entity").is_some_and(|e| !e.is_object()) {
                return Err(failure(format!(
                    "{op}: a request's entity must be an object of fields"
                )));
            }
            if r.get("viewer").is_some_and(|v| !v.is_null()) {
                return Err(failure(format!(
                    "{op}: a request's viewer is retired; a step names `identity`, the session's bound identity"
                )));
            }
            if r.get("identity")
                .is_some_and(|v| !v.is_object() && !v.is_null())
            {
                return Err(failure(format!(
                    "{op}: a request's identity must be an identity object or null"
                )));
            }
            if r.get("settleMs").is_some_and(|n| !n.is_number()) {
                return Err(failure(format!(
                    "{op}: a request's settleMs must be a number of milliseconds"
                )));
            }
            if r.get("config").is_some_and(|c| !c.is_object()) {
                return Err(failure(format!(
                    "{op}: a request's config must be an object of knob values"
                )));
            }
            if r.get("credentials").is_some_and(|c| !c.is_object()) {
                return Err(failure(format!(
                    "{op}: a request's credentials must be an object of credential values"
                )));
            }
            if r.get("now").is_some_and(|n| !n.is_number()) {
                return Err(failure(format!(
                    "{op}: a request's now must be a number (ms since the epoch)"
                )));
            }            if r.get("answers").is_some_and(|a| !a.is_array()) {
                return Err(failure(format!(
                    "{op}: a request's answers must be a list of host answers"
                )));
            }
        }
        let step_of = |r: &mut serde_json::Map<String, serde_json::Value>, take: bool| {
            let mut field = |key: &str| if take { r.remove(key) } else { r.get(key).cloned() };
            let mut step = serde_json::json!({
                "trait": field("trait").unwrap_or(serde_json::Value::Null),
                "event": field("event").unwrap_or(serde_json::Value::Null),
                "payload": field("payload").unwrap_or(serde_json::json!({})),
            });
            for key in ["from", "entity", "config", "now", "settleMs", "identity", "session", "credentials", "answers"] {
                if let Some(v) = field(key) {
                    step[key] = v;
                }
            }
            step
        };
        // A raw run never reads its requests again, so their fields move into the steps instead of being copied.
        let mut requests = requests;
        let steps = serde_json::Value::Array(
            requests
                .iter_mut()
                .map(|r| match r.as_object_mut() {
                    Some(fields) => step_of(fields, raw),
                    None => serde_json::Value::Null,
                })
                .collect(),
        );
        let scratch = Scratch::new("almadar-step")?;
        let path = scratch.write("program.orb", &program.to_string())?;
        let live = match options.map(to_json).as_ref().and_then(|o| o.get("live")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.live must be a boolean, not {other}"
                )))
            }
        };
        let cost = match options.map(to_json).as_ref().and_then(|o| o.get("cost")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.cost must be a boolean, not {other}"
                )))
            }
        };
        let report = run_cascade(&path, &steps, live, cost, raw)
        .map_err(|e| failure(format!("{op}: {e}")))?;
        if raw {
            return from_json(serde_json::to_value(report).map_err(|e| failure(format!("{op}: {e}")))?);
        }
        let render = match options.map(to_json).as_ref().and_then(|o| o.get("render")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.render must be a boolean, not {other}"
                )))
            }
        };
        let render_props = match options.map(to_json).as_ref().and_then(|o| o.get("renderProps")) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(failure(format!(
                    "{op}: options.renderProps must be a boolean, not {other}"
                )))
            }
        };
        if render_props && !render {
            return Err(failure(format!(
                "{op}: options.renderProps adds props to the render facts, so it needs options.render: true"
            )));
        }
        let renderer = if render {
            let schema = resolve_with_step_config(&path, steps.as_array().map(Vec::as_slice).unwrap_or_default())
                .map_err(|e| failure(format!("{op}: {e}")))?;
            let target = crate::utils::desktop::target()
                .map_err(|e| failure(format!("{op}: options.render: {e}")))?;
            Some((schema, target))
        } else {
            None
        };
        let typed: Vec<orbital_core::program_machine::PlayRequest> =
            serde_json::from_value(serde_json::Value::Array(requests))
                .map_err(|e| failure(format!("{op}: {e}")))?;
        let mut played = orbital_core::step_replay::played_steps(&typed, &report)
            .map_err(|r| failure(format!("{op}: {}", r.error)))?;
        if let Some((schema, target)) = &renderer {
            for step in &mut played {
                let slots = orbital_core::client::render_probe::played_render_facts(
                    schema,
                    &step.trait_name,
                    &step.from,
                    step.seeds.entity.as_ref(),
                    &step.event,
                    step.payload.clone(),
                    target,
                    render_props,
                    |s, _| orbital_core::client::render_probe::probe_kernel(s),
                )
                .map_err(|e| failure(format!("{op}: render: {e}")))?;
                step.result.rendered = Some(slots);
            }
        }
        serialized(&played)
    }
}

const LAYOUT_STRATEGIES: [&str; 5] = ["sidebar", "tabs", "dashboard", "wizard-flow", "single"];

/// The option keys `program/compose` runs; the operator table declares the same set.
const COMPOSE_OPTION_KEYS: [&str; 4] = ["appName", "layout", "theme", "overrides"];

impl ProgramHost {
    /// `program/compose`: the programs, in order, as one app program (`orbital_compiler::composition::compose_programs`).
    fn compose(&self, args: &[Value]) -> Result<Value, SubstrateFailure> {
        let op = "program/compose";
        let Some(Value::Array(list)) = args.first() else {
            return Err(failure("program/compose: expected a list of programs"));
        };
        let programs = list
            .iter()
            .map(|p| program_value_arg(op, Some(p)))
            .collect::<Result<Vec<_>, _>>()?;
        let options = args
            .get(1)
            .map(to_json)
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| failure("program/compose: options must be an object with an appName"))?;
        let (mut app_name, mut layout, mut theme, mut overrides) = (None, None, None, None);
        for (key, value) in options.as_object().into_iter().flatten() {
            if !COMPOSE_OPTION_KEYS.contains(&key.as_str()) {
                return Err(failure(format!("program/compose: unsupported option \"{key}\"")));
            }
            match (key.as_str(), value.as_str()) {
                ("overrides", _) => overrides = Some(from_json(value.clone())?),
                ("appName", Some(v)) if !v.is_empty() => app_name = Some(v.to_string()),
                ("theme", Some(v)) => theme = (!v.is_empty()).then(|| v.to_string()),
                ("layout", v) => match v.filter(|l| LAYOUT_STRATEGIES.contains(l)) {
                    Some(l) => layout = Some(l.to_string()),
                    None => {
                        return Err(failure(format!(
                            "program/compose: layout must be one of {}",
                            LAYOUT_STRATEGIES.join(", ")
                        )))
                    }
                },
                _ => {
                    return Err(failure(format!(
                        "program/compose: option \"{key}\" is not a string"
                    )))
                }
            }
        }
        let app_name = app_name
            .ok_or_else(|| failure("program/compose: options must name the app (appName)"))?;
        let now = orbital_core::runtime::mock_random::iso_from_ms(
            orbital_core::time_util::now_millis() as f64,
        );
        let load = |from: &str| {
            reflect::source_value_of(from)
                .map_err(|e| format!("behavior/source: {e}"))
        };
        let schema = orbital_compiler::composition::compose_programs(
            programs,
            &app_name,
            layout.as_deref(),
            theme.as_deref(),
            overrides.as_ref(),
            &now,
            &load,
        )
        .map_err(|e| failure(format!("{op}: {e}")))?;
        release_transients();
        Ok(schema)
    }
}

impl SubstrateHandler for ProgramHost {
    fn run(&self, op: &str, args: &[Value]) -> Result<Value, SubstrateFailure> {
        match op {
            "program/read" => match args.first() {
                Some(Value::String(_)) if read_modes(args.get(1)).len() > 1 => Err(failure(format!(
                    "program/read: unsupported option \"{}\" with `{}`: one of inspect, embeds, resolve, stamp, references, findings per call",
                    read_modes(args.get(1))[1],
                    read_modes(args.get(1))[0]
                ))),
                Some(Value::String(text)) if option_flag(args.get(1), "inspect") == Some(true) => {
                    let options = to_json(&args[1]);
                    super::program_inspect::inspect(text, &options).and_then(from_json)
                }
                Some(Value::String(text)) if option_flag(args.get(1), "embeds") == Some(true) => {
                    let embeds = orbital_lolo::embeds::inline_embeds(text)
                        .map_err(|e| failure(format!("program/read: {e}")))?;
                    serialized(&embeds)
                }
                Some(Value::String(text)) if option_flag(args.get(1), "findings") == Some(true) => {
                    let found: Vec<serde_json::Value> = crate::utils::validation::source_findings(text)
                        .into_iter()
                        .map(|f| {
                            serde_json::json!({
                                "code": f.code,
                                "path": f.path,
                                "message": f.message,
                                "suggestion": f.suggestion.unwrap_or_default(),
                                "severity": "warning",
                            })
                        })
                        .collect();
                    from_json(serde_json::Value::Array(found))
                }
                Some(Value::String(_)) if option_flag(args.get(1), "resolve") == Some(true) => {
                    Self::read_resolved(&to_json(&args[1]))
                }
                Some(Value::String(_)) if option_flag(args.get(1), "references") == Some(true) => {
                    Self::read_references(&to_json(&args[1]))
                }
                Some(Value::String(_)) if option_flag(args.get(1), "stamp") == Some(true) => {
                    Self::read_stamped(&to_json(&args[1]))
                }
                Some(Value::String(text)) => self.read(text),
                _ => Err(failure("program/read: expected .lolo text")),
            },
            "program/print" if has_option(args.get(1), "target") => Self::print_target(&to_json(&args[1])),
            "program/print" => match PrintSource::of(args.get(1))? {
                Some(source) => source.print(),
                None => self.print(program_arg(op, args.first())?),
            },
            "program/prove" => Self::prove(args),
            "program/os-grants" => Self::os_grants(args),
            "program/validate" if option_flag(args.get(1), "report") == Some(true) => Self::validation_report(&to_json(&args[1])),
            "program/validate" => self.validate(program_arg(op, args.first())?, args.get(1)),
            "behavior/catalog" => Self::catalog(args.first(), args.get(1)),
            "behavior/describe" => Self::describe(op, args.first()),
            "behavior/packages" => Self::packages(),
            "behavior/source" => Self::source(op, args.first(), args.get(1)),
            "behavior/resolve" if option_flag(args.get(1), "expects") == Some(true) => {
                let spec = Self::behavior_of(op, args.first())?;
                let derived = reflect::expectations(&spec).map_err(|e| failure(format!("{op}: {e}")))?;
                from_json(derived)
            }
            "behavior/resolve" if option_flag(args.get(1), "path") == Some(true) => {
                let spec = Self::behavior_of(op, args.first())?;
                match which(&spec).map_err(|e| failure(format!("{op}: {e}")))? {
                    Ok((path, owner)) => from_json(serde_json::json!({ "path": path.display().to_string(), "owner": owner })),
                    Err(refusal) => Err(failure(format!("{op}: {refusal}"))),
                }
            }
            "behavior/resolve" => self.resolve_behavior(&Self::behavior_of(op, args.first())?),
            "program/compose" => self.compose(args),
            "program/eval" if has_option(args.get(1), "generate") => Self::eval_generate(&to_json(&args[1])),
            "program/eval" => self.eval(program_arg(op, args.first())?, args.get(1)),
            "program/step" => self.step(program_arg(op, args.first())?, args.get(1), args.get(2)),
            "behavior/dependencies" => self.dependencies(program_arg(op, args.first())?),
            other => Err(failure(format!(
                "{other}: not an op the native program host answers"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = r#"app note-pad [
  version: "1.0.0",
  description: "One note."
]

orbital NoteOrbital {
  entity Note [runtime] {
    id : string!
    text : string = ""
  }

  trait NoteView -> Note {
    initial: idle

    state idle {
      INIT -> idle
        (set @entity.text "hello")
        (render-ui main { type: "typography", content: @entity.text })
    }
  }

  page "/" -> NoteView
}
"#;

    /// The canonical `.orb` text of `source`: the bytes `orb emit orb --output <file>` writes, from the live `orb_value` path.
    fn orb_text(source: &str, source_path: &str) -> String {
        let value = orb_value(source, source_path).unwrap();
        serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value)).unwrap()
    }

    fn json(value: &Value) -> serde_json::Value {
        to_json(value)
    }

    #[test]
    fn read_keeps_a_record_literals_author_key_order() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let probe = r#"app order-probe [ version: "1.0.0" ]
orbital O {
  entity E [runtime] {
    id : string!
  }
  trait T -> E {
    initial: a
    state a {
      INIT -> a
        (log (json/stringify { zeta: 1, alpha: 2, mid: 3 }))
    }
  }
}
"#;
        let read = |text: &str| serde_json::to_string(&json(&ProgramHost.run("program/read", &[Value::String(text.to_string().into())]).expect("reads"))).unwrap();
        let text = read(probe);
        let (z, a, m) = (text.find("\"zeta\"").unwrap(), text.find("\"alpha\"").unwrap(), text.find("\"mid\"").unwrap());
        assert!(z < a && a < m, "author order zeta, alpha, mid: {text}");
        let control = read(&probe.replace("{ zeta: 1, alpha: 2, mid: 3 }", "{ alpha: 2, mid: 3, zeta: 1 }"));
        let (z, a, m) = (control.find("\"zeta\"").unwrap(), control.find("\"alpha\"").unwrap(), control.find("\"mid\"").unwrap());
        assert!(a < m && m < z, "control: the other author order is kept too: {control}");
    }

    #[test]
    fn read_lowers_text_to_the_program_emit_orb_prints_and_print_writes_it_back() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
            .expect("reads");
        let as_json = json(&program);
        assert_eq!(as_json["name"], "note-pad");
        let expected: serde_json::Value =
            serde_json::from_str(&orb_text(NOTE, "note.lolo")).unwrap();
        assert_eq!(
            as_json,
            json(&from_json(expected).unwrap()),
            "the value `orb emit orb` prints, as a kernel value"
        );
        let printed = host.run("program/print", &[program]).expect("prints");
        let Value::String(text) = printed else {
            panic!("text")
        };
        assert!(
            text.contains("note-pad") && text.contains("NoteView"),
            "{text}"
        );
    }

    #[test]
    fn read_with_embeds_lists_each_inline_embed_and_control_none_without_one() {
        let source = "app a [version: \"1.0.0\"]\norbital O {\n  uses Button from \"std/behaviors/std-button\"\n  entity E [runtime] {\n    id : string!\n  }\n  trait V -> E {\n    initial: idle\n    state idle {\n      INIT -> idle\n        (render-ui main <Button.traits.ButtonRender label={\"Go\"} />)\n    }\n  }\n}\n";
        let options = from_json(serde_json::json!({ "embeds": true })).unwrap();
        let embeds = json(&ProgramHost.run("program/read", &[Value::string(source), options.clone()]).expect("reads"));
        assert_eq!(embeds.as_array().map(Vec::len), Some(1), "{embeds}");
        assert_eq!(embeds[0]["behavior"], "Button.traits.ButtonRender");
        let none = json(&ProgramHost.run("program/read", &[Value::string(NOTE), options]).expect("reads"));
        assert_eq!(none, serde_json::json!([]));
    }

    #[test]
    fn read_with_resolve_answers_what_orb_resolve_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "resolve": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"));
        let expected = resolved_read(&file, None).map(|r| r.text).expect("orb resolve");
        assert_eq!(answer["text"], serde_json::Value::String(expected.clone()));
        assert_eq!(answer["program"], serde_json::from_str::<serde_json::Value>(&expected).unwrap());
    }

    #[test]
    fn read_with_resolve_says_whether_the_source_was_already_resolved() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE.replacen("app note-pad [", "app note-pad [\n  locales: [en, ar],", 1)).unwrap();
        let resolved_file = dir.path().join("note.orb");
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let read = |file: &std::path::Path, output: &std::path::Path| {
            let options = from_json(serde_json::json!({ "resolve": true, "path": file.display().to_string(), "output": output.display().to_string() })).unwrap();
            json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"))
        };
        let authored = read(&source, &resolved_file);
        assert_eq!(authored["loadedResolved"], false, "a .lolo is authored: {authored}");
        let resolved = read(&resolved_file, &out.join("again.orb"));
        assert_eq!(resolved["loadedResolved"], true, "a resolved .orb is loaded as it is: {resolved}");
        assert!(!out.join("again.orb").exists(), "a resolved source is not written out again");
        let mut unresolved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&resolved_file).unwrap()).unwrap();
        unresolved.as_object_mut().unwrap().remove("resolved");
        let bare = dir.path().join("bare.orb");
        std::fs::write(&bare, unresolved.to_string()).unwrap();
        let unstamped = read(&bare, &out.join("bare.orb"));
        assert_eq!(unstamped["loadedResolved"], false, "control: an .orb without the stamp is authored: {unstamped}");
        assert!(out.join("bare.orb").exists(), "control: an unresolved .orb is resolved and written");
    }

    #[test]
    fn read_with_resolve_names_where_the_program_lives() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE.replacen("app note-pad [", "app note-pad [\n  locales: [en, ar],", 1)).unwrap();
        let read = |file: &std::path::Path, output: Option<&std::path::Path>| {
            let mut options = serde_json::json!({ "resolve": true, "path": file.display().to_string() });
            if let Some(out) = output {
                options["output"] = out.display().to_string().into();
            }
            ProgramHost.run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
        };
        let written = dir.path().join("dist");
        std::fs::create_dir(&written).unwrap();
        let app = written.join("app.orb");
        let authored = json(&read(&source, Some(&app)).expect("resolves and writes"));
        assert_eq!(authored["programPath"], app.display().to_string(), "an authored source lives at its output: {authored}");
        assert_eq!(authored["loadedResolved"], false);
        assert!(app.exists());
        assert_eq!(std::fs::read_dir(&written).unwrap().count(), 1, "the program carries its messages: nothing is written beside it");
        assert!(authored["program"]["messages"]["ar"].is_object(), "{authored}");

        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let again = json(&read(&app, Some(&elsewhere.join("app.orb"))).expect("loads as it is"));
        assert_eq!(again["programPath"], app.display().to_string(), "a resolved source lives where it was read: {again}");
        assert_eq!(again["loadedResolved"], true);
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing is written when the source is resolved");

        let bare = json(&read(&app, None).expect("a resolved source with locales needs no output"));
        assert_eq!(bare["programPath"], app.display().to_string(), "{bare}");
        assert_eq!(bare["program"], again["program"]);

        let in_memory = json(&read(&source, None).expect("an authored source with locales needs no output"));
        let mut locales: Vec<&String> = in_memory["program"]["messages"].as_object().expect("messages").keys().collect();
        locales.sort();
        assert_eq!(locales, ["ar", "en"], "the program carries each declared locale's messages: {in_memory}");
        assert!(in_memory.get("programPath").is_none(), "{in_memory}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "nothing new is written beside the source (note.lolo, dist, elsewhere)");
        assert!(in_memory.get("catalogs").is_none() && bare.get("catalogs").is_none(), "control: no catalogs ride beside the program: {bare}");
    }

    #[test]
    fn read_with_resolve_without_text_and_with_output_answers_no_text() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE.replacen("app note-pad [", "app note-pad [\n  locales: [en, ar],", 1)).unwrap();
        let read = |file: &std::path::Path, output: &std::path::Path| {
            let options = from_json(serde_json::json!({ "resolve": true, "text": false, "path": file.display().to_string(), "output": output.display().to_string() })).unwrap();
            json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"))
        };
        let dist = dir.path().join("dist");
        std::fs::create_dir(&dist).unwrap();
        let app = dist.join("app.orb");
        let authored = read(&source, &app);
        assert!(authored.get("text").is_none(), "no text in the answer: {authored}");
        assert_eq!(authored["loadedResolved"], false);
        assert_eq!(authored["programPath"], app.display().to_string());
        assert_eq!(authored["program"]["name"], "note-pad");
        assert!(app.exists(), "an authored source is written to output");
        assert_eq!(std::fs::read_dir(&dist).unwrap().count(), 1, "the program carries its messages: nothing is written beside it");

        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let again = read(&app, &elsewhere.join("app.orb"));
        assert!(again.get("text").is_none(), "{again}");
        assert_eq!(again["loadedResolved"], true);
        assert_eq!(again["programPath"], app.display().to_string());
        assert_eq!(again["program"], authored["program"]);
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "a resolved source is answered in place");
    }

    #[test]
    fn control_read_with_resolve_without_text_and_output_answers_program_and_lazy_only() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "resolve": true, "text": false, "path": source.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"));
        assert_eq!(answer["lazy"], serde_json::json!([]), "{answer}");
        assert!(answer.get("text").is_none() && answer.get("loadedResolved").is_none() && answer.get("programPath").is_none(), "{answer}");
    }

    #[test]
    fn control_read_with_resolve_refuses_a_resolved_orb_of_another_type_contract() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE).unwrap();
        let mut resolved: serde_json::Value = serde_json::from_str(&resolved_read(&source, None).map(|r| r.text).unwrap()).unwrap();
        resolved["resolved"]["typeContractVersion"] = 999.into();
        let stale = dir.path().join("stale.orb");
        std::fs::write(&stale, resolved.to_string()).unwrap();
        let options = from_json(serde_json::json!({ "resolve": true, "path": stale.display().to_string() })).unwrap();
        let refused = ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).unwrap_err();
        assert!(refused.message.contains("type contract 999"), "{}", refused.message);
    }

    #[test]
    fn read_with_resolve_in_memory_names_no_program_path() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "resolve": true, "path": source.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"));
        assert!(answer.get("programPath").is_none(), "resolved in memory and written nowhere: {answer}");
    }

    #[test]
    fn read_with_resolve_answers_a_resolved_orb_as_it_is() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE).unwrap();
        let resolved_text = resolved_read(&source, None).map(|r| r.text).expect("orb resolve");
        let resolved_file = dir.path().join("note.orb");
        std::fs::write(&resolved_file, &resolved_text).unwrap();
        let read = |file: &std::path::Path| {
            let options = from_json(serde_json::json!({ "resolve": true, "path": file.display().to_string() })).unwrap();
            json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("resolves"))
        };
        let answer = read(&resolved_file);
        assert!(answer["program"]["resolved"]["orb"].is_string(), "{answer}");
        assert_eq!(answer["program"], read(&source)["program"]);
    }

    #[test]
    fn the_resolved_value_is_the_printed_resolve_read_back() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("note.lolo");
        std::fs::write(&source, NOTE).unwrap();
        let printed: Value = serde_json::from_str::<Value>(&resolved_read(&source, None).map(|r| r.text).expect("orb resolve"))
            .expect("reads back");
        let direct = resolved_value(&source).expect("resolved value");
        assert_eq!(direct.get_path("program"), Some(&printed));
        assert_eq!(direct.get_path("lazy"), Some(&Value::array(Vec::<Value>::new())));
        let other = std::fs::write(&source, NOTE.replacen("note-pad", "note-other", 1));
        other.unwrap();
        let changed = resolved_value(&source).expect("resolved value");
        assert_ne!(changed.get_path("program"), Some(&printed), "control: another program is another value");
    }

    #[test]
    fn read_with_resolve_and_no_text_answers_a_program_with_locales() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE.replacen("app note-pad [", "app note-pad [\n  locales: [en, ar],", 1)).unwrap();
        let read = |extra: serde_json::Value| {
            let mut options = serde_json::json!({ "resolve": true, "path": file.display().to_string() });
            options.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            ProgramHost.run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
        };
        let answer = json(&read(serde_json::json!({ "text": false })).expect("answers the program"));
        assert_eq!(answer["program"]["name"], "note-pad", "{answer}");
        assert!(answer.get("text").is_none(), "no text is printed: {answer}");
        let printed = json(&read(serde_json::json!({})).expect("the printed form needs no output either"));
        assert!(printed["text"].is_string() && printed["program"]["messages"]["en"].is_object(), "{printed}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "nothing is written beside the source");
    }

    #[test]
    fn read_with_resolve_and_no_text_answers_each_lazy_child_by_its_orbital() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("child.lolo"), "app child [ version: \"1.0.0\" ]\norbital ChildOrbital {\n  entity Note [runtime] {\n    id : string!\n  }\n  trait ChildView -> Note {\n    initial: a\n    state a {\n      INIT -> a\n        (render-ui main { type: \"typography\", content: \"child\" })\n    }\n  }\n  page \"/child\" as ChildPage -> ChildView\n}\n").unwrap();
        let main = |lazy: bool| {
            let head = if lazy { "uses lazy Child from \"./child\"\n" } else { "" };
            let tail = if lazy { "orbital Kid = Child.orbitals.ChildOrbital {\n}\n" } else { "" };
            format!("app main [ version: \"1.0.0\" ]\n{head}orbital MainOrbital {{\n  entity Item [runtime] {{\n    id : string!\n  }}\n  trait MainView -> Item {{\n    initial: a\n    state a {{\n      INIT -> a\n        (render-ui main {{ type: \"typography\", content: \"main\" }})\n    }}\n  }}\n  page \"/\" as Home -> MainView\n}}\n{tail}")
        };
        let read = |text: String| {
            let file = dir.path().join("main.lolo");
            std::fs::write(&file, text).unwrap();
            let options = from_json(serde_json::json!({ "resolve": true, "text": false, "path": file.display().to_string() })).unwrap();
            json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"))
        };
        let with = read(main(true));
        let lazy = with["lazy"].as_array().expect("lazy list");
        assert_eq!(lazy.len(), 1, "{with}");
        assert_eq!(lazy[0]["orbital"], "Kid");
        assert!(lazy[0]["program"]["orbitals"].is_array(), "the child program is answered: {}", lazy[0]);
        assert_eq!(read(main(false))["lazy"], serde_json::json!([]), "control: no lazy import, no children");
    }

    const REFERENCED_ATOM: &str = r#"{ "name": "atom", "version": "1.0.0", "orbitals": [ { "name": "AtomOrbital",
      "entity": { "name": "Widget", "persistence": "persistent", "fields": [ { "name": "id", "type": "string", "required": true }, { "name": "n", "type": "number" } ] },
      "traits": [ { "name": "Widget", "linkedEntity": "Widget", 
        "config": { "k": { "type": "number", "default": 0 } },
        "stateMachine": { "states": [ { "name": "idle", "isInitial": true } ], "events": [ { "key": "LOAD" } ],
          "transitions": [ { "from": "idle", "to": "idle", "event": "LOAD", "effects": [ ["set", "@entity.n", "@config.k"] ] } ] } } ] } ] }"#;

    const REFERENCED_CONSUMER: &str = r#"{ "name": "consumer", "version": "1.0.0", "orbitals": [ { "name": "Composer",
      "uses": [ { "from": "./atom.orb", "as": "Up" } ],
      "entity": { "name": "Rec", "persistence": "runtime", "fields": [ { "name": "id", "type": "string", "required": true } ] },
      "traits": [ { "ref": "Up.traits.Widget", "name": "T", "linkedEntity": "Rec", "config": { "k": { "type": "number", "default": 1 } } } ] } ] }"#;

    #[test]
    fn read_with_references_answers_the_unresolved_program_and_each_reference_with_its_declaration() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("atom.orb"), REFERENCED_ATOM).unwrap();
        let file = dir.path().join("consumer.orb");
        std::fs::write(&file, REFERENCED_CONSUMER).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        assert_eq!(answer["program"]["name"], "consumer", "{answer}");
        assert_eq!(answer["program"]["orbitals"][0]["traits"][0]["ref"], "Up.traits.Widget", "the program is unresolved: {answer}");
        let references = answer["references"].as_array().expect("a list");
        assert_eq!(references.len(), 2, "{answer}");
        let uses = references.iter().find(|r| r["kind"] == "uses").expect("the uses import is a reference");
        assert_eq!(uses["alias"], "Up");
        assert_eq!(uses["declaration"], serde_json::json!({}), "an import declaring no app-level config answers an empty one: {uses}");
        assert!(uses.get("aliasOrbitals").is_none(), "{uses}");
        let pair = references.iter().find(|r| r["kind"] == "trait").expect("the trait reference");
        assert_eq!(pair["alias"], "Up");
        assert_eq!(pair["name"], "Widget");
        assert_eq!(pair["path"], "orbitals[name=Composer].traits[0]");
        assert_eq!(pair["callSite"]["config"]["k"]["default"], 1);
        assert_eq!(pair["declaration"], serde_json::json!({ "file": "atom.orb", "orbital": 0, "trait": 0 }), "{pair}");
        let declared = &answer["files"]["atom.orb"]["orbitals"][0]["traits"][0];
        assert_eq!(declared["name"], "Widget", "{answer}");
        assert!(declared.to_string().contains("@config.k"), "{declared}");
    }

    #[test]
    fn read_with_references_answers_one_import_once_for_two_references_to_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("atom.orb"), REFERENCED_ATOM).unwrap();
        let consumer = r#"{ "name": "consumer", "version": "1.0.0", "orbitals": [ { "name": "Composer",
          "uses": [ { "from": "./atom.orb", "as": "Up" } ],
          "entity": { "name": "Rec", "persistence": "runtime", "fields": [ { "name": "id", "type": "string", "required": true } ] },
          "traits": [ { "ref": "Up.traits.Widget", "name": "A", "linkedEntity": "Rec" }, { "ref": "Up.traits.Widget", "name": "B", "linkedEntity": "Rec", "config": { "k": { "type": "number", "default": 2 } } } ] } ] }"#;
        let file = dir.path().join("consumer.orb");
        std::fs::write(&file, consumer).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        let references = answer["references"].as_array().expect("a list");
        assert_eq!(references.len(), 3, "{answer}");
        let files = answer["files"].as_object().expect("a map");
        assert_eq!(files.keys().collect::<Vec<_>>(), ["atom.orb"], "one entry for the one import: {answer}");
        assert_eq!(files["atom.orb"]["orbitals"].as_array().expect("orbitals").len(), 1);
        assert_eq!(answer["files"].to_string().matches("\"stateMachine\"").count(), 1, "the declaration is serialized once: {answer}");
        let traits: Vec<_> = references.iter().filter(|r| r["kind"] == "trait").collect();
        assert_eq!(traits.len(), 2);
        assert_eq!(traits[0]["declaration"], traits[1]["declaration"], "both name the same declaration");
        assert!(references.iter().all(|r| r.get("aliasOrbitals").is_none()), "{answer}");
    }

    #[test]
    fn read_with_references_answers_no_files_for_a_program_with_no_uses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("atom.orb");
        std::fs::write(&file, REFERENCED_ATOM).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        assert_eq!(answer["files"], serde_json::json!({}), "{answer}");
        assert_eq!(answer["references"], serde_json::json!([]), "{answer}");
    }

    #[test]
    fn read_with_references_pairs_an_orbital_reference_with_the_other_orbitals_of_its_import() {
        let upstream = r#"{ "name": "up", "version": "1.0.0", "config": { "title": { "type": "string", "default": "t" } }, "orbitals": [
          { "name": "MainOrbital", "entity": { "name": "Widget", "persistence": "persistent", "fields": [ { "name": "id", "type": "string", "required": true } ] }, "traits": [] },
          { "name": "OtherOrbital", "entity": { "name": "Other", "persistence": "persistent", "fields": [ { "name": "id", "type": "string", "required": true } ] }, "traits": [] } ] }"#;
        let consumer = r#"{ "name": "consumer", "version": "1.0.0", "orbitals": [ { "name": "Local",
          "uses": [ { "from": "./up.orb", "as": "Up", "config": { "title": { "type": "string", "default": "x" } } } ],
          "entity": "Up.orbitals.MainOrbital.entity", "traits": [], "reference": { "ref": "Up.orbitals.MainOrbital" } } ] }"#;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("up.orb"), upstream).unwrap();
        let file = dir.path().join("consumer.orb");
        std::fs::write(&file, consumer).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        let references = answer["references"].as_array().expect("a list");
        let orbital = references.iter().find(|r| r["kind"] == "orbital").expect("the orbital reference");
        assert_eq!(orbital["declaration"], serde_json::json!({ "file": "up.orb", "index": 0 }), "{orbital}");
        assert!(orbital.get("aliasOrbitals").is_none(), "{orbital}");
        let declared = answer["files"]["up.orb"]["orbitals"].as_array().expect("the orbitals of the import");
        assert_eq!(declared[0]["name"], "MainOrbital", "{answer}");
        assert_eq!(declared.len(), 2, "{answer}");
        assert_eq!(declared[1]["name"], "OtherOrbital");
        assert_eq!(declared[1]["entity"]["name"], "Other");
        let uses = references.iter().find(|r| r["kind"] == "uses").expect("the uses import");
        assert_eq!(uses["callSite"]["config"]["title"]["default"], "x", "{uses}");
        assert!(uses["declaration"]["title"].is_object(), "the import's declared app-level knob: {uses}");
    }

    fn prover_program(guard: Option<serde_json::Value>, invariant: serde_json::Value) -> serde_json::Value {
        let mut transition = serde_json::json!({ "from": "open", "to": "open", "event": "ADD",
            "effects": [["persist", "create", "Order", { "total": "@payload.total" }]] });
        if let Some(g) = guard {
            transition["guard"] = g;
        }
        serde_json::json!({ "name": "App", "version": "1.0.0", "orbitals": [{
            "name": "Shop",
            "entity": { "name": "Order", "collection": "orders", "persistence": "persistent", "fields": [
                { "name": "id", "type": "string", "required": true },
                { "name": "total", "type": "number", "required": true },
                { "name": "status", "type": "string" } ] },
            "expects": [{ "kind": "entity", "name": "Order", "invariants": [invariant], "strict": true }],
            "traits": [{ "name": "Checkout", "linkedEntity": "Order", "stateMachine": {
                "states": [{ "name": "open", "isInitial": true }],
                "events": [{ "key": "INIT", "name": "Init" }, { "key": "ADD", "name": "Add", "payloadSchema": [{ "name": "total", "type": "number", "required": true }] }],
                "transitions": [transition] } }],
            "pages": [] }] })
    }

    fn prove_program(program: serde_json::Value, options: Option<serde_json::Value>) -> Result<serde_json::Value, SubstrateFailure> {
        let mut args = vec![from_json(program).unwrap()];
        args.extend(options.map(|o| from_json(o).unwrap()));
        ProgramHost.run("program/prove", &args).map(|v| json(&v))
    }

    #[test]
    fn prove_answers_a_confirmed_counterexample_as_violated() {
        let answer = prove_program(prover_program(None, serde_json::json!([">=", "@entity.total", 0])), None).expect("answers");
        let findings = answer["findings"].as_array().expect("a list");
        assert_eq!(findings.len(), 1, "{answer}");
        assert_eq!(findings[0]["verdict"], "violated");
        assert!(findings[0]["counterexample"]["payload"]["total"].as_f64().unwrap() < 0.0, "{answer}");
        assert_eq!(findings[0]["proposition"]["kind"], "row");
        assert_eq!(findings[0]["proposition"]["entity"], "Order");
        assert_eq!(findings[0]["proposition"]["expr"], serde_json::json!([">=", "@entity.total", 0]));
        assert!(findings[0]["site"].as_str().unwrap().contains("Checkout"), "{answer}");
        let reason = findings[0]["reason"].as_str().unwrap();
        assert!(reason.contains("(>= @entity.total 0)") && reason.contains("false on the counterexample"), "the violation names what failed: {reason}");
        let unproved = prove_program(prover_program(Some(serde_json::json!([">=", "@payload.total", 0])), serde_json::json!([">", ["str/len", "@entity.status"], 0])), None).expect("answers");
        assert_ne!(unproved["findings"][0]["reason"], findings[0]["reason"], "control: an unproved finding keeps the prover's own reason");
    }

    #[test]
    fn serialized_builds_the_same_value_in_the_same_key_order_as_the_json_round_trip() {
        let program = prover_program(None, serde_json::json!([">=", "@entity.total", 0]));
        let schema: orbital_core::OrbitalSchema = serde_json::from_value(program).expect("a program");
        let report = orbital_core::proof::prove_report(&schema);
        let direct = serialized(&report).expect("serializes");
        let round_trip = from_json(serde_json::to_value(&report).unwrap()).unwrap();
        assert_eq!(serde_json::to_string(&json(&direct)).unwrap(), serde_json::to_string(&json(&round_trip)).unwrap());
    }

    #[test]
    fn prove_answers_an_undecidable_atom_as_unproved_with_its_reason() {
        let program = prover_program(Some(serde_json::json!([">=", "@payload.total", 0])), serde_json::json!([">", ["str/len", "@entity.status"], 0]));
        let answer = prove_program(program, None).expect("answers");
        let findings = answer["findings"].as_array().expect("a list");
        assert_eq!(findings.len(), 1, "{answer}");
        assert_eq!(findings[0]["verdict"], "unproved");
        assert!(findings[0]["reason"].as_str().unwrap().contains("str/len"), "{answer}");
    }

    #[test]
    fn control_prove_answers_no_finding_for_a_proved_proposition_and_for_a_program_with_none() {
        let proved = prover_program(Some(serde_json::json!([">=", "@payload.total", 0])), serde_json::json!([">=", "@entity.total", 0]));
        assert_eq!(prove_program(proved, None).expect("answers"), serde_json::json!({ "findings": [] }));
        let mut none = prover_program(None, serde_json::json!(true));
        none["orbitals"][0]["expects"] = serde_json::json!([]);
        assert_eq!(prove_program(none, None).expect("answers"), serde_json::json!({ "findings": [] }));
    }

    #[test]
    fn prove_reads_the_file_at_an_absolute_path_and_refuses_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("shop.orb");
        std::fs::write(&file, prover_program(None, serde_json::json!([">=", "@entity.total", 0])).to_string()).unwrap();
        let by_path = |path: &str| {
            let options = from_json(serde_json::json!({ "path": path })).unwrap();
            ProgramHost.run("program/prove", &[Value::Null, options]).map(|v| json(&v))
        };
        let answer = by_path(&file.display().to_string()).expect("answers");
        assert_eq!(answer["findings"][0]["verdict"], "violated", "{answer}");
        assert!(by_path("shop.orb").unwrap_err().message.contains("absolute `path`"));
        assert!(by_path("/nowhere/shop.orb").unwrap_err().message.contains("Failed to read"));
        let unknown = from_json(serde_json::json!({ "path": file.display().to_string(), "bogus": true })).unwrap();
        assert!(ProgramHost.run("program/prove", &[Value::Null, unknown]).unwrap_err().message.contains("unsupported option"));
        assert!(prove_program(serde_json::json!({ "name": "x" }), None).unwrap_err().message.contains("not a program"));
    }

    fn os_grants_program(grants: serde_json::Value, call: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "name": "App", "version": "1.0.0", "orbitals": [{
            "name": "Files", "osGrants": grants,
            "entity": { "name": "Row", "persistence": "runtime", "fields": [{ "name": "id", "type": "string", "required": true }] },
            "traits": [{ "name": "Reader", "linkedEntity": "Row", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "events": [{ "key": "INIT", "name": "Init" }],
                "transitions": [{ "from": "idle", "to": "idle", "event": "INIT", "effects": [call] }] } }],
            "pages": [] }] })
    }

    fn os_grants_of(program: serde_json::Value, options: Option<serde_json::Value>) -> Result<serde_json::Value, SubstrateFailure> {
        let mut args = vec![from_json(program).unwrap()];
        args.extend(options.map(|o| from_json(o).unwrap()));
        ProgramHost.run("program/os-grants", &args).map(|v| json(&v))
    }

    #[test]
    fn os_grants_answers_a_missing_grant_and_control_a_covered_call_has_no_finding() {
        let grants = serde_json::json!({ "fs": [{ "glob": "/data/**", "modes": ["read"] }] });
        let answer = os_grants_of(os_grants_program(grants.clone(), serde_json::json!(["os/read-file", "/etc/passwd"])), None).expect("answers");
        let findings = answer["findings"].as_array().expect("a list");
        assert_eq!(findings.len(), 1, "the written pass reports missing only; unused is the final pass's: {answer}");
        assert_eq!(findings[0]["code"], "missing");
        assert_eq!(findings[0]["orbital"], "Files");
        assert_eq!(findings[0]["call"], "os/read-file");
        assert_eq!(findings[0]["grant"], "fs: { \"/etc/passwd\": [read] }");
        let finished = os_grants_of(os_grants_program(grants.clone(), serde_json::json!(["os/read-file", "/etc/passwd"])), Some(serde_json::json!({ "final": true }))).expect("answers");
        assert!(finished["findings"].as_array().expect("a list").iter().any(|f| f["code"] == "unused"), "the final pass reports the unused grant: {finished}");
        let covered = os_grants_of(os_grants_program(grants, serde_json::json!(["os/read-file", "/data/a"])), None).expect("answers");
        assert_eq!(covered, serde_json::json!({ "findings": [] }));
    }

    #[test]
    fn os_grants_answers_a_computed_target_and_the_final_pass_reads_config() {
        let computed = os_grants_of(os_grants_program(serde_json::json!({}), serde_json::json!(["os/read-file", "@payload.path"])), None).expect("answers");
        assert_eq!(computed["findings"][0]["code"], "target-not-static");
        assert_eq!(computed["findings"][0]["argument"], 1);
        let mut fed = os_grants_program(serde_json::json!({ "exec": ["ls"] }), serde_json::json!(["os/spawn", "@config.cmd", []]));
        fed["orbitals"][0]["traits"][0]["config"] = serde_json::json!({ "cmd": { "type": "string", "default": "git" } });
        let written = os_grants_of(fed.clone(), None).expect("answers");
        assert_eq!(written, serde_json::json!({ "findings": [] }), "control: the written pass defers a config target and reports no unused: {written}");
        let finished = os_grants_of(fed, Some(serde_json::json!({ "final": true }))).expect("answers");
        assert_eq!(finished["findings"][0]["code"], "missing");
        assert_eq!(finished["findings"][0]["via"], "config");
    }

    #[test]
    fn os_grants_reads_the_file_at_an_absolute_path_and_refuses_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("files.orb");
        std::fs::write(&file, os_grants_program(serde_json::json!({}), serde_json::json!(["os/memory-info"])).to_string()).unwrap();
        let by_path = |options: serde_json::Value| ProgramHost.run("program/os-grants", &[Value::Null, from_json(options).unwrap()]).map(|v| json(&v));
        let answer = by_path(serde_json::json!({ "path": file.display().to_string() })).expect("answers");
        assert_eq!(answer["findings"][0]["code"], "missing", "{answer}");
        assert!(by_path(serde_json::json!({ "path": "files.orb" })).unwrap_err().message.contains("absolute `path`"));
        assert!(by_path(serde_json::json!({ "path": file.display().to_string(), "bogus": true })).unwrap_err().message.contains("unsupported option"));
        assert!(os_grants_of(serde_json::json!({ "name": "x" }), None).unwrap_err().message.contains("not a program"));
    }

    #[test]
    fn control_read_with_references_answers_none_for_a_program_without_uses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("atom.orb");
        std::fs::write(&file, REFERENCED_ATOM).unwrap();
        let options = from_json(serde_json::json!({ "references": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("answers"));
        assert_eq!(answer["references"], serde_json::json!([]));
    }

    #[test]
    fn control_read_with_references_refuses_a_relative_path_an_unknown_option_and_a_second_mode() {
        let read = |options: serde_json::Value| {
            ProgramHost
                .run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
                .unwrap_err()
                .message
        };
        assert!(read(serde_json::json!({ "references": true, "path": "note.orb" })).contains("absolute `path`"));
        assert!(read(serde_json::json!({ "references": true, "path": "/x/note.orb", "bogus": true })).contains("unsupported option \"bogus\""));
        assert!(read(serde_json::json!({ "references": true, "stamp": true, "path": "/x/note.orb" })).contains("one of inspect, embeds, resolve, stamp, references, findings per call"));
        assert!(read(serde_json::json!({ "references": true, "path": "/nowhere/note.orb" })).contains("Failed to read"));
    }

    #[test]
    fn read_with_findings_surfaces_the_l1_warnings_the_lowered_program_loses() {
        let body = "orbital Foo {\n  entity Bar [runtime] {\n    id : string!\n  }\n}\n";
        let options = from_json(serde_json::json!({ "findings": true })).unwrap();
        let read = |text: String| json(&ProgramHost.run("program/read", &[Value::string(text), options.clone()]).expect("reads"));
        let legacy = read(format!("app Foo \"1.0.0\"\n{body}"));
        assert_eq!(legacy.as_array().map(Vec::len), Some(1), "{legacy}");
        assert_eq!(legacy[0]["code"], "ORB_L_APP_HEADER_LEGACY");
        assert_eq!(legacy[0]["severity"], "warning");
        assert_eq!(legacy[0]["path"], "app:1");
        assert_eq!(read(format!("app Foo [version: \"1.0.0\"]\n{body}")), serde_json::json!([]), "control: the bracket header is clean");
        let report_input = serde_json::json!({ "findings": legacy });
        assert!(serde_json::from_value::<Vec<orbital_compiler::phases::validation::report::ProgramFinding>>(report_input["findings"].clone()).is_ok(), "the list feeds program/validate's `findings` as is");
    }

    #[test]
    fn read_with_resolve_refuses_a_relative_path() {
        let options = from_json(serde_json::json!({ "resolve": true, "path": "note.lolo" })).unwrap();
        let error = ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).unwrap_err();
        assert!(error.message.contains("absolute `path`"), "{}", error.message);
    }

    fn text_of(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap_or_else(|| panic!("`{key}` in {value}")).to_string()
    }

    #[test]
    fn print_with_a_target_answers_what_orb_emit_prints_and_each_target_differs() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let path = file.display().to_string();
        let orb = json(&print_with(serde_json::json!({ "target": "orb", "path": path })).expect("emits orb"));
        let expected = format!("{}\n", orb_text(NOTE, &path));
        assert_eq!(text_of(&orb, "text"), expected, "the bytes `orb emit orb` prints");
        assert_eq!(text_of(&orb, "stderr"), "");
        let lolo = json(&print_with(serde_json::json!({ "target": "lolo", "path": path })).expect("emits lolo"));
        let expected = format!("{}\n", lolo_text(NOTE, &path).unwrap());
        assert_eq!(text_of(&lolo, "text"), expected, "the bytes `orb emit lolo` prints");
        assert_ne!(text_of(&lolo, "text"), text_of(&orb, "text"), "control: the target picks the output");
    }

    #[test]
    fn print_with_a_target_and_output_writes_the_file_and_prints_nothing() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let out = dir.path().join("note.orb");
        let answer = json(
            &print_with(serde_json::json!({ "target": "orb", "path": file.display().to_string(), "output": out.display().to_string() }))
                .expect("emits"),
        );
        assert_eq!(text_of(&answer, "text"), "");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), orb_text(NOTE, &file.display().to_string()));
    }

    #[test]
    fn control_print_with_a_target_refuses_a_relative_path_an_unknown_option_and_a_lone_key_half() {
        let refusal = |options: serde_json::Value| print_with(options).unwrap_err().message;
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "note.lolo" })).contains("absolute `path`"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/x/note.lolo", "node": "a" })).contains("unsupported option \"node\""));
        assert!(refusal(serde_json::json!({ "target": "java", "path": "/x/note.lolo" })).contains("not one of the accepted values"));
        assert!(refusal(serde_json::json!({ "target": "manifest", "path": "/x/n.lolo", "signKey": "/k" })).contains("go together"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/x/n.lolo", "documentVersion": 2 })).contains("needs `inspectSource`"));
        assert!(refusal(serde_json::json!({ "target": "orb", "path": "/nowhere/missing.lolo" })).contains("Failed to read"));
    }

    #[test]
    fn read_with_stamp_answers_what_orb_stamp_prints_and_writes_output_when_given() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("note.orb");
        std::fs::write(&orb, orb_text(NOTE, "note.lolo")).unwrap();
        let options = from_json(serde_json::json!({ "stamp": true, "path": orb.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("stamps"));
        let expected = stamped_json(&orb, None, None).expect("orb stamp");
        assert_eq!(answer["text"], serde_json::Value::String(expected.clone()));
        assert_eq!(answer["program"], serde_json::from_str::<serde_json::Value>(&expected).unwrap());
        let out = dir.path().join("stamped.orb");
        let options = from_json(serde_json::json!({
            "stamp": true, "path": orb.display().to_string(), "prior": orb.display().to_string(), "output": out.display().to_string()
        }))
        .unwrap();
        let written = json(&ProgramHost.run("program/read", &[Value::String(String::new().into()), options]).expect("stamps"));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text_of(&written, "text"), "--output writes the stamped text");
        let with_prior = stamped_json(&orb, Some(&orb), None).unwrap();
        assert_eq!(text_of(&written, "text"), with_prior, "--prior anchors the ids");
    }

    #[test]
    fn control_read_with_stamp_refuses_a_relative_path_an_unknown_option_and_a_missing_file() {
        let read = |options: serde_json::Value| {
            ProgramHost
                .run("program/read", &[Value::String(String::new().into()), from_json(options).unwrap()])
                .unwrap_err()
                .message
        };
        assert!(read(serde_json::json!({ "stamp": true, "path": "note.orb" })).contains("absolute `path`"));
        assert!(read(serde_json::json!({ "stamp": true, "path": "/x/note.orb", "resolve": true })).contains("unsupported option \"resolve\""));
        assert!(read(serde_json::json!({ "stamp": true, "path": "/nowhere/note.orb" })).contains("Failed to read"));
    }

    fn generate_with(options: serde_json::Value) -> Result<Value, SubstrateFailure> {
        ProgramHost.run("program/eval", &[Value::Null, from_json(options).unwrap()])
    }

    #[test]
    fn a_shells_prebuilt_web_client_is_copied_into_the_output_with_its_nested_assets() {
        let shell = tempfile::tempdir().expect("tempdir");
        let built = shell.path().join("almadar-web-client");
        std::fs::create_dir_all(built.join("assets/fonts")).unwrap();
        std::fs::write(built.join("index.html"), "<html></html>").unwrap();
        std::fs::write(built.join("assets/fonts/a.woff2"), "font").unwrap();
        let out = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(out.path().join("packages/client/public")).unwrap();
        std::fs::write(out.path().join("packages/client/public/client.json"), "{}").unwrap();
        std::fs::write(out.path().join("packages/client/public/program.client.orb"), "{\"name\":\"app\"}").unwrap();
        let copied = copy_web_client(Some(&built), out.path()).expect("copies");
        assert_eq!(copied, out.path().join("web-client"));
        assert_eq!(std::fs::read_to_string(copied.join("index.html")).unwrap(), "<html></html>");
        assert_eq!(std::fs::read_to_string(copied.join("assets/fonts/a.woff2")).unwrap(), "font");
        assert_eq!(std::fs::read_to_string(copied.join("program.client.orb")).unwrap(), "{\"name\":\"app\"}");
        let report = orbital_compiler::mixed::CompileReport { files: 3, ..Default::default() };
        let answer = generate_answer(&report, Some(&copied));
        assert_eq!(answer["webClient"], copied.display().to_string());
        assert!(generate_answer(&report, None).get("webClient").is_some_and(serde_json::Value::is_null));
    }

    #[test]
    fn g_orbital_shell_typescript_142_a_rebuild_clears_the_previous_clients_assets() {
        let shell = tempfile::tempdir().expect("tempdir");
        let built = shell.path().join("almadar-web-client");
        std::fs::create_dir_all(built.join("assets")).unwrap();
        std::fs::write(built.join("index.html"), "<html></html>").unwrap();
        std::fs::write(built.join("assets/app-new.js"), "new").unwrap();
        let out = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(out.path().join("packages/client/public")).unwrap();
        std::fs::write(out.path().join("packages/client/public/client.json"), "{}").unwrap();
        std::fs::create_dir_all(out.path().join("web-client/assets")).unwrap();
        std::fs::write(out.path().join("web-client/assets/app-old.js"), "old").unwrap();
        let copied = copy_web_client(Some(&built), out.path()).expect("copies");
        assert!(!copied.join("assets/app-old.js").exists(), "a hashed asset of an earlier build stays");
        assert_eq!(std::fs::read_to_string(copied.join("assets/app-new.js")).unwrap(), "new");
        let again = copy_web_client(Some(&built), out.path()).expect("copies again");
        assert_eq!(std::fs::read_to_string(again.join("assets/app-new.js")).unwrap(), "new");
    }

    #[test]
    fn control_no_built_client_or_no_client_mode_files_is_refused() {
        let out = tempfile::tempdir().expect("tempdir");
        let none = copy_web_client(None, out.path()).unwrap_err().message;
        assert!(none.contains("ships no prebuilt web client"), "{none}");
        let shell = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(shell.path().join("assets")).unwrap();
        let unbuilt = copy_web_client(Some(shell.path()), out.path()).unwrap_err().message;
        assert!(unbuilt.contains("holds no index.html"), "{unbuilt}");
        assert!(!out.path().join("web-client").exists());
        std::fs::write(shell.path().join("index.html"), "<html></html>").unwrap();
        let unprojected = copy_web_client(Some(shell.path()), out.path()).unwrap_err().message;
        assert!(unprojected.contains("wrote no client.json"), "{unprojected}");
    }

    #[test]
    fn control_eval_with_generate_refuses_a_relative_path_an_unknown_option_and_a_missing_shell() {
        let refusal = |options: serde_json::Value| generate_with(options).unwrap_err().message;
        assert!(refusal(serde_json::json!({ "generate": { "path": "n.lolo", "output": "/o", "shell": "typescript" } })).contains("absolute `path`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "o", "shell": "typescript" } })).contains("absolute `output`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "typescript", "verbose": true } })).contains("unsupported option \"verbose\""));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o" } })).contains("needs the `shell`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.orb", "output": "/o", "shell": "typescript", "source": "app.lolo" } })).contains("absolute `source`"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "cobol" } })).contains("not one of the accepted values"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "android", "renderer": "native-gpu2" } })).contains("`renderer` \"native-gpu2\" is not one of the accepted values"));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/n.lolo", "output": "/o", "shell": "typescript" }, "final": true })).contains("unsupported option \"final\""));
        assert!(refusal(serde_json::json!({ "generate": { "path": "/nowhere/n.lolo", "output": "/o", "shell": "typescript" } })).contains("program/eval:"));
    }

    #[test]
    fn step_with_a_raw_report_is_what_orb_execute_steps_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let source = NOTE.to_string();
        let program = ProgramHost.run("program/read", &[Value::String(source.into())]).expect("reads");
        let steps = serde_json::json!([{ "trait": "NoteView", "event": "INIT" }]);
        let raw = from_json(serde_json::json!({ "report": "raw" })).unwrap();
        let answer = json(&ProgramHost.run("program/step", &[program.clone(), from_json(steps.clone()).unwrap(), raw]).expect("steps"));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("program.orb");
        std::fs::write(&orb, json(&program).to_string()).unwrap();
        let expected = serde_json::to_value(run_cascade(&orb, &steps, false, false, true).expect("execute --steps")).unwrap();
        assert_eq!(answer, expected);
        assert_eq!(answer["success"], true, "{answer}");
    }

    #[test]
    fn step_cost_must_be_a_boolean_and_without_a_counting_allocator_no_step_carries_a_cost() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let program = ProgramHost.run("program/read", &[Value::String(NOTE.to_string().into())]).expect("reads");
        let steps = from_json(serde_json::json!([{ "trait": "NoteView", "event": "INIT" }])).unwrap();
        let refused = ProgramHost
            .run("program/step", &[program.clone(), steps.clone(), from_json(serde_json::json!({ "report": "raw", "cost": "yes" })).unwrap()])
            .unwrap_err();
        assert!(refused.message.contains("options.cost must be a boolean"), "{}", refused.message);
        let answer = json(
            &ProgramHost
                .run("program/step", &[program, steps, from_json(serde_json::json!({ "report": "raw", "cost": true })).unwrap()])
                .expect("steps"),
        );
        assert_eq!(answer["success"], true, "{answer}");
        let outcome = &answer["steps"][0]["outcomes"][0];
        assert!(outcome.get("cost").is_none(), "this test binary counts no allocations, so cost is absent, never zero: {outcome}");
    }

    #[test]
    fn control_a_reshaped_step_still_needs_its_from_and_report_takes_only_raw() {
        let source = NOTE.to_string();
        let program = ProgramHost.run("program/read", &[Value::String(source.into())]).expect("reads");
        let steps = from_json(serde_json::json!([{ "orbital": "NoteOrbital", "trait": "NoteView", "event": "INIT" }])).unwrap();
        let error = ProgramHost.run("program/step", &[program.clone(), steps.clone()]).unwrap_err();
        assert!(error.message.contains("needs a from"), "{}", error.message);
        let wrong = from_json(serde_json::json!({ "report": "full" })).unwrap();
        let error = ProgramHost.run("program/step", &[program, steps, wrong]).unwrap_err();
        assert!(error.message.contains("must be \"raw\""), "{}", error.message);
    }

    #[test]
    fn resolve_with_path_answers_what_orb_behaviors_which_prints() {
        let spec = "std/behaviors/std-button";
        let value = from_json(serde_json::json!({ "behavior": spec })).unwrap();
        let options = from_json(serde_json::json!({ "path": true })).unwrap();
        let answer = ProgramHost.run("behavior/resolve", &[value, options]);
        match which(spec).expect("packages discover") {
            Ok((path, owner)) => assert_eq!(
                json(&answer.expect("resolves")),
                serde_json::json!({ "path": path.display().to_string(), "owner": owner })
            ),
            Err(refusal) => assert!(answer.unwrap_err().message.contains(&refusal)),
        }
    }

    #[test]
    fn resolve_with_expects_answers_what_orb_behaviors_expects_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = dir.path().join("note.orb");
        let program = ProgramHost.run("program/read", &[Value::String(NOTE.to_string().into())]).expect("reads");
        std::fs::write(&orb, json(&program).to_string()).unwrap();
        let spec = orb.display().to_string();
        let value = from_json(serde_json::json!({ "behavior": spec })).unwrap();
        let options = from_json(serde_json::json!({ "expects": true })).unwrap();
        let answer = json(&ProgramHost.run("behavior/resolve", &[value, options]).expect("derives"));
        assert_eq!(answer, reflect::expectations(&spec).unwrap());
        assert!(answer.get("NoteOrbital").is_some(), "{answer}");
    }

    #[test]
    fn control_resolve_with_path_refuses_a_value_naming_no_behavior() {
        let options = from_json(serde_json::json!({ "path": true })).unwrap();
        let error = ProgramHost.run("behavior/resolve", &[Value::Null, options]).unwrap_err();
        assert!(error.message.contains("naming its behavior"), "{}", error.message);
    }

    #[test]
    fn validate_with_a_report_answers_what_orb_validate_json_prints() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string() })).unwrap();
        let answer = json(&ProgramHost.run("program/validate", &[Value::Null, options]).expect("reports"));
        let expected = serde_json::to_value(crate::utils::validation::validation_report(&file, None, None, false, &[]).unwrap()).unwrap();
        assert_eq!(answer, expected);
        assert!(answer.get("valid").is_some(), "{answer}");
    }

    #[test]
    fn validate_with_findings_positions_each_lint_finding_like_a_compiler_finding() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let report = |findings: serde_json::Value| {
            let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string(), "findings": findings })).unwrap();
            ProgramHost.run("program/validate", &[Value::Null, options]).map(|v| json(&v))
        };
        let plain = report(serde_json::json!([])).expect("reports");
        let expected = serde_json::to_value(crate::utils::validation::validation_report(&file, None, None, false, &[]).unwrap()).unwrap();
        assert_eq!(plain, expected, "control: no findings leave the report as it is");
        let with = report(serde_json::json!([
            { "code": "ORB_LINT_E", "path": "orbitals[0].traits[0]", "message": "m", "suggestion": "", "severity": "error", "validValues": ["a", "b"] },
            { "code": "ORB_LINT_W", "path": "orbitals[0]", "message": "w", "suggestion": "s", "severity": "warning" }
        ]))
        .expect("reports");
        let error = with["errors"].as_array().unwrap().iter().find(|e| e["code"] == "ORB_LINT_E").expect("the lint error joins errors");
        assert!(error["line"].is_number() && error["column"].is_number(), "positioned: {error}");
        assert!(error.get("suggestion").is_none(), "an empty suggestion is left out: {error}");
        assert_eq!(error["validValues"], serde_json::json!(["a", "b"]), "the closed set travels with the finding");
        assert!(with["warnings"].as_array().unwrap().iter().all(|w| w.get("validValues").is_none()), "control: a finding without one carries none");
        assert_eq!(with["valid"], false, "an error-severity finding fails the report");
        let warning = with["warnings"].as_array().unwrap().iter().find(|w| w["code"] == "ORB_LINT_W").expect("the lint warning joins warnings");
        assert_eq!(warning["suggestion"], "s");
        assert!(warning["line"].is_number(), "positioned: {warning}");
        let refused = report(serde_json::json!([{ "code": "X", "path": "p", "message": "m", "severity": "fatal" }])).unwrap_err();
        assert!(refused.message.contains("`findings` must be a list"), "{}", refused.message);
    }

    #[test]
    fn a_finding_carries_its_structured_entity_and_field_into_the_report() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.lolo");
        std::fs::write(&file, NOTE).unwrap();
        let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string(), "findings": [
            { "code": "ORB_LINT_E", "path": "orbitals[0]", "message": "m", "suggestion": "", "severity": "error", "entity": "Other", "field": "otherId" },
            { "code": "ORB_LINT_W", "path": "orbitals[0]", "message": "w", "suggestion": "", "severity": "warning", "entity": "Other" },
            { "code": "ORB_LINT_P", "path": "orbitals[0]", "message": "p", "suggestion": "", "severity": "warning" }
        ] })).unwrap();
        let report = ProgramHost.run("program/validate", &[Value::Null, options]).map(|v| json(&v)).expect("reports");
        let error = report["errors"].as_array().unwrap().iter().find(|e| e["code"] == "ORB_LINT_E").expect("error");
        assert_eq!((error["entity"].as_str(), error["field"].as_str()), (Some("Other"), Some("otherId")));
        let warnings = report["warnings"].as_array().unwrap();
        let warned = warnings.iter().find(|w| w["code"] == "ORB_LINT_W").expect("warning");
        assert_eq!(warned["entity"], "Other");
        assert!(warned.get("field").is_none(), "unset members stay out: {warned}");
        let plain = warnings.iter().find(|w| w["code"] == "ORB_LINT_P").expect("control");
        assert!(plain.get("entity").is_none(), "control: a finding with no entity carries none: {plain}");
    }

    #[test]
    fn a_lint_error_keeps_the_compile_step_from_restating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("ghost.orb");
        std::fs::write(
            &file,
            serde_json::json!({ "name": "ghost", "version": "1.0.0", "orbitals": [{ "name": "Notes",
                "entity": { "name": "Note", "persistence": "runtime", "fields": [
                    { "name": "id", "type": "string", "required": true },
                    { "name": "owner", "type": "relation", "relation": { "entity": "Ghost", "cardinality": "one" } }
                ] }, "traits": [], "pages": [] }] })
            .to_string(),
        )
        .unwrap();
        let codes = |findings: serde_json::Value| {
            let options = from_json(serde_json::json!({ "report": true, "path": file.display().to_string(), "findings": findings })).unwrap();
            let report = json(&ProgramHost.run("program/validate", &[Value::Null, options]).expect("reports"));
            report["errors"].as_array().into_iter().flatten().map(|e| e["code"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>()
        };
        let relation = serde_json::json!({ "code": "ORB_E_INVALID_RELATION", "path": "orbitals[0].entity.fields[1].relation.entity", "message": "Relation target entity 'Ghost' does not exist", "severity": "error" });
        let with = codes(serde_json::json!([relation]));
        assert!(with.contains(&"ORB_E_INVALID_RELATION".to_string()), "{with:?}");
        assert!(!with.contains(&"COMPILATION_ERROR".to_string()), "the compile step only restates the lint error: {with:?}");
        let without = codes(serde_json::json!([]));
        assert!(without.contains(&"COMPILATION_ERROR".to_string()), "control: with no finding the compile step still reports: {without:?}");
        let warned = codes(serde_json::json!([{ "code": "ORB_LINT_W", "path": "orbitals[0]", "message": "w", "severity": "warning" }]));
        assert!(warned.contains(&"COMPILATION_ERROR".to_string()), "control: a warning does not hold the compile step back: {warned:?}");
    }

    #[test]
    fn control_validate_report_refuses_a_relative_path_and_an_unknown_option() {
        let relative = from_json(serde_json::json!({ "report": true, "path": "note.lolo" })).unwrap();
        assert!(ProgramHost.run("program/validate", &[Value::Null, relative]).unwrap_err().message.contains("absolute `path`"));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.lolo").display().to_string();
        let unknown = from_json(serde_json::json!({ "report": true, "path": path, "prior": {} })).unwrap();
        assert!(ProgramHost.run("program/validate", &[Value::Null, unknown]).unwrap_err().message.contains("unsupported option"));
    }

    fn print_with(options: serde_json::Value) -> Result<Value, SubstrateFailure> {
        ProgramHost.run("program/print", &[Value::Null, from_json(options).unwrap()])
    }

    #[test]
    fn print_of_source_keeps_its_comments_as_orb_format_does() {
        let commented = NOTE.replace("orbital NoteOrbital {", ";; the one note\norbital NoteOrbital {");
        let Value::String(text) = print_with(serde_json::json!({ "source": commented })).expect("prints") else {
            panic!("text")
        };
        assert!(text.contains(";; the one note"), "{text}");
        assert_eq!(text, formatted_text(&commented, "x").unwrap(), "the same bytes as orb format");
    }

    #[test]
    fn print_of_a_node_is_that_construct_and_nodes_is_every_match() {
        let Value::String(entity) = print_with(serde_json::json!({ "source": NOTE, "node": "orbitals[0].entity" })).expect("prints") else {
            panic!("text")
        };
        assert!(entity.starts_with("entity Note"), "{entity}");
        let fields = json(&print_with(serde_json::json!({ "source": NOTE, "nodes": "orbitals[0].entity.fields[*]" })).expect("prints"));
        assert_eq!(fields.as_array().map(Vec::len), Some(2), "{fields}");
    }

    #[test]
    fn print_refuses_a_node_without_source_a_wildcard_node_and_both_keys() {
        let message = |options| print_with(options).unwrap_err().message;
        assert!(message(serde_json::json!({ "node": "orbitals[0]" })).contains("missing"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[*]" })).contains("use `nodes`"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[0]", "nodes": "orbitals[*]" })).contains("not both"));
        assert!(message(serde_json::json!({ "source": NOTE, "node": "orbitals[3]" })).contains("out of range"));
    }

    #[test]
    fn validate_reports_the_validators_findings_and_a_bad_name_before_any_run() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
            .unwrap();
        let report = json(
            &host
                .run("program/validate", &[program.clone()])
                .expect("validates"),
        );
        assert!(
            report["errors"].is_array() && report["warnings"].is_array(),
            "{report}"
        );
        let mut renamed = json(&program);
        renamed["name"] = serde_json::json!("not a file name");
        let refused = json(
            &host
                .run("program/validate", &[from_json(renamed).unwrap()])
                .unwrap(),
        );
        assert_eq!(refused["valid"], false);
        assert_eq!(refused["errors"][0]["code"], "PROGRAM_NAME_INVALID");
    }

    #[test]
    fn the_behavior_ops_answer_what_the_behaviors_commands_print() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let source = json(
            &host
                .run("behavior/source", &[value.clone()])
                .expect("source"),
        );
        let printed = reflect::source("almadar-std/std-retry").unwrap();
        assert_eq!(source, json(&from_json(printed).unwrap()));
        let described = json(
            &host
                .run("behavior/describe", &[value.clone()])
                .expect("describe"),
        );
        assert_eq!(described["behavior"], "almadar-std/std-retry");
        let resolved = json(&host.run("behavior/resolve", &[value]).expect("resolve"));
        assert_eq!(resolved["name"], "std-retry");
        let missing =
            from_json(serde_json::json!({ "behavior": "almadar-std/no-such-behavior" })).unwrap();
        assert!(
            host.run("behavior/source", &[missing]).is_err(),
            "control: an unknown behavior fails"
        );
    }

    #[test]
    fn a_resolved_behavior_carries_the_client_metadata_a_browser_kernel_registers_it_with() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let resolved = json(&ProgramHost.run("behavior/resolve", &[value]).expect("resolve"));
        let metadata = &resolved["_metadata"];
        assert!(metadata["protocols"].as_array().is_some_and(|rows| !rows.is_empty()), "{metadata}");
        assert!(metadata.get("dispatchFacts").is_some(), "{metadata}");
        assert_eq!(
            metadata.as_object().unwrap().iter().filter(|(k, _)| ["operatorSites", "dispatchFacts", "protocols"].contains(&k.as_str())).count(),
            orbital_core::effects::runs_on::client_program_metadata(&resolved["orbitals"]).len(),
            "the same metadata the served client program carries"
        );
    }

    #[test]
    fn edge_a_resolved_program_keeps_the_metadata_it_already_carries() {
        let mut program = serde_json::json!({ "name": "P", "orbitals": [], "_metadata": { "provenance": "kept" } });
        with_client_metadata(&mut program);
        assert_eq!(program["_metadata"]["provenance"], "kept");
        assert!(program["_metadata"].get("protocols").is_some());
    }

    #[test]
    fn source_with_the_file_option_answers_the_path_and_file_text_as_read_behavior_does() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry" })).unwrap();
        let file = from_json(serde_json::json!({ "file": true })).unwrap();
        let answered =
            json(&ProgramHost::source("behavior/source", Some(&value), Some(&file)).expect("file"));
        let path =
            reflect::behavior_source_path("almadar-std/std-retry")
                .unwrap();
        assert_eq!(
            answered,
            serde_json::json!({ "path": path, "source": std::fs::read_to_string(&path).unwrap() })
        );
        assert!(answered["path"]
            .as_str()
            .unwrap()
            .ends_with("std-retry.lolo"));
        let program =
            json(&ProgramHost::source("behavior/source", Some(&value), None).expect("program"));
        assert_eq!(
            program,
            json(
                &from_json(
                    reflect::source("almadar-std/std-retry").unwrap()
                )
                .unwrap()
            ),
            "control: without the option the answer stays the program"
        );
        assert!(program.get("path").is_none());
    }

    #[test]
    fn describe_answers_a_trait_without_vectors_whatever_the_options() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let value = from_json(serde_json::json!({ "behavior": "almadar-std/std-retry", "trait": "Retry" })).unwrap();
        let plain = json(&ProgramHost.run("behavior/describe", &[value.clone()]).expect("describe"));
        let knobs = plain["knobs"].as_array().unwrap();
        assert_eq!(knobs.len(), 4, "{plain}");
        assert!(knobs.iter().all(|k| k.get("vector").is_none()));
        assert_eq!(plain["model"], "");
        let options = from_json(serde_json::json!({ "emit": { "success": "DESCRIBED", "failure": "DESCRIBE_FAILED" } })).unwrap();
        assert_eq!(json(&ProgramHost.run("behavior/describe", &[value, options]).unwrap()), plain, "control: the emit options do not change the answer");
    }

    #[test]
    fn catalog_lists_rows_with_no_vectors() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let scope = from_json(serde_json::json!({ "paths": ["almadar-std/ml/organisms"] })).unwrap();
        let bare = json(&ProgramHost::catalog(Some(&scope), None).expect("catalog"));
        let entries = bare.as_array().unwrap();
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e["vectors"] == serde_json::json!({}) && e["model"] == "" && e["traits"].is_array()));
        let keys: Vec<&str> = entries.iter().map(|e| e["behavior"].as_str().unwrap()).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "rows come in `<prefix>/<name>` order: {keys:?}");

        let lean = from_json(serde_json::json!({ "traits": false })).unwrap();
        let lean = json(&ProgramHost::catalog(Some(&scope), Some(&lean)).unwrap());
        assert!(lean.as_array().unwrap().iter().all(|e| e.get("traits").is_none()));

        let none = from_json(serde_json::json!({ "exposure": [] })).unwrap();
        assert!(ProgramHost::catalog(Some(&none), None).is_err(), "control: no paths fails");
        let bad = from_json(serde_json::json!({ "paths": ["almadar-std/ml/organisms"], "exposure": ["nope"] })).unwrap();
        assert!(ProgramHost::catalog(Some(&bad), None).is_err(), "control: an unknown exposure fails");
    }

    #[test]
    fn a_listing_without_traits_reads_the_same_rows_from_headers_as_from_programs() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let scope = from_json(serde_json::json!({ "paths": ["almadar-std/ml/organisms", "almadar-std/dev/atoms"] })).unwrap();
        let header = json(&ProgramHost::catalog(Some(&scope), Some(&from_json(serde_json::json!({ "traits": false })).unwrap())).unwrap());
        let program = json(&ProgramHost::catalog(Some(&scope), None).unwrap());
        let facts = |rows: &serde_json::Value| -> Vec<serde_json::Value> {
            rows.as_array().unwrap().iter().map(|e| serde_json::json!([e["behavior"], e["package"], e["topic"], e["level"], e["exposure"], e["description"], e["capabilities"], e["pattern"]])).collect()
        };
        assert!(!facts(&header).is_empty());
        assert_eq!(facts(&header), facts(&program), "header rows and program rows agree on every path and header fact");
    }

    #[test]
    fn control_a_header_row_never_lowers_its_program() {
        let dir = tempfile::tempdir().unwrap();
        let lolo = dir.path().join("broken-body.lolo");
        std::fs::write(&lolo, "app broken-body [\n  version: \"1.0.0\",\n  description: \"Listed from its header.\"\n]\norbital BrokenBody {\n  this body does not parse\n}\n").unwrap();
        let text = std::fs::read_to_string(&lolo).unwrap();
        let header = orbital_lolo::parse_app_header(&text).expect("the header parses although the body does not");
        assert_eq!(header.description.as_deref(), Some("Listed from its header."));
        assert!(orbital_lolo::parse_str(&text).is_err(), "control: the whole program does not parse");
    }

    #[test]
    fn behavior_texts_is_not_an_operator_the_host_answers() {
        let scope = from_json(serde_json::json!({ "paths": ["almadar-std/ml/organisms"] })).unwrap();
        let unknown = ProgramHost.run("behavior/texts", &[scope]).unwrap_err();
        assert!(unknown.message.contains("behavior/texts"), "{}", unknown.message);
    }

    #[test]
    fn packages_lists_each_installed_package_with_its_resolved_vectors() {
        let listed = json(&ProgramHost::packages().expect("packages"));
        let cwd = std::env::current_dir().unwrap();
        let installed = discover_packages(&cwd).unwrap();
        let entries = listed.as_array().unwrap();
        assert_eq!(entries.len(), installed.len());
        assert!(!entries.is_empty());
        for (entry, pkg) in entries.iter().zip(&installed) {
            assert_eq!(entry["prefix"], pkg.prefix.as_str());
            assert_eq!(entry["root"], pkg.root.to_string_lossy().as_ref());
            let vectors = resolve_vectors(
                pkg,
                &UreqFetcher,
            )
            .unwrap();
            match vectors {
                Some(path) => {
                    assert_eq!(entry["vectors"], path.to_string_lossy().as_ref());
                    let kind = entry["vectorsIn"].as_str().unwrap();
                    assert!(["node-modules", "sidecars", "outside"].contains(&kind));
                    assert_eq!(entry["vectorsRel"].is_null(), kind == "outside");
                }
                None => assert!(
                    entry["vectors"].is_null() && entry["vectorsRel"].is_null() && entry["vectorsIn"].is_null(),
                    "control: no orb.vectors is null"
                ),
            }
        }
        assert!(entries.iter().any(|e| e["prefix"] == "almadar-std"));
    }

    /// A core providing language 1.0.0 in `node_modules`: discovery reads only its language version.
    fn write_core(node_modules: &Path) {
        let dir = node_modules.join("@almadar/core");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({ "name": "@almadar/core", "version": "10.1.0", "orbLanguage": { "version": "1.0.0" } });
        std::fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
    }

    fn write_package(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join("behaviors/registry")).unwrap();
        std::fs::write(dir.join("behaviors/registry/pa-x.orb"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("dist/vectors.vec"), "x").unwrap();
        let manifest = serde_json::json!({
            "name": name, "version": "1.0.0",
            "orb": { "prefix": "pa", "registry": "behaviors/registry", "vectors": "dist/vectors.vec", "language": "^1" },
        });
        std::fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
    }

    #[test]
    fn packages_answer_each_snapshot_relative_to_a_fixed_anchor() {
        use orbital_core::behavior_packages::discover_with_cache;
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let cache = tmp.path().join("cache");
        let installed = tmp.path().join("node_modules/@fake/pkg-a");
        write_core(&tmp.path().join("node_modules"));
        write_package(&installed, "@fake/pkg-a");
        let found = discover_with_cache(tmp.path(), &store, &cache).unwrap();
        let vectors = found[0].vectors.clone().unwrap();
        let place = vectors_place(&vectors, tmp.path(), &cache);
        assert_eq!((place.kind, place.rel.as_deref()), ("node-modules", Some("@fake/pkg-a/dist/vectors.vec")));

        let below = tmp.path().join("deeper");
        std::fs::create_dir_all(&below).unwrap();
        let higher = vectors_place(&vectors, &below, &cache);
        assert_eq!((higher.kind, higher.rel), ("outside", None), "control: a node_modules higher up the ancestors");

        let elsewhere = vectors_place(Path::new("/nowhere/orb.vectors"), tmp.path(), &cache);
        assert_eq!((elsewhere.kind, elsewhere.rel), ("outside", None), "control: no anchor holds it");
    }

    #[test]
    fn the_default_sidecar_store_is_an_anchor_and_a_custom_cache_is_outside() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
        let store = home.join(".almadar/sidecars");
        let cached = vectors_place(&store.join("abc123"), Path::new("/nowhere"), &store);
        assert_eq!((cached.kind, cached.rel.as_deref()), ("sidecars", Some("abc123")));
        let tmp = tempfile::tempdir().unwrap();
        let custom = vectors_place(&tmp.path().join("abc123"), Path::new("/nowhere"), tmp.path());
        assert_eq!((custom.kind, custom.rel), ("outside", None), "control: a cache that is not the default");
    }

    #[test]
    fn a_linked_package_whose_real_path_is_outside_the_anchor_is_outside() {
        use orbital_core::behavior_packages::discover_with_cache;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("workspace/pkg-b");
        write_package(&real, "pkg-b");
        std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        write_core(&tmp.path().join("node_modules"));
        std::os::unix::fs::symlink(&real, tmp.path().join("node_modules/pkg-b")).unwrap();
        let found = discover_with_cache(tmp.path(), &tmp.path().join("store"), &tmp.path().join("cache")).unwrap();
        let place = vectors_place(found[0].vectors.as_deref().unwrap(), tmp.path(), &tmp.path().join("cache"));
        assert_eq!((place.kind, place.rel), ("outside", None));
    }

    #[test]
    fn program_compose_assembles_an_orbital_value_into_an_app_carrying_its_hooks() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "Drop", "orbitals": [{ "behavior": "almadar-std/std-file-drop", "orbital": "FileDropOrbital" }] })).unwrap();
        let options =
            from_json(serde_json::json!({ "appName": "DropApp", "theme": "ocean" })).unwrap();
        let schema = json(
            &host
                .run(
                    "program/compose",
                    &[Value::Array(vec![program.clone()].into()), options],
                )
                .expect("compose"),
        );
        assert_eq!(schema["name"], "DropApp");
        assert_eq!(schema["theme"], "ocean");
        assert_eq!(
            schema["inbound"],
            serde_json::json!(["file-drop"]),
            "the imported behavior's hook joins the app"
        );
        assert_eq!(
            schema["orbitals"][0]["reference"]["ref"],
            "Source.orbitals.FileDropOrbital"
        );
        let bad = from_json(serde_json::json!({ "appName": "X", "layout": "grid" })).unwrap();
        assert!(
            host.run("program/compose", &[Value::Array(vec![program].into()), bad])
                .unwrap_err()
                .message
                .contains("layout must be one of"),
            "control: an unknown layout is refused"
        );
    }

    #[test]
    fn program_compose_runs_exactly_the_options_the_operator_table_declares() {
        use orbital_core::language::registry::{get_operator, OperatorTypeRef, OperatorTypeRefKind};
        let options = &get_operator("program/compose").expect("table entry").params[1].ty;
        let OperatorTypeRef::Tagged(OperatorTypeRefKind::Object { fields, .. }) = options else {
            panic!("program/compose options are an object");
        };
        let mut declared: Vec<&str> = fields.keys().map(String::as_str).collect();
        declared.sort_unstable();
        let mut run = COMPOSE_OPTION_KEYS.to_vec();
        run.sort_unstable();
        assert_eq!(declared, run);
        let bad = from_json(serde_json::json!({ "appName": "X", "bogus": "y" })).unwrap();
        let refused = ProgramHost.run("program/compose", &[Value::Array(Vec::new().into()), bad]).unwrap_err();
        assert!(refused.message.contains("unsupported option \"bogus\""), "control: {}", refused.message);
    }

    #[test]
    fn eval_writes_a_valid_program_into_the_project_and_refuses_an_invalid_one() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = json(
            &host
                .run("program/read", &[Value::String(NOTE.to_string().into())])
                .unwrap(),
        );
        let project = std::env::temp_dir().join(format!("orb-eval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let out = json(
            &host
                .eval_in(&project, program.clone(), None)
                .expect("a valid program evaluates"),
        );
        assert_eq!(out["behavior"], "./orbitals/note-pad");
        assert_eq!(
            out["traits"],
            serde_json::json!([{ "behavior": "./orbitals/note-pad", "trait": "NoteView" }])
        );
        assert!(
            project.join("orbitals/note-pad.orb").is_file(),
            "the program is written into the project"
        );
        let mut broken = program.clone();
        broken["orbitals"][0]["traits"][0]["stateMachine"]["states"] = serde_json::json!([]);
        let refused = host.eval_in(&project, broken, None).unwrap_err();
        assert!(
            refused.message.starts_with("program/eval: ") && !refused.errors.is_empty(),
            "control: an invalid program is refused with its findings: {}",
            refused.message
        );
    }

    #[test]
    fn step_plays_a_transition_and_dependencies_list_what_the_program_imports() {
        orbital_compiler::loader::register_source_lowerer(|path, _, _| crate::lower_lolo_file(path));
        let host = ProgramHost;
        let program = host
            .run("program/read", &[Value::String(NOTE.to_string().into())])
            .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "NoteOrbital", "trait": "NoteView", "from": "idle", "event": "INIT", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program.clone(), requests])
                .expect("steps"),
        );
        assert_eq!(played[0]["result"]["transitionFired"], true);
        assert_eq!(
            played[0]["result"]["state"],
            serde_json::json!({ "before": "idle", "after": "idle" })
        );
        let bad = from_json(serde_json::json!([{ "trait": "NoteView" }])).unwrap();
        assert!(
            host.run("program/step", &[program, bad])
                .unwrap_err()
                .message
                .contains("needs a orbital"),
            "control: a request without its orbital is refused"
        );
        let importer = from_json(serde_json::json!({ "name": "Uses", "orbitals": [{ "name": "R", "uses": [{ "from": "std/behaviors/std-retry", "as": "Retry" }], "traits": [], "pages": [] }] })).unwrap();
        let graph = json(
            &host
                .run("behavior/dependencies", &[importer])
                .expect("graph"),
        );
        assert_eq!(graph["orbitals"]["R"], serde_json::json!(["std-retry"]));
        assert_eq!(graph["behaviors"]["std-retry"]["layer"], "std");
        let alone = from_json(serde_json::json!({ "name": "Alone", "orbitals": [{ "name": "A", "traits": [], "pages": [] }] })).unwrap();
        let graph = json(&host.run("behavior/dependencies", &[alone]).expect("graph"));
        assert_eq!(
            graph,
            serde_json::json!({ "orbitals": { "A": [] }, "behaviors": {} }),
            "control: a program with no uses lists none"
        );
    }

    #[test]
    fn step_reports_each_ensures_judged_by_the_kernel_and_whether_a_failure_was_routed() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "CounterApp", "orbitals": [{ "name": "Counting",
            "entity": { "name": "Counter", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "count", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "Count", "linkedEntity": "Counter", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }],
                "transitions": [
                    { "from": "ready", "to": "ready", "event": "INC",
                      "effects": [["set", "@entity.count", ["+", "@entity.count", 1]]],
                      "ensures": [["=", "@entity.count", ["+", "@fromEntity.count", 1]], ["=", "@entity.count", 99]] },
                    { "from": "ready", "to": "ready", "event": "GATED", "guard": ["=", 1, 2],
                      "ensures": [["=", "@entity.count", 0]] }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([
            { "orbital": "Counting", "trait": "Count", "from": "ready", "event": "INC", "payload": {} },
            { "orbital": "Counting", "trait": "Count", "from": "ready", "event": "GATED", "payload": {} }
        ]))
        .unwrap();
        let played = json(
            &host
                .run("program/step", &[program.clone(), requests.clone()])
                .expect("steps"),
        );
        let inc = &played[0]["result"];
        assert_eq!(
            inc["effects"][0]["routed"], false,
            "an executed set routes nothing"
        );
        assert_eq!(inc["pendingAsks"], serde_json::json!([]), "no ask left pending: {inc}");
        assert_eq!(inc["ensures"][0]["verdict"], "held", "{inc}");
        assert_eq!(inc["ensures"][1]["verdict"], "broken");
        assert_eq!(
            inc["ensures"][1]["proposition"],
            serde_json::json!(["=", "@entity.count", 99])
        );
        assert_eq!(inc["ensures"][1]["counterexample"]["entity"]["count"], 1);
        assert_eq!(
            inc["ensures"][1]["counterexample"]["fromEntity"]["count"],
            0
        );
        let gated = &played[1]["result"];
        assert_eq!(gated["transitionFired"], false);
        assert_eq!(gated["ensures"][0]["verdict"], "inconclusive", "{gated}");
        assert_eq!(gated["ensures"][0]["reason"], "the transition did not fire");
        assert!(
            serde_json::from_value::<orbital_core::program_machine::PlayedStep>(played[0].clone())
                .is_ok(),
            "the host answers the typed PlayedStep wire"
        );
        assert!(
            inc.get("rendered").is_none(),
            "control: no render facts unless options.render asks"
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    requests,
                    from_json(serde_json::json!({ "render": "yes" })).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("options.render must be a boolean"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn the_render_probe_resolves_the_program_with_the_steps_config_knobs() {
        let scratch = Scratch::new("almadar-step-config").unwrap();
        let program = serde_json::json!({ "name": "KnobApp", "typeContractVersion": 1, "orbitals": [{ "name": "Knobs",
            "entity": { "name": "Box", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "K", "linkedEntity": "Box", "config": { "limit": { "type": "number", "default": 1 } },
                "stateMachine": { "states": [{ "name": "ready", "isInitial": true }],
                    "transitions": [{ "from": "ready", "to": "ready", "event": "GO", "effects": [["set", "@entity.n", "@config.limit"]] }] } }] }] });
        let path = scratch.write("program.orb", &program.to_string()).unwrap();
        let effect = |steps: serde_json::Value| {
            let schema = resolve_with_step_config(&path, steps.as_array().unwrap()).unwrap();
            serde_json::to_value(&schema).unwrap()["orbitals"][0]["traits"][0]["stateMachine"]["transitions"][0]["effects"].to_string()
        };
        let seeded = effect(serde_json::json!([{ "trait": "K", "config": { "limit": 7 } }]));
        let default = effect(serde_json::json!([{ "trait": "K" }]));
        assert!(seeded.contains('7') && !default.contains('7'), "{seeded} vs {default}");
        assert!(
            resolve_with_step_config(&path, &[serde_json::json!({ "trait": "K", "config": { "nope": 1 } })]).is_err(),
            "edge: an undeclared knob is refused"
        );
    }

    #[test]
    fn step_refuses_render_props_that_is_not_a_boolean_or_has_no_render_facts_to_extend() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "CounterApp", "orbitals": [{ "name": "Counting",
            "entity": { "name": "Counter", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "Count", "linkedEntity": "Counter", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }],
                "transitions": [{ "from": "ready", "to": "ready", "event": "INC", "effects": [] }] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([
            { "orbital": "Counting", "trait": "Count", "from": "ready", "event": "INC", "payload": {} }
        ]))
        .unwrap();
        let run = |options: serde_json::Value| {
            host.run("program/step", &[program.clone(), requests.clone(), from_json(options).unwrap()])
        };
        assert!(run(serde_json::json!({ "renderProps": "yes", "render": true }))
            .unwrap_err()
            .message
            .contains("options.renderProps must be a boolean"));
        assert!(run(serde_json::json!({ "renderProps": true }))
            .unwrap_err()
            .message
            .contains("needs options.render: true"));
        assert!(
            run(serde_json::json!({ "renderProps": false })).is_ok(),
            "control: renderProps false asks nothing extra"
        );
    }

    #[test]
    fn step_seeds_the_entity_and_pins_now_for_that_request_only() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "GateApp", "orbitals": [{ "name": "Gating",
            "entity": { "name": "Gate", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "owner", "type": "string", "default": "" }] },
            "pages": [],
            "traits": [{ "name": "G", "linkedEntity": "Gate", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }, { "name": "open" }],
                "transitions": [
                    { "from": "ready", "to": "open", "event": "OPEN", "guard": ["=", "@entity.owner", "ann"] },
                    { "from": "ready", "to": "open", "event": "LATE", "guard": ["time/isAfter", "@now", "1970-01-01T00:00:01.000Z"] }
                ] } }] }] }))
        .unwrap();
        let step = |extra: serde_json::Value| {
            let mut r = serde_json::json!({ "orbital": "Gating", "trait": "G", "from": "ready", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            r
        };
        let requests = from_json(serde_json::json!([
            step(serde_json::json!({ "event": "OPEN", "entity": { "owner": "ann" } })),
            step(serde_json::json!({ "event": "OPEN", "entity": { "owner": "bob" } })),
            step(serde_json::json!({ "event": "LATE", "now": 0 })),
            step(serde_json::json!({ "event": "LATE" }))
        ]))
        .unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "a seeded owner admits the guard: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "fail",
            "control: another owner does not"
        );
        assert_eq!(
            played[2]["result"]["guard"], "fail",
            "a pinned @now of 0 is not past one second after the epoch: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the real clock is"
        );
        assert_eq!(
            played[0]["entity"],
            serde_json::json!({ "owner": "ann" }),
            "the request is answered as asked"
        );
    }

    #[test]
    fn step_reports_each_cascaded_arm_with_its_own_effects() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RelayApp",
            "osGrants": { "fs": [], "exec": [], "net": [], "env": ["HOME"], "stdio": [], "sys": [], "desktop": [] },
            "orbitals": [{ "name": "Auth",
            "entity": { "name": "Key", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "ApiKeys", "linkedEntity": "Key", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "ISSUE", "effects": [["emit", "ISSUED", {}]] }] } },
                { "name": "ServerBearer", "linkedEntity": "Key",
                  "listens": [{ "event": "ISSUED", "triggers": "ARM", "source": { "kind": "trait", "trait": "ApiKeys" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "armed" }],
                    "transitions": [
                        { "from": "idle", "to": "armed", "event": "ARM", "effects": [["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                        { "from": "armed", "to": "armed", "event": "GOT" }, { "from": "armed", "to": "idle", "event": "NOGO" }
                    ] } }
            ] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Auth", "trait": "ApiKeys", "from": "idle", "event": "ISSUE", "payload": {} }])).unwrap();
        let played = json(&host.run("program/step", &[program, requests]).expect("steps"));
        let r = &played[0]["result"];
        assert_eq!(r["outcomes"][1]["effects"][0]["type"], "os/getenv", "the cascaded arm reports its own effect: {r}");
        assert_eq!(r["outcomes"][0]["effects"], r["effects"], "control: the requested trait's entry carries the top-level effects");
        assert_eq!(r["effects"].as_array().map(Vec::len), Some(1), "the top-level effects stay the requested arm's only: {r}");
    }

    #[test]
    fn step_lists_every_trait_the_dispatch_cascade_moved_in_order() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RelayApp", "orbitals": [{ "name": "Auth",
            "entity": { "name": "Key", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "ApiKeys", "linkedEntity": "Key", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }],
                    "transitions": [{ "from": "idle", "to": "idle", "event": "ISSUE", "effects": [["emit", "ISSUED", {}]] }] } },
                { "name": "ServerBearer", "linkedEntity": "Key",
                  "listens": [{ "event": "ISSUED", "triggers": "ARM", "source": { "kind": "trait", "trait": "ApiKeys" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "armed" }],
                    "transitions": [{ "from": "idle", "to": "armed", "event": "ARM" }] } }
            ] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Auth", "trait": "ApiKeys", "from": "idle", "event": "ISSUE", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let outcomes = &played[0]["result"]["outcomes"];
        assert_eq!(outcomes.as_array().map(Vec::len), Some(2), "{outcomes}");
        assert_eq!(outcomes[0]["orbital"], "Auth");
        assert_eq!(outcomes[0]["trait"], "ApiKeys");
        assert_eq!(outcomes[0]["emitted"][0]["type"], "ISSUED");
        assert_eq!(outcomes[0]["event"], "ISSUE");
        assert_eq!(outcomes[0]["arm"], 0);
        assert_eq!(outcomes[1]["trait"], "ServerBearer");
        assert_eq!(
            outcomes[1]["event"], "ARM",
            "the listener's own event: {outcomes}"
        );
        assert_eq!(outcomes[1]["fired"], true);
        assert_eq!(outcomes[1]["arm"], 0);
        assert_eq!(
            (&outcomes[1]["from"], &outcomes[1]["to"]),
            (&serde_json::json!("idle"), &serde_json::json!("armed"))
        );
    }

    #[test]
    fn control_a_single_trait_step_has_one_outcome_equal_to_its_top_level_fields() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EditorApp", "orbitals": [{ "name": "Editor",
            "entity": { "name": "Doc", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "Modes", "linkedEntity": "Doc", "stateMachine": {
                "states": [{ "name": "NORMAL", "isInitial": true }, { "name": "INSERT" }],
                "transitions": [{ "from": "NORMAL", "to": "INSERT", "event": "KEY", "guard": ["=", "@payload.key", "i"] }] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Editor", "trait": "Modes", "from": "NORMAL", "event": "KEY", "payload": { "key": "i" } }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(
            r["outcomes"],
            serde_json::json!([{ "orbital": "Editor", "trait": r["trait"], "event": r["event"], "from": r["state"]["before"], "to": r["state"]["after"], "arm": r["firedArm"], "fired": r["transitionFired"], "effects": r["effects"], "emitted": r["emitted"] }])
        );
    }

    #[test]
    fn step_carries_an_array_of_objects_and_bytes_to_the_next_request() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "CarryApp", "orbitals": [{ "name": "Carry",
            "entity": { "name": "Box", "persistence": "runtime", "fields": [
                { "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 },
                { "name": "body", "type": "bytes" },
                { "name": "items", "type": "array", "default": [], "items": { "type": "object", "properties": { "name": { "name": "name", "type": "string" } } } }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Box", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "done" }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "PUT", "effects": [
                        ["set", "@entity.items", ["array/append", "@entity.items", { "name": "x" }]],
                        ["set", "@entity.body", ["bytes/from-string", "hi"]],
                        ["set", "@entity.n", 1]] },
                    { "from": "idle", "to": "done", "event": "HAS_ITEM", "guard": [">", ["array/len", "@entity.items"], 0] },
                    { "from": "idle", "to": "done", "event": "HAS_BODY", "guard": ["=", ["bytes/to-string", "@entity.body"], "hi"] },
                    { "from": "idle", "to": "done", "event": "HAS_N", "guard": ["=", "@entity.n", 1] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Carry", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([
                            req("PUT"),
                            req("HAS_ITEM"),
                            req("HAS_BODY"),
                            req("HAS_N")
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[1]["result"]["guard"], "pass",
            "the appended object is there: {}",
            played[1]
        );
        assert_eq!(
            played[2]["result"]["guard"], "pass",
            "the bytes read back: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the scalar carries"
        );
    }

    #[test]
    fn step_seeds_nested_arrays_of_objects_and_keeps_the_unseeded_siblings() {
        let host = ProgramHost;
        let obj =
            |props: serde_json::Value| serde_json::json!({ "type": "object", "properties": props });
        let program = from_json(serde_json::json!({ "name": "JudgeApp", "orbitals": [{ "name": "Judge",
            "entity": { "name": "Run", "persistence": "runtime", "fields": [
                { "name": "id", "type": "string" },
                { "name": "played", "type": "array", "default": [], "items": obj(serde_json::json!({ "id": { "name": "id", "type": "string" }, "targets": { "name": "targets", "type": "array", "items": obj(serde_json::json!({ "t": { "name": "t", "type": "string" } })) } })) },
                { "name": "judged", "type": "array", "default": [], "items": obj(serde_json::json!({ "id": { "name": "id", "type": "string" }, "verdicts": { "name": "verdicts", "type": "array", "items": obj(serde_json::json!({ "v": { "name": "v", "type": "string" } })) } })) }] },
            "pages": [],
            "traits": [{ "name": "J", "linkedEntity": "Run", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "done" }],
                "transitions": [
                    { "from": "idle", "to": "done", "event": "TARGETS", "guard": ["=", ["object/get", ["array/nth", ["object/get", ["array/nth", "@entity.played", 0], "targets"], 0], "t"], "a"] },
                    { "from": "idle", "to": "done", "event": "VERDICTS", "guard": ["=", ["object/get", ["array/nth", ["object/get", ["array/nth", "@entity.judged", 0], "verdicts"], 0], "v"], "ok"] },
                    { "from": "idle", "to": "done", "event": "NO_JUDGED", "guard": ["=", ["array/len", "@entity.judged"], 0] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str, entity: serde_json::Value| serde_json::json!({ "orbital": "Judge", "trait": "J", "from": "idle", "event": event, "payload": {}, "entity": entity });
        let full = serde_json::json!({ "played": [{ "id": "p", "targets": [{ "t": "a" }] }], "judged": [{ "id": "j", "verdicts": [{ "v": "ok" }] }] });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([
                            req("TARGETS", full.clone()),
                            req("VERDICTS", full),
                            req("TARGETS", serde_json::json!({ "judged": [] })),
                            req("NO_JUDGED", serde_json::json!({ "judged": [] }))
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "nested targets read back: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "pass",
            "nested verdicts read back: {}",
            played[1]
        );
        assert_eq!(
            played[2]["result"]["guard"], "pass",
            "seeding judged keeps played: {}",
            played[2]
        );
        assert_eq!(
            played[3]["result"]["guard"], "pass",
            "control: the seeded field itself is replaced"
        );
    }

    #[test]
    fn step_deletes_a_row_an_earlier_request_of_the_same_call_created() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "PApp", "orbitals": [{ "name": "P",
            "entity": { "name": "Ticket", "persistence": "persistent", "collection": "tickets", "fields": [{ "name": "id", "type": "string" }, { "name": "v", "type": "string", "default": "" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Ticket", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "MAKE", "effects": [["persist", "create", "Ticket", { "id": "tk1", "v": "a" }, { "emit": { "success": "MADE", "failure": "MAKE_FAILED" } }]] },
                    { "from": "idle", "to": "idle", "event": "DROP", "effects": [["persist", "delete", "Ticket", "tk1", { "emit": { "success": "DROPPED", "failure": "DROP_FAILED" } }]] },
                    { "from": "idle", "to": "idle", "event": "MADE" }, { "from": "idle", "to": "idle", "event": "MAKE_FAILED" },
                    { "from": "idle", "to": "idle", "event": "DROPPED" }, { "from": "idle", "to": "idle", "event": "DROP_FAILED" }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "P", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let both = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req("MAKE"), req("DROP")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            both[1]["result"]["emitted"][0]["type"], "DROPPED",
            "{}",
            both[1]
        );
        let alone = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("DROP")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            alone[0]["result"]["emitted"][0]["type"], "DROP_FAILED",
            "control: no row, not found"
        );
    }

    #[test]
    fn step_reports_the_evaluated_emit_payload() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EmApp", "orbitals": [{ "name": "Em",
            "entity": { "name": "Req", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Req", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [{ "from": "idle", "to": "idle", "event": "GO", "effects": [
                    ["let", [["big", [">", "@payload.n", 10]]],
                        ["emit", "RESPOND", { "status": ["if", "@big", 413, 200], "lit": 7 }]]] }] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Em", "trait": "T", "from": "idle", "event": "GO", "payload": { "n": 3 } }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(r["emitted"][0]["payload"]["status"], 200.0, "{r}");
        assert_eq!(r["emitted"][0]["payload"]["lit"], 7.0, "control: a literal");
        assert_eq!(r["outcomes"][0]["emitted"], r["emitted"]);
    }

    #[test]
    fn step_overrides_a_config_knob_for_the_call() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "KnobApp", "orbitals": [{ "name": "Knob",
            "entity": { "name": "K", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [
                { "name": "T", "linkedEntity": "K",
                  "config": { "mode": { "type": "string", "default": "off" } },
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "on" }],
                    "transitions": [
                        { "from": "idle", "to": "on", "event": "GO", "guard": ["=", "@config.mode", "on"], "effects": [["emit", "WENT", {}]] }
                    ] } },
                { "name": "L", "linkedEntity": "K",
                  "config": { "mode": { "type": "string", "default": "off" } },
                  "listens": [{ "event": "WENT", "triggers": "HEARD", "source": { "kind": "trait", "trait": "T" } }],
                  "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "heard" }],
                    "transitions": [{ "from": "idle", "to": "heard", "event": "HEARD", "guard": ["=", "@config.mode", "off"] }] } }
            ] }] }))
        .unwrap();
        let req = |config: Option<serde_json::Value>| {
            let mut r = serde_json::json!({ "orbital": "Knob", "trait": "T", "from": "idle", "event": "GO", "payload": {} });
            if let Some(c) = config {
                r["config"] = c;
            }
            r
        };
        let on = || Some(serde_json::json!({ "mode": "on" }));
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req(on()), req(None)])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "the overridden knob admits the guard: {}",
            played[0]
        );
        assert_eq!(
            played[0]["result"]["outcomes"][1]["fired"], true,
            "the override is the requested trait's only, a listener keeps its default: {}",
            played[0]
        );
        let default = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([req(None)])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            default[0]["result"]["guard"], "fail",
            "control: a call that overrides nothing reads the default"
        );
        let split = host
            .run(
                "program/step",
                &[
                    program.clone(),
                    from_json(serde_json::json!([
                        req(on()),
                        req(Some(serde_json::json!({ "mode": "off" })))
                    ]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            split.message.contains("one call takes one value per knob"),
            "{}",
            split.message
        );
        let unknown = host
            .run(
                "program/step",
                &[
                    program.clone(),
                    from_json(serde_json::json!([req(Some(
                        serde_json::json!({ "nope": 1 })
                    ))]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            unknown.message.contains("declares no knob `nope`"),
            "{}",
            unknown.message
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    from_json(serde_json::json!([req(Some(serde_json::json!("on")))])).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("config must be an object"),
            "{}",
            bad.message
        );
    }

    #[test]
    fn step_runs_an_os_effect_only_when_the_call_is_live() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "LiveApp",
            "osGrants": { "fs": [], "exec": [], "net": [], "env": ["HOME"], "stdio": [], "sys": [], "desktop": [] },
            "orbitals": [{ "name": "Live",
            "entity": { "name": "L", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "L", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "asking" }, { "name": "got" }],
                "transitions": [
                    { "from": "idle", "to": "asking", "event": "ASK", "effects": [
                        ["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                    { "from": "asking", "to": "got", "event": "GOT" },
                    { "from": "asking", "to": "idle", "event": "NOGO" }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Live", "trait": "T", "from": "idle", "event": "ASK", "payload": {} }])).unwrap();
        let live = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        requests.clone(),
                        from_json(serde_json::json!({ "live": true })).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        let events: Vec<&str> = live[0]["result"]["outcomes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o["event"].as_str())
            .collect();
        assert_eq!(
            events,
            vec!["ASK", "GOT"],
            "live: the host answers getenv and its success route runs: {}",
            live[0]
        );
        let replay = json(
            &host
                .run("program/step", &[program.clone(), requests.clone()])
                .expect("steps"),
        );
        assert_eq!(
            replay[0]["result"]["state"]["after"], "asking",
            "control: without live the ask stays pending"
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program,
                    requests,
                    from_json(serde_json::json!({ "live": "yes" })).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("options.live must be a boolean"),
            "{}",
            bad.message
        );
    }

    fn read_file_program(path: &str) -> Value {
        from_json(serde_json::json!({ "name": "LiveRead",
            "osGrants": { "fs": [{ "glob": path, "modes": ["read"] }], "exec": [], "net": [], "env": [], "stdio": [], "sys": [], "desktop": [] },
            "orbitals": [{ "name": "Live",
            "entity": { "name": "L", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "L", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "asking" }, { "name": "got" }, { "name": "failed" }],
                "transitions": [
                    { "from": "idle", "to": "asking", "event": "ASK", "effects": [
                        ["os/read-file", path, { "key": "r", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                    { "from": "asking", "to": "got", "event": "GOT" },
                    { "from": "asking", "to": "failed", "event": "NOGO" }
                ] } }] }] }))
        .unwrap()
    }

    fn live_read(path: &str, options: serde_json::Value) -> serde_json::Value {
        let requests = from_json(serde_json::json!([{ "orbital": "Live", "trait": "T", "from": "idle", "event": "ASK", "payload": {} }])).unwrap();
        json(
            &ProgramHost
                .run("program/step", &[read_file_program(path), requests, from_json(options).unwrap()])
                .expect("steps"),
        )
    }

    #[test]
    fn step_live_read_file_delivers_the_success_route() {
        let dir = std::env::temp_dir().join(format!("almadar-live-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let path = file.display().to_string();
        let live = live_read(&path, serde_json::json!({ "live": true }));
        let events: Vec<&str> = live[0]["result"]["outcomes"].as_array().into_iter().flatten().filter_map(|o| o["event"].as_str()).collect();
        assert_eq!(events, vec!["ASK", "GOT"], "{}", live[0]);
        assert_eq!(live[0]["result"]["outcomes"][1]["to"], "got", "{}", live[0]);
        assert_eq!(live[0]["result"]["outcomes"][0]["effects"][0]["status"], "executed", "{}", live[0]);
        let replay = live_read(&path, serde_json::json!({}));
        assert_eq!(replay[0]["result"]["state"]["after"], "asking", "control: without live the ask stays pending");
        let missing = dir.join("missing.txt").display().to_string();
        let miss = live_read(&missing, serde_json::json!({ "live": true }));
        let events: Vec<&str> = miss[0]["result"]["outcomes"].as_array().into_iter().flatten().filter_map(|o| o["event"].as_str()).collect();
        assert_eq!(events, vec!["ASK", "NOGO"], "control: a missing file delivers the failure route: {}", miss[0]);
        assert_eq!(miss[0]["result"]["outcomes"][1]["to"], "failed", "{}", miss[0]);
        let effect = &miss[0]["result"]["outcomes"][0]["effects"][0];
        assert_eq!((effect["status"].as_str(), effect["routed"].as_bool()), (Some("failed"), Some(true)), "the failure was delivered to its route: {}", miss[0]);
    }

    #[test]
    fn step_live_waits_for_open_calls_unless_settle_ms_cuts_them_off() {
        let host = ProgramHost;
        let program = |seconds: &str| {
            from_json(serde_json::json!({ "name": "SleepApp",
                "osGrants": { "fs": [], "exec": ["sleep"], "net": [], "env": [], "stdio": [], "sys": [], "desktop": [] },
                "orbitals": [{ "name": "Sleep",
                "entity": { "name": "S", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "done", "type": "boolean", "default": false }] },
                "pages": [],
                "traits": [{ "name": "T", "linkedEntity": "S", "stateMachine": {
                    "states": [{ "name": "idle", "isInitial": true }, { "name": "running" }, { "name": "done" }],
                    "transitions": [
                        { "from": "idle", "to": "running", "event": "RUN", "effects": [
                            ["os/spawn", "sleep", [seconds], { "cwd": null, "env": null, "stdin": "null", "stdout": "null", "stderr": "null", "detached": false },
                                { "key": "spawn", "emit": { "success": "SPAWNED", "failure": "FAILED" } }]] },
                        { "from": "running", "to": "running", "event": "SPAWNED", "effects": [
                            ["os/wait", "@payload.pid", { "key": "wait", "emit": { "success": "WAITED", "failure": "FAILED" } }]] },
                        { "from": "running", "to": "done", "event": "WAITED", "effects": [["set", "@entity.done", true]] },
                        { "from": "running", "to": "idle", "event": "FAILED" }
                    ] } }] }] }))
            .unwrap()
        };
        let run = |extra: serde_json::Value| {
            let mut r = serde_json::json!({ "orbital": "Sleep", "trait": "T", "from": "idle", "event": "RUN", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            from_json(serde_json::json!([r])).unwrap()
        };
        let live = from_json(serde_json::json!({ "live": true })).unwrap();
        let started = std::time::Instant::now();
        let cut = json(
            &host
                .run(
                    "program/step",
                    &[
                        program("5"),
                        run(serde_json::json!({ "settleMs": 300 })),
                        live.clone(),
                    ],
                )
                .expect("steps"),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "settleMs cuts the open wait off"
        );
        assert_ne!(
            cut[0]["result"]["entityAfter"]["done"], true,
            "the cut-off call never answered: {}",
            cut[0]
        );
        let waited = json(
            &host
                .run(
                    "program/step",
                    &[program("0.2"), run(serde_json::json!({})), live.clone()],
                )
                .expect("steps"),
        );
        assert_eq!(
            waited[0]["result"]["entityAfter"]["done"], true,
            "control: without settleMs the step waits for the process: {}",
            waited[0]
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program("0"),
                    run(serde_json::json!({ "settleMs": "soon" })),
                    live,
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("settleMs must be a number"),
            "{}",
            bad.message
        );
        let replay = host
            .run(
                "program/step",
                &[program("0"), run(serde_json::json!({ "settleMs": 10 }))],
            )
            .unwrap_err();
        assert!(
            replay
                .message
                .contains("settleMs` applies to a live call only"),
            "control: no cutoff to set without live: {}",
            replay.message
        );
    }

    #[test]
    fn step_dispatches_with_the_request_identity_bound_to_the_replay_session() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "SeeApp", "orbitals": [{ "name": "See",
            "entity": { "name": "V", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "V", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "WHO", "effects": [["emit", "SEEN", { "id": "@user.id" }]] },
                    { "from": "idle", "to": "idle", "event": "MINE", "guard": ["=", "@user.id", "u1"] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str, identity: Option<serde_json::Value>| {
            let mut r = serde_json::json!({ "orbital": "See", "trait": "T", "from": "idle", "event": event, "payload": {} });
            if let Some(v) = identity {
                r["identity"] = v;
            }
            r
        };
        let u1 = serde_json::json!({ "id": "u1", "email": "u1@example.com" });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program.clone(),
                        from_json(serde_json::json!([
                            req("MINE", Some(u1.clone())),
                            req("MINE", None),
                            req("WHO", Some(u1)),
                            req("WHO", Some(serde_json::Value::Null))
                        ]))
                        .unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["guard"], "pass",
            "the step identity is @user: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["guard"], "fail",
            "control: without an identity @user is the anonymous viewer: {}",
            played[1]
        );
        assert_eq!(played[2]["result"]["emitted"][0]["payload"]["id"], "u1");
        assert_eq!(
            played[3]["result"]["emitted"][0]["payload"]["id"], "anonymous",
            "control: a null identity is the anonymous viewer: {}",
            played[3]
        );
        let bad = host
            .run(
                "program/step",
                &[
                    program.clone(),
                    from_json(serde_json::json!([req(
                        "WHO",
                        Some(serde_json::json!("u1"))
                    )]))
                    .unwrap(),
                ],
            )
            .unwrap_err();
        assert!(
            bad.message.contains("identity must be an identity object or null"),
            "{}",
            bad.message
        );
        let retired = host
            .run(
                "program/step",
                &[
                    program,
                    from_json(serde_json::json!([{ "orbital": "See", "trait": "T", "from": "idle", "event": "WHO", "viewer": { "id": "u1" } }])).unwrap(),
                ],
            )
            .unwrap_err();
        assert!(retired.message.contains("`viewer` is retired") || retired.message.contains("viewer is retired"), "{}", retired.message);
    }

    #[test]
    fn step_reports_an_os_effect_without_running_or_failing_it() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "OsApp",
            "osGrants": { "env": ["HOME"] },
            "orbitals": [{ "name": "Os",
            "entity": { "name": "O", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "O", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }, { "name": "asking" }],
                "transitions": [
                    { "from": "idle", "to": "asking", "event": "ASK", "effects": [
                        ["set", "@entity.n", 1],
                        ["os/getenv", "HOME", { "key": "home", "emit": { "success": "GOT", "failure": "NOGO" } }]] },
                    { "from": "asking", "to": "idle", "event": "GOT" },
                    { "from": "asking", "to": "idle", "event": "NOGO" }
                ] } }] }] }))
        .unwrap();
        let requests = from_json(serde_json::json!([{ "orbital": "Os", "trait": "T", "from": "idle", "event": "ASK", "payload": {} }])).unwrap();
        let played = json(
            &host
                .run("program/step", &[program, requests])
                .expect("steps"),
        );
        let r = &played[0]["result"];
        let os = r["effects"]
            .as_array()
            .and_then(|es| es.iter().find(|e| e["type"] == "os/getenv"))
            .cloned();
        assert!(
            os.as_ref().is_some_and(|e| e["status"] != "failed"),
            "the os ask is reported, not failed: {r}"
        );
        assert_eq!(
            r["effects"][0]["status"], "executed",
            "control: the arm's other effect runs"
        );
        assert_eq!(
            r["state"]["after"], "asking",
            "no answer event re-enters the trait: {r}"
        );
        assert_eq!(r["outcomes"].as_array().map(Vec::len), Some(1), "{r}");
    }

    #[test]
    fn step_reports_the_row_the_requested_trait_holds_after_it() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "RowApp", "orbitals": [{ "name": "Row",
            "entity": { "name": "R", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "n", "type": "number", "default": 0 }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "R", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "BUMP", "effects": [["set", "@entity.n", ["+", "@entity.n", 5]]] },
                    { "from": "idle", "to": "idle", "event": "NOPE", "guard": ["=", 1, 2], "effects": [["set", "@entity.n", 99]] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Row", "trait": "T", "from": "idle", "event": event, "payload": {}, "entity": { "n": 1 } });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("BUMP"), req("NOPE")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        assert_eq!(
            played[0]["result"]["entityAfter"]["n"], 6.0,
            "the set shows in the row: {}",
            played[0]
        );
        assert_eq!(
            played[1]["result"]["entityAfter"]["n"], 1.0,
            "control: a request that fires nothing leaves the seeded row: {}",
            played[1]
        );
    }

    #[test]
    fn step_never_reports_an_emit_whose_payload_failed_to_evaluate() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "EmApp", "orbitals": [{ "name": "Em",
            "entity": { "name": "Req", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }, { "name": "items", "type": "array", "default": [], "items": { "type": "object", "properties": { "id": { "name": "id", "type": "number" } } } }] },
            "pages": [],
            "traits": [{ "name": "T", "linkedEntity": "Req", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [
                    { "from": "idle", "to": "idle", "event": "GO", "effects": [
                        ["emit", "BAD", { "list": ["array/append", ["object/get", ["array/find", "@entity.items", ["fn", "p", ["=", "@p.id", 9]]], "list"], 1] }]] },
                    { "from": "idle", "to": "idle", "event": "LIT", "effects": [["emit", "GOOD", { "lit": 7 }]] }
                ] } }] }] }))
        .unwrap();
        let req = |event: &str| serde_json::json!({ "orbital": "Em", "trait": "T", "from": "idle", "event": event, "payload": {} });
        let played = json(
            &host
                .run(
                    "program/step",
                    &[
                        program,
                        from_json(serde_json::json!([req("GO"), req("LIT")])).unwrap(),
                    ],
                )
                .expect("steps"),
        );
        let r = &played[0]["result"];
        assert_eq!(
            r["emitted"],
            serde_json::json!([]),
            "a failed emit is no emit: {r}"
        );
        assert_eq!(r["effects"][0]["status"], "failed", "{r}");
        assert_eq!(
            played[1]["result"]["emitted"][0]["payload"]["lit"], 7.0,
            "control: a literal emit is reported"
        );
    }

    #[test]
    fn control_a_step_entity_that_is_not_an_object_or_a_now_that_is_not_a_number_is_refused() {
        let host = ProgramHost;
        let program = from_json(serde_json::json!({ "name": "GateApp", "orbitals": [{ "name": "Gating",
            "entity": { "name": "Gate", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
            "pages": [],
            "traits": [{ "name": "G", "linkedEntity": "Gate", "stateMachine": {
                "states": [{ "name": "ready", "isInitial": true }],
                "transitions": [{ "from": "ready", "to": "ready", "event": "GO" }] } }] }] }))
        .unwrap();
        for (bad, says) in [
            (
                serde_json::json!({ "entity": "ann" }),
                "entity must be an object",
            ),
            (serde_json::json!({ "now": "noon" }), "now must be a number"),
        ] {
            let mut r = serde_json::json!({ "orbital": "Gating", "trait": "G", "from": "ready", "event": "GO", "payload": {} });
            r.as_object_mut()
                .unwrap()
                .extend(bad.as_object().unwrap().clone());
            let err = host
                .run(
                    "program/step",
                    &[program.clone(), from_json(serde_json::json!([r])).unwrap()],
                )
                .unwrap_err();
            assert!(err.message.contains(says), "{}", err.message);
        }
    }

    #[test]
    fn control_a_bad_argument_and_an_op_not_hosted_here_fail_with_a_message() {
        let host = ProgramHost;
        assert!(host
            .run("program/read", &[Value::Number(1.0)])
            .unwrap_err()
            .message
            .contains("expected .lolo text"));
        assert!(host
            .run("program/print", &[Value::Null])
            .unwrap_err()
            .message
            .contains("not a program"));
        assert!(host
            .run("program/nonsense", &[])
            .unwrap_err()
            .message
            .contains("not an op the native program host answers"));
        assert!(host
            .run("program/compose", &[])
            .unwrap_err()
            .message
            .contains("expected a list of programs"));
    }
}
