//! The vector lookup behind `orb behaviors catalog | describe` and `orb behaviors vectors import`:
//! package snapshots (the manifest's `orb.vectors`) layered over the user store
//! (`~/.almadar/vectors/<model>.vec`), both `orbital_core::vector_store` files keyed by
//! `sha256(model ‖ 0 ‖ text)`. A text with no stored vector is embedded through the
//! native `std-llm` atom (an in-process `RuntimeKernel` over a real HTTP transport) and kept in the user store (D-A7); with no provider configured it
//! is an error, never an empty vector.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::sidecar_fetch::resolve_vectors;
use orbital_compiler::loader::index_registry_source;
use orbital_core::behavior_packages::BehaviorPackage;
use orbital_core::behavior_packages::{RegistrySource, STD_PACKAGE};
use orbital_core::effects::Effect;
use orbital_core::runtime::http_service::SecretSource;
use orbital_core::runtime::resumable::{
    drive_blocking, BlockingHost, CallSite, HttpHost, IoAnswer, IoRequest,
};
use orbital_core::runtime::{EntitySource, InMemoryEntityCache, KernelConfig, RuntimeKernel};
use orbital_core::sidecar_store::SidecarFetcher;
use orbital_core::vector_store::{quantize, text_key, Key, Layered, Quantized, VectorStore};
use orbital_core::{OrbitalSchema, RuntimeEnvironment};
use orbital_core::runtime::http_service::Transport;
use orbital_core::runtime::native_transport::UreqTransport;

use super::npm_project::discover_from;

pub(crate) const CANONICAL_MODEL: &str = "baai/bge-base-en-v1.5";
const LMSTUDIO_DEFAULT_MODEL: &str = "text-embedding-bge-base-en-v1.5";
const EMBED_BATCH: usize = 64;
const MISSING_SAMPLE: usize = 5;

/// Turns texts into vectors. The real one is the provider client; tests inject a fake.
pub(crate) trait Embedder {
    /// The model id the vectors are stored under.
    fn model(&self) -> &str;
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// The credentials the std-llm atom asks the host for, by env-var name, from the captured environment.
struct VarSecrets(HashMap<String, String>);

impl SecretSource for VarSecrets {
    fn secret(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.0.get(name).filter(|v| !v.is_empty()).cloned())
    }
}

/// Embeds through the native `std-llm` atom: an in-process `RuntimeKernel` runs `LLM_EMBED`, the atom's
/// `http` effect goes out over the shared transport, and `LLM_EMBED_RESULT` carries the vectors.
pub(crate) struct ProviderEmbedder {
    vars: HashMap<String, String>,
    provider: String,
    /// `scheme://host[:port]` the embeddings request goes to; the operator's own endpoint, so it joins the egress.
    origin: String,
    base_path: String,
    /// The env var holding the key; `None` for the keyless lmstudio.
    key_env: Option<String>,
    /// The model name sent to the provider.
    sent_model: String,
    /// The model id the vectors are stored under.
    model: String,
    transport: Arc<dyn Transport>,
    /// The `std-llm` schema; `None` loads the installed one on first use.
    schema: Option<OrbitalSchema>,
}

impl ProviderEmbedder {
    /// `None` when the environment configures no embedding provider.
    pub(crate) fn from_env() -> Option<Self> {
        // The embedding endpoint is operator configuration (a local LM Studio included), not program egress.
        Self::from_vars(
            std::env::vars().collect(),
            Arc::new(UreqTransport::unchecked()),
        )
    }

    pub(crate) fn from_vars(
        vars: HashMap<String, String>,
        transport: Arc<dyn Transport>,
    ) -> Option<Self> {
        let get = |vars: &HashMap<String, String>, k: &str| {
            vars.get(k).filter(|v| !v.is_empty()).cloned()
        };
        let provider = get(&vars, "EMBEDDING_PROVIDER").unwrap_or_else(|| "openrouter".into());
        let has_key = |vars: &HashMap<String, String>, own: &str| {
            get(vars, "EMBEDDING_API_KEY").is_some() || get(vars, own).is_some()
        };
        let configured = match provider.as_str() {
            "openai" => has_key(&vars, "OPENAI_API_KEY"),
            "openrouter" => has_key(&vars, "OPEN_ROUTER_API_KEY"),
            _ => true,
        };
        if !configured {
            return None;
        }
        let (sent_model, model) = if provider == "lmstudio" {
            let sent = get(&vars, "LMSTUDIO_EMBEDDING_MODEL")
                .unwrap_or_else(|| LMSTUDIO_DEFAULT_MODEL.into());
            (sent, CANONICAL_MODEL.to_string())
        } else {
            let m = get(&vars, "EMBEDDING_MODEL").unwrap_or_else(|| CANONICAL_MODEL.into());
            (m.clone(), m)
        };
        let own_key = |own: &str| {
            if get(&vars, "EMBEDDING_API_KEY").is_some() {
                "EMBEDDING_API_KEY".to_string()
            } else {
                own.to_string()
            }
        };
        let (origin, base_path, key_env) = match provider.as_str() {
            "openai" => (
                "https://api.openai.com".to_string(),
                "/v1".to_string(),
                Some(own_key("OPENAI_API_KEY")),
            ),
            "openrouter" => (
                "https://openrouter.ai".to_string(),
                "/api/v1".to_string(),
                Some(own_key("OPEN_ROUTER_API_KEY")),
            ),
            _ => {
                let url =
                    get(&vars, "LMSTUDIO_URL").unwrap_or_else(|| "http://127.0.0.1:1234/v1".into());
                let url = url.trim_end_matches('/');
                let (origin, path) = split_origin(url);
                (origin, path, None)
            }
        };
        Some(Self {
            vars,
            provider,
            origin,
            base_path,
            key_env,
            sent_model,
            model,
            transport,
            schema: None,
        })
    }

    /// The `std-llm` schema with the embedding knobs this configuration names.
    fn configured_schema(&self) -> anyhow::Result<OrbitalSchema> {
        let base = match &self.schema {
            Some(schema) => schema.clone(),
            None => installed_std_llm()?,
        };
        let mut knobs = vec![
            ("embedProvider", serde_json::json!(self.provider)),
            ("embedOrigin", serde_json::json!(self.origin)),
            ("embedBasePath", serde_json::json!(self.base_path)),
            ("embedModel", serde_json::json!(self.sent_model)),
        ];
        if let Some(key_env) = &self.key_env {
            knobs.push(("embedApiKey", serde_json::json!(key_env)));
        }
        let mut json = serde_json::to_value(&base)?;
        for (knob, value) in &knobs {
            let mut patched = false;
            for orbital in json["orbitals"].as_array_mut().into_iter().flatten() {
                for t in orbital["traits"].as_array_mut().into_iter().flatten() {
                    if let Some(entry) = t["config"].get_mut(*knob) {
                        entry["default"] = value.clone();
                        patched = true;
                    }
                }
            }
            if !patched {
                anyhow::bail!("the installed std-llm atom declares no `{knob}` config knob");
            }
        }
        let mut schema: OrbitalSchema = serde_json::from_value(json)?;
        let egress = schema.egress.get_or_insert_with(Vec::new);
        if !egress.contains(&self.origin) {
            egress.push(self.origin.clone());
        }
        Ok(schema)
    }

    fn answer(&self, store: &dyn EntitySource, request: &IoRequest) -> IoAnswer {
        match request {
            IoRequest::Http {
                request, delay_ms, ..
            } => {
                if let Some(ms) = delay_ms {
                    std::thread::sleep(Duration::from_millis(*ms));
                }
                IoAnswer::Http(self.transport.send(request))
            }
            other => {
                let secrets = VarSecrets(self.vars.clone());
                let egress = [self.origin.clone()];
                let on_event = |_: Option<&CallSite>, _: orbital_core::Value| {};
                let host = HttpHost {
                    transport: self.transport.as_ref(),
                    secrets: &secrets,
                    egress: &egress,
                    on_event: &on_event,
                    os: None,
                };
                BlockingHost {
                    source: store,
                    service: None,
                    substrate: None,
                }
                .answer_with_http(&host, other)
            }
        }
    }
}

type Outcomes = Vec<(
    String,
    orbital_core::runtime::process_result::ProcessEventResult,
)>;

/// Reads the atom's verdict from a dispatch: the embeddings it emitted, or the failure it ended in.
fn scan(outcomes: &Outcomes, result: &mut Option<serde_json::Value>) -> anyhow::Result<()> {
    for (_, outcome) in outcomes {
        for effect in &outcome.effect_results {
            let Effect::Emit { event, payload } = &effect.effect else {
                continue;
            };
            let value = serde_json::to_value(payload)?;
            match event.as_str() {
                "LLM_FAILED" => {
                    let message = value
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or_default();
                    anyhow::bail!("embedding failed: {message}");
                }
                "LLM_EMBED_RESULT" => *result = Some(value),
                _ => {}
            }
        }
    }
    Ok(())
}

/// `scheme://host[:port]` and the path after it (`""` when none).
fn split_origin(url: &str) -> (String, String) {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    match url[after_scheme..].find('/') {
        Some(i) => (
            url[..after_scheme + i].to_string(),
            url[after_scheme + i..].to_string(),
        ),
        None => (url.to_string(), String::new()),
    }
}

/// The installed `@almadar/std` `std-llm` atom as a schema: lowered from `.lolo` for a live package,
/// parsed from `.orb` for a materialized one.
fn installed_std_llm() -> anyhow::Result<OrbitalSchema> {
    let packages = discover_from(&std::env::current_dir()?)?;
    let std = packages
        .iter()
        .find(|p| p.name == STD_PACKAGE)
        .ok_or_else(|| {
            anyhow::anyhow!("{STD_PACKAGE} is not installed; embedding runs its std-llm atom (`orb add {STD_PACKAGE}`)")
        })?;
    let index = index_registry_source(&std.registry)?;
    let path = index.get("std-llm").ok_or_else(|| {
        anyhow::anyhow!("{STD_PACKAGE}@{} ships no std-llm behavior", std.version)
    })?;
    match std.registry {
        RegistrySource::Live(_) => orbital_lolo::parse_and_lower(path)
            .map_err(|e| anyhow::anyhow!("lower {}: {e}", path.display())),
        RegistrySource::Materialized(_) => {
            let mut schema: OrbitalSchema = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            orbital_core::stamp::ensure_stamped(&mut schema);
            Ok(schema)
        }
    }
}

impl Embedder for ProviderEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if !matches!(self.provider.as_str(), "openai" | "openrouter" | "lmstudio") {
            anyhow::bail!(
                "embedding failed: Unknown embedding provider: {}",
                self.provider
            );
        }
        let store: Arc<dyn EntitySource> = Arc::new(InMemoryEntityCache::new());
        let kernel = RuntimeKernel::new(
            RuntimeEnvironment::Server,
            store.clone(),
            KernelConfig::default(),
        );
        kernel
            .register_schema(self.configured_schema()?)
            .map_err(|e| anyhow::anyhow!("register std-llm: {e:?}"))?;
        let drive = |progress| {
            drive_blocking(&kernel, progress, &|request| {
                self.answer(store.as_ref(), request)
            })
            .map_err(|e| anyhow::anyhow!("embedding dispatch: {e:?}"))
        };
        let start = |event: &str, payload: serde_json::Value| -> anyhow::Result<_> {
            let payload: orbital_core::Value = serde_json::from_value(payload)?;
            kernel
                .start_dispatch("LlmClient", None, event, payload, None, None)
                .map_err(|e| anyhow::anyhow!("embedding dispatch: {e:?}"))
        };
        // INIT lets the atom learn whether the embeddings key is configured (a keyless LM Studio sends it when it is).
        drive(start("INIT", serde_json::Value::Null)?)?;
        // A failed attempt is sent again by the atom with its own `delayMs`; the host's `Http` answer waits it out.
        let mut result = None;
        scan(
            &drive(start("LLM_EMBED", serde_json::json!({ "texts": texts }))?)?,
            &mut result,
        )?;
        let rows = result
            .as_ref()
            .and_then(|v| v.get("embeddings"))
            .and_then(|e| e.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the std-llm atom emitted no LLM_EMBED_RESULT with an `embeddings` array"
                )
            })?;
        rows.iter()
            .map(|row| {
                row.as_array()
                    .ok_or_else(|| anyhow::anyhow!("an embedding is not an array"))?
                    .iter()
                    .map(|n| {
                        n.as_f64()
                            .map(|f| f as f32)
                            .ok_or_else(|| anyhow::anyhow!("an embedding holds a non-number"))
                    })
                    .collect()
            })
            .collect()
    }
}

/// `$ALMADAR_VECTORS_DIR`, else `~/.almadar/vectors`.
pub(crate) fn user_vectors_dir() -> PathBuf {
    match std::env::var_os("ALMADAR_VECTORS_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".almadar")
            .join("vectors"),
    }
}

/// `<dir>/<model>.vec`, the model id made file-name safe.
pub(crate) fn user_store_path(dir: &Path, model: &str) -> PathBuf {
    let slug: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{slug}.vec"))
}

/// Where the vectors live: package snapshots (first hit wins) over the user store directory.
pub(crate) struct Sources {
    pub snapshots: Vec<PathBuf>,
    pub user_dir: PathBuf,
}

impl Sources {
    /// The snapshots the packages declare (`orb.vectors`, fetched through the lock when absent
    /// from the cache), over the user store.
    pub(crate) fn for_packages<'a>(
        packages: impl IntoIterator<Item = &'a BehaviorPackage>,
        fetcher: &dyn SidecarFetcher,
    ) -> anyhow::Result<Self> {
        let mut snapshots = Vec::new();
        for pkg in packages {
            if let Some(path) = resolve_vectors(pkg, fetcher)? {
                snapshots.push(path);
            }
        }
        Ok(Self {
            snapshots,
            user_dir: user_vectors_dir(),
        })
    }
}

/// A text to look up under the caller's key.
pub(crate) struct Wanted {
    pub key: String,
    pub text: String,
}

/// The vectors already stored for `wanted` (by caller key), decoded; nothing is embedded and a miss
/// is simply absent. `fallback_model` names the model when no snapshot does.
pub(crate) fn probe(
    sources: &Sources,
    wanted: &[Wanted],
    fallback_model: &str,
) -> anyhow::Result<(String, HashMap<String, Vec<f64>>)> {
    let snapshots = sources
        .snapshots
        .iter()
        .map(|p| VectorStore::open(p))
        .collect::<Result<Vec<_>, _>>()?;
    let model = snapshots
        .iter()
        .find(|s| !s.model().is_empty())
        .map_or_else(|| fallback_model.to_string(), |s| s.model().to_string());
    let mut stores = snapshots;
    stores.push(VectorStore::open(&user_store_path(
        &sources.user_dir,
        &model,
    ))?);
    let layers = Layered::new(stores)?;
    let mut out = HashMap::new();
    for w in wanted {
        if let Some(q) = layers.get(&text_key(&model, &w.text)) {
            out.insert(w.key.clone(), decode(q.scale, q.bytes));
        }
    }
    Ok((model, out))
}

/// The vectors for `wanted` (by caller key), decoded; a text with no stored vector is embedded by
/// `embedder` and kept in the user store.
pub(crate) fn lookup(
    sources: &Sources,
    wanted: &[Wanted],
    embedder: Option<&dyn Embedder>,
) -> anyhow::Result<(String, HashMap<String, Vec<f64>>)> {
    let snapshots = sources
        .snapshots
        .iter()
        .map(|p| VectorStore::open(p))
        .collect::<Result<Vec<_>, _>>()?;
    let model = snapshots
        .iter()
        .find(|s| !s.model().is_empty())
        .map(|s| s.model().to_string())
        .or_else(|| embedder.map(|e| e.model().to_string()));
    let Some(model) = model else {
        if wanted.is_empty() {
            return Ok((String::new(), HashMap::new()));
        }
        // No snapshot to name the model: an existing user store of the canonical model can still answer.
        return lookup_with_model(sources, snapshots, wanted, embedder, CANONICAL_MODEL);
    };
    lookup_with_model(sources, snapshots, wanted, embedder, &model)
}

fn lookup_with_model(
    sources: &Sources,
    snapshots: Vec<VectorStore>,
    wanted: &[Wanted],
    embedder: Option<&dyn Embedder>,
    model: &str,
) -> anyhow::Result<(String, HashMap<String, Vec<f64>>)> {
    let user_path = user_store_path(&sources.user_dir, model);
    let keyed: Vec<(&Wanted, Key)> = wanted
        .iter()
        .map(|w| (w, text_key(model, &w.text)))
        .collect();
    let layered = |snapshots: Vec<VectorStore>| -> anyhow::Result<Layered> {
        let mut stores = snapshots;
        stores.push(VectorStore::open(&user_path)?);
        Ok(Layered::new(stores)?)
    };
    let mut layers = layered(snapshots.clone())?;
    let mut misses: Vec<&(&Wanted, Key)> = keyed
        .iter()
        .filter(|(_, k)| layers.get(k).is_none())
        .collect();
    if !misses.is_empty() {
        // One embed per distinct text.
        let mut seen: HashMap<Key, ()> = HashMap::new();
        misses.retain(|(_, k)| seen.insert(*k, ()).is_none());
        let Some(embedder) = embedder else {
            let first: Vec<&str> = misses
                .iter()
                .take(MISSING_SAMPLE)
                .map(|(w, _)| w.key.as_str())
                .collect();
            anyhow::bail!(
                "ORB_VECTOR_PROVIDER_MISSING: {} embedding text(s) have no stored vector (first: {}) and no embedding provider is configured; set EMBEDDING_PROVIDER (openrouter|openai|lmstudio) with OPEN_ROUTER_API_KEY, OPENAI_API_KEY or EMBEDDING_API_KEY (lmstudio: LMSTUDIO_URL), or import vectors with `orb behaviors vectors import`",
                misses.len(),
                first.join(", ")
            );
        };
        if embedder.model() != model {
            anyhow::bail!(
                "ORB_VECTOR_MODEL_MISMATCH: the stored vectors are `{model}` but the configured embedder produces `{}`",
                embedder.model()
            );
        }
        for chunk in misses.chunks(EMBED_BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(w, _)| w.text.clone()).collect();
            let vectors = embedder.embed(&texts)?;
            if vectors.len() != texts.len() {
                anyhow::bail!(
                    "the embedder returned {} vectors for {} texts",
                    vectors.len(),
                    texts.len()
                );
            }
            let dims = vectors.first().map(Vec::len).unwrap_or(0);
            let dims16 = u16::try_from(dims).map_err(|_| {
                anyhow::anyhow!("embedding has {dims} components, over the store limit")
            })?;
            let records: Vec<(Key, Quantized)> = chunk
                .iter()
                .zip(&vectors)
                .map(|((_, k), v)| (*k, quantize(v)))
                .collect();
            VectorStore::insert_many(&user_path, model, dims16, records)?;
        }
        layers = layered(snapshots)?;
    }
    let mut out = HashMap::new();
    for (w, k) in &keyed {
        if let Some(q) = layers.get(k) {
            out.insert(w.key.clone(), decode(q.scale, q.bytes));
        }
    }
    Ok((model.to_string(), out))
}

/// `byte × scale ÷ 127` in f64, the scale read at its stored 6 significant digits.
fn decode(scale: f32, bytes: &[u8]) -> Vec<f64> {
    let s: f64 = scale.to_string().parse().unwrap_or(scale as f64);
    bytes
        .iter()
        .map(|b| (f64::from(*b as i8) * s) / 127.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        model: String,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                model: CANONICAL_MODEL.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn embedded(&self) -> usize {
            self.calls.lock().unwrap().iter().map(Vec::len).sum()
        }
    }

    impl Embedder for Fake {
        fn model(&self) -> &str {
            &self.model
        }
        fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.calls.lock().unwrap().push(texts.to_vec());
            Ok(texts
                .iter()
                .map(|t| {
                    let n = t.len() as f32;
                    vec![n, n * 0.5 + 1.0, -n, 2.0]
                })
                .collect())
        }
    }

    fn wanted(items: &[(&str, &str)]) -> Vec<Wanted> {
        items
            .iter()
            .map(|(k, t)| Wanted {
                key: k.to_string(),
                text: t.to_string(),
            })
            .collect()
    }

    fn sources(dir: &Path) -> Sources {
        Sources {
            snapshots: Vec::new(),
            user_dir: dir.join("user"),
        }
    }

    #[test]
    fn a_miss_is_embedded_once_kept_and_never_embedded_again() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let fake = Fake::new();
        let w = wanted(&[("a", "alpha text"), ("a#capabilities", "caps")]);
        let (model, got) = lookup(&src, &w, Some(&fake)).unwrap();
        assert_eq!(model, CANONICAL_MODEL);
        assert_eq!(got.len(), 2);
        assert_eq!(got["a"].len(), 4);
        assert_eq!(fake.embedded(), 2);
        // Control: a second lookup, even with no provider, is answered from the user store.
        let (_, again) = lookup(&src, &w, None).unwrap();
        assert_eq!(again, got);
        assert_eq!(fake.embedded(), 2);
        // Decoded values follow the int8 quantization of the embedder's vector.
        let v = &got["a"];
        assert!((v[0] - 10.0).abs() < 0.1, "{v:?}");
    }

    #[test]
    fn only_the_missing_texts_are_embedded_and_duplicates_once() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let fake = Fake::new();
        lookup(&src, &wanted(&[("a", "one")]), Some(&fake)).unwrap();
        let before = fake.embedded();
        let w = wanted(&[("a", "one"), ("b", "two"), ("c", "two")]);
        let (_, got) = lookup(&src, &w, Some(&fake)).unwrap();
        assert_eq!(fake.embedded() - before, 1);
        assert_eq!(got["b"], got["c"]);
    }

    #[test]
    fn no_provider_and_a_miss_names_the_variables_and_the_first_keys() {
        let dir = tempfile::tempdir().unwrap();
        let w = wanted(&[("k1", "t1"), ("k2", "t2")]);
        let err = lookup(&sources(dir.path()), &w, None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ORB_VECTOR_PROVIDER_MISSING"), "{err}");
        assert!(
            err.contains("OPEN_ROUTER_API_KEY") && err.contains("EMBEDDING_PROVIDER"),
            "{err}"
        );
        assert!(err.contains("k1, k2"), "{err}");
        assert!(
            !dir.path().join("user").exists(),
            "nothing written on the error path"
        );
    }

    #[test]
    fn a_snapshot_answers_first_and_names_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("pkg/dist/vectors.vec");
        let q = quantize(&[1.0, -1.0, 0.5, 0.0]);
        VectorStore::insert_many(
            &snap,
            "snap-model",
            4,
            vec![(text_key("snap-model", "hello"), q)],
        )
        .unwrap();
        let src = Sources {
            snapshots: vec![snap],
            user_dir: dir.path().join("user"),
        };
        let fake = Fake::new();
        let (model, got) = lookup(&src, &wanted(&[("h", "hello")]), Some(&fake)).unwrap();
        assert_eq!(model, "snap-model");
        assert_eq!(got["h"].len(), 4);
        assert_eq!(fake.embedded(), 0);
        // Control: a miss under a snapshot whose model differs from the embedder's is refused.
        let err = lookup(&src, &wanted(&[("x", "other")]), Some(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ORB_VECTOR_MODEL_MISMATCH"), "{err}");
    }

    #[test]
    fn probe_answers_only_what_is_stored_and_never_embeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let fake = Fake::new();
        lookup(&src, &wanted(&[("a", "stored")]), Some(&fake)).unwrap();
        let before = fake.embedded();
        let (model, got) = probe(
            &src,
            &wanted(&[("a", "stored"), ("b", "not stored")]),
            CANONICAL_MODEL,
        )
        .unwrap();
        assert_eq!(model, CANONICAL_MODEL);
        assert!(got.contains_key("a") && !got.contains_key("b"));
        assert_eq!(fake.embedded(), before);
    }

    #[test]
    fn nothing_wanted_needs_no_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (model, got) = lookup(&sources(dir.path()), &[], None).unwrap();
        assert!(model.is_empty() && got.is_empty());
    }

    #[test]
    fn provider_is_configured_only_with_what_its_embed_needs() {
        let t: Arc<dyn Transport> = Arc::new(UreqTransport::new());
        let vars = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert!(ProviderEmbedder::from_vars(vars(&[]), t.clone()).is_none());
        assert!(
            ProviderEmbedder::from_vars(vars(&[("EMBEDDING_PROVIDER", "openai")]), t.clone())
                .is_none()
        );
        let or =
            ProviderEmbedder::from_vars(vars(&[("OPEN_ROUTER_API_KEY", "k")]), t.clone()).unwrap();
        assert_eq!(or.model(), CANONICAL_MODEL);
        let lm =
            ProviderEmbedder::from_vars(vars(&[("EMBEDDING_PROVIDER", "lmstudio")]), t.clone())
                .unwrap();
        assert_eq!(lm.model(), CANONICAL_MODEL);
        assert_eq!(lm.sent_model.as_str(), LMSTUDIO_DEFAULT_MODEL);
        let oa = ProviderEmbedder::from_vars(
            vars(&[
                ("EMBEDDING_PROVIDER", "openai"),
                ("OPENAI_API_KEY", "k"),
                ("EMBEDDING_MODEL", "text-embedding-3-small"),
            ]),
            t,
        )
        .unwrap();
        assert_eq!(oa.model(), "text-embedding-3-small");
    }

    use orbital_core::runtime::http_service::{HttpRequest, HttpResponse, TransportError};
    use std::collections::VecDeque;

    const MOCK_BASE: &str = "http://mock.test";

    /// Answers each request from the recorded exchanges of an `orbital-integration-tests/fixtures/llm` embed fixture,
    /// after checking the atom sent the recorded method, URL, body and semantic headers.
    struct Replay {
        script: Mutex<VecDeque<serde_json::Value>>,
        /// When each request arrived, and the `Authorization` header it carried.
        seen: Mutex<Vec<(std::time::Instant, String)>>,
    }

    impl Transport for Replay {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let exchange = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("the atom sent an unrecorded request");
            let recorded = &exchange["request"];
            let auth = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            self.seen
                .lock()
                .unwrap()
                .push((std::time::Instant::now(), auth));
            assert_eq!(request.method, recorded["method"].as_str().unwrap());
            assert_eq!(
                request.url,
                recorded["url"]
                    .as_str()
                    .unwrap()
                    .replace("{base}", MOCK_BASE)
            );
            let sent: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let expected: serde_json::Value =
                serde_json::from_str(recorded["body"].as_str().unwrap()).unwrap();
            assert_eq!(sent, expected);
            for (name, value) in recorded["headers"].as_object().unwrap() {
                let got = request
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.as_str())
                    .unwrap_or_else(|| panic!("header {name} missing"));
                if value == "<redacted>" {
                    assert!(got.starts_with("Bearer "), "{got}");
                } else {
                    assert_eq!(got, value.as_str().unwrap(), "header {name}");
                }
            }
            let reply = &exchange["response"];
            Ok(HttpResponse {
                status: reply["status"].as_u64().unwrap() as u16,
                headers: Vec::new(),
                body: reply["body"].as_str().unwrap().as_bytes().to_vec(),
            })
        }
    }

    fn replayed(case: &str) -> (ProviderEmbedder, serde_json::Value, Arc<Replay>) {
        replayed_with(case, &[])
    }

    fn replayed_with(
        case: &str,
        extra: &[(&str, &str)],
    ) -> (ProviderEmbedder, serde_json::Value, Arc<Replay>) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../orbital-integration-tests/fixtures/llm")
            .join(format!("{case}.json"));
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut vars: HashMap<String, String> = fixture["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().replace("{base}", MOCK_BASE)))
            .collect();
        // The recording names the model in the request; LM Studio would otherwise send its own default.
        if vars.get("EMBEDDING_PROVIDER").map(String::as_str) == Some("lmstudio") {
            vars.insert("LMSTUDIO_EMBEDDING_MODEL".into(), CANONICAL_MODEL.into());
        }
        for (k, v) in extra {
            vars.insert(k.to_string(), v.to_string());
        }
        let replay = Arc::new(Replay {
            seen: Mutex::new(Vec::new()),
            script: Mutex::new(
                fixture["exchanges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect(),
            ),
        });
        let mut embedder = ProviderEmbedder::from_vars(vars, replay.clone()).unwrap();
        embedder.schema = Some(orbital_test_support::installed_schema(
            "almadar-std",
            Path::new(env!("CARGO_MANIFEST_DIR")),
            "std-llm",
        ));
        (embedder, fixture, replay)
    }

    fn texts_of(fixture: &serde_json::Value) -> Vec<String> {
        fixture["params"]["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_atom_embeds_the_recorded_exchange_for_each_provider() {
        for case in ["openrouter-embed-default", "lmstudio-embed"] {
            let (embedder, fixture, replay) = replayed(case);
            let got = embedder.embed(&texts_of(&fixture)).unwrap();
            let expected: Vec<Vec<f32>> = fixture["outcome"]["data"]["embeddings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    row.as_array()
                        .unwrap()
                        .iter()
                        .map(|n| n.as_f64().unwrap() as f32)
                        .collect()
                })
                .collect();
            assert_eq!(got, expected, "{case}");
            assert!(
                replay.script.lock().unwrap().is_empty(),
                "{case}: unused exchanges"
            );
        }
    }

    #[test]
    fn a_provider_error_envelope_fails_with_the_recorded_message_after_the_recorded_retries() {
        let (embedder, fixture, replay) = replayed("lmstudio-embed-provider-error-envelope");
        let err = embedder.embed(&texts_of(&fixture)).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "embedding failed: {}",
                fixture["outcome"]["message"].as_str().unwrap()
            )
        );
        assert!(replay.script.lock().unwrap().is_empty());
    }

    #[test]
    fn the_recorded_retries_wait_the_old_backoff_between_attempts() {
        let (embedder, fixture, replay) = replayed("lmstudio-embed-provider-error-envelope");
        embedder.embed(&texts_of(&fixture)).unwrap_err();
        let seen = replay.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let gap = |i: usize| seen[i].0.duration_since(seen[i - 1].0);
        // Control: the waits are the 500 ms and 2000 ms of the retired client, not an immediate re-send.
        assert!(
            gap(1) >= Duration::from_millis(500) && gap(1) < Duration::from_millis(1500),
            "{:?}",
            gap(1)
        );
        assert!(
            gap(2) >= Duration::from_millis(2000) && gap(2) < Duration::from_millis(3500),
            "{:?}",
            gap(2)
        );
    }

    #[test]
    fn lmstudio_sends_the_embedding_key_when_set_and_not_needed_when_it_is_not() {
        let (keyed, fixture, replay) =
            replayed_with("lmstudio-embed", &[("EMBEDDING_API_KEY", "key-123")]);
        keyed.embed(&texts_of(&fixture)).unwrap();
        assert_eq!(replay.seen.lock().unwrap()[0].1, "Bearer key-123");
        let (keyless, fixture, replay) = replayed("lmstudio-embed");
        keyless.embed(&texts_of(&fixture)).unwrap();
        assert_eq!(replay.seen.lock().unwrap()[0].1, "Bearer not-needed");
    }

    #[test]
    fn a_miss_is_embedded_through_the_atom_and_kept_while_no_provider_still_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let (embedder, _, _) = replayed("openrouter-embed-default");
        let w = wanted(&[("a", "alpha")]);
        let (model, got) = lookup(&src, &w, Some(&embedder)).unwrap();
        assert_eq!(model, CANONICAL_MODEL);
        assert_eq!(got["a"].len(), 2);
        assert!((got["a"][0] - 0.5).abs() < 0.01, "{:?}", got["a"]);
        // Control: with no provider configured the same environment-free lookup of a new text is refused.
        assert!(
            ProviderEmbedder::from_vars(HashMap::new(), Arc::new(UreqTransport::new())).is_none()
        );
        let err = lookup(&src, &wanted(&[("b", "beta")]), None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ORB_VECTOR_PROVIDER_MISSING"), "{err}");
    }

    #[test]
    fn an_lmstudio_url_splits_into_origin_and_base_path() {
        assert_eq!(
            split_origin("http://127.0.0.1:1234/v1"),
            ("http://127.0.0.1:1234".to_string(), "/v1".to_string())
        );
        assert_eq!(
            split_origin("http://lm.local:9"),
            ("http://lm.local:9".to_string(), String::new())
        );
    }

    #[test]
    fn the_user_store_file_name_is_made_safe() {
        assert_eq!(
            user_store_path(Path::new("/d"), "baai/bge-base-en-v1.5"),
            Path::new("/d/baai_bge-base-en-v1.5.vec")
        );
    }
}
