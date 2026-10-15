//! The vector lookup behind `orb behaviors catalog | describe` and `orb behaviors vectors import`:
//! package snapshots (the manifest's `orb.vectors`) layered over the user store
//! (`~/.almadar/vectors/<model>.vec`), both `orbital_core::vector_store` files keyed by
//! `sha256(model ‖ 0 ‖ text)`. A text with no stored vector is embedded through the
//! `orbital-integrations` client and kept in the user store (D-A7); with no provider configured it
//! is an error, never an empty vector.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::sidecar_fetch::resolve_vectors;
use orbital_core::behavior_packages::BehaviorPackage;
use orbital_core::sidecar_store::SidecarFetcher;
use orbital_core::vector_store::{quantize, text_key, Key, Layered, Quantized, VectorStore};
use orbital_integrations::credentials::Credentials;
use orbital_integrations::transport::{Transport, UreqTransport};

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

pub(crate) struct ProviderEmbedder {
    creds: Credentials,
    model: String,
    transport: Arc<dyn Transport>,
}

impl ProviderEmbedder {
    /// `None` when the environment configures no embedding provider.
    pub(crate) fn from_env() -> Option<Self> {
        Self::from_vars(std::env::vars().collect(), Arc::new(UreqTransport::new()))
    }

    pub(crate) fn from_vars(
        mut vars: HashMap<String, String>,
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
        let (sent, stored) = if provider == "lmstudio" {
            let sent = get(&vars, "LMSTUDIO_EMBEDDING_MODEL")
                .unwrap_or_else(|| LMSTUDIO_DEFAULT_MODEL.into());
            (sent, CANONICAL_MODEL.to_string())
        } else {
            let m = get(&vars, "EMBEDDING_MODEL").unwrap_or_else(|| CANONICAL_MODEL.into());
            (m.clone(), m)
        };
        vars.insert("EMBEDDING_MODEL".into(), sent);
        Some(Self {
            creds: Credentials::from_env(vars),
            model: stored,
            transport,
        })
    }
}

impl Embedder for ProviderEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let wait = |d: Duration| std::thread::sleep(d);
        let out =
            orbital_integrations::llm::embed_texts(&self.creds, &self.transport, &wait, texts)
                .map_err(|e| anyhow::anyhow!("embedding failed: {}", e.message))?;
        let rows = out
            .get("embeddings")
            .and_then(|e| e.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("the embedding client returned no `embeddings` array")
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
        assert_eq!(
            lm.creds.get("EMBEDDING_MODEL"),
            Some(LMSTUDIO_DEFAULT_MODEL)
        );
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

    #[test]
    fn the_user_store_file_name_is_made_safe() {
        assert_eq!(
            user_store_path(Path::new("/d"), "baai/bge-base-en-v1.5"),
            Path::new("/d/baai_bge-base-en-v1.5.vec")
        );
    }
}
