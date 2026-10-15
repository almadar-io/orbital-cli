//! `orb behaviors vectors import`: convert a legacy JSON embedding manifest into the user vector
//! store. A store key needs the text, so each manifest key is joined to the text the installed
//! package builds for it, and the vector is kept only when sha256(text) equals the hash the
//! calibrate text index recorded for that key (the vector then really is that text's).

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use orbital_core::vector_store::{quantize, text_key, Key, Quantized, VectorStore};

use super::behaviors_catalog::text_facts;
use super::behaviors_manifest::{Manifest, Want};
use super::behaviors_texts::{
    index_texts, texts_of, RegistryEntry, TextKind, CAPABILITY_KEY_SUFFIX,
};
use super::behaviors_vectors::{user_store_path, user_vectors_dir};
use super::npm_project::discover_from;

#[derive(Serialize, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportReport {
    pub model: String,
    pub store: String,
    pub imported: usize,
    /// The manifest key names no organism or capability text of the package (a knob key, or a removed behavior).
    pub skipped_no_text: usize,
    /// The index has no hash for the key.
    pub skipped_not_indexed: usize,
    /// sha256 of the text built now differs from the hash the vector was embedded from.
    pub skipped_text_changed: usize,
    /// Imported under the text a full describe builds, because the index text did not hash to the recorded one.
    pub imported_via_describe: usize,
}

/// `<derivedCacheDir>/embed-index/<sha256(abs manifest path)>.json`.
fn default_index_path(manifest: &Path) -> anyhow::Result<PathBuf> {
    let abs = std::path::absolute(manifest)?;
    let id: String = Sha256::digest(abs.to_string_lossy().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let root = match std::env::var_os("ALMADAR_DERIVED_CACHE") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".almadar")
            .join("derived"),
    };
    Ok(root.join("embed-index").join(format!("{id}.json")))
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The organism and capability texts of every behavior in `index` (a package `index` JSON), by manifest key.
fn package_texts(index: &Value) -> anyhow::Result<HashMap<String, String>> {
    let behaviors = index
        .get("behaviors")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("the package index has no `behaviors` map"))?;
    let mut out = HashMap::new();
    for (name, entry) in behaviors {
        let (organism, capability) = index_texts(name, entry)?;
        out.insert(name.clone(), organism);
        if let Some(c) = capability {
            out.insert(format!("{name}{CAPABILITY_KEY_SUFFIX}"), c);
        }
    }
    Ok(out)
}

/// Import `manifest` into the store under `store_dir`, joining keys to `texts` and checking `hashes`.
/// A key whose index text does not hash to the recorded hash is retried with `describe_text(key)`,
/// the text a full describe of the behavior builds (the index lacks capabilities inherited through
/// composed traits).
pub(crate) fn import(
    manifest_path: &Path,
    texts: &HashMap<String, String>,
    hashes: &HashMap<String, String>,
    describe_text: &dyn Fn(&str) -> anyhow::Result<Option<String>>,
    store_dir: &Path,
) -> anyhow::Result<ImportReport> {
    let manifest = Manifest::read(manifest_path, &Want::Organisms)?;
    let model = manifest.model.clone();
    let mut report = ImportReport {
        model: model.clone(),
        ..Default::default()
    };
    let mut keys: Vec<&String> = manifest.keys().collect();
    keys.sort();
    let mut records: Vec<(Key, Quantized)> = Vec::new();
    let mut dims = 0usize;
    for key in keys {
        let Some(text) = texts.get(key.as_str()) else {
            report.skipped_no_text += 1;
            continue;
        };
        let Some(hash) = hashes.get(key.as_str()) else {
            report.skipped_not_indexed += 1;
            continue;
        };
        let mut text = text.clone();
        if *hash != sha256_hex(&text) {
            match describe_text(key)? {
                Some(full) if *hash == sha256_hex(&full) => {
                    text = full;
                    report.imported_via_describe += 1;
                }
                _ => {
                    report.skipped_text_changed += 1;
                    continue;
                }
            }
        }
        let Some(v) = manifest.vector(key) else {
            anyhow::bail!(
                "{}: `{key}` holds an unreadable vector",
                manifest_path.display()
            );
        };
        let v32: Vec<f32> = v.iter().map(|x| *x as f32).collect();
        dims = v32.len();
        records.push((text_key(&model, &text), quantize(&v32)));
    }
    let path = user_store_path(store_dir, &model);
    report.imported = records.len();
    report.store = path.display().to_string();
    if !records.is_empty() {
        let dims16 = u16::try_from(dims)
            .map_err(|_| anyhow::anyhow!("vectors have {dims} components, over the store limit"))?;
        VectorStore::insert_many(&path, &model, dims16, records)?;
    }
    Ok(report)
}

pub(crate) fn run_import(from: PathBuf, index: Option<PathBuf>) -> anyhow::Result<()> {
    let from = std::fs::canonicalize(&from)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", from.display()))?;
    let packages = discover_from(&std::env::current_dir()?)?;
    let pkg = packages
        .iter()
        .find(|p| {
            std::fs::canonicalize(&p.root)
                .map(|root| from.starts_with(root))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} is not inside an installed behavior package; run this from the project that installs it",
                from.display()
            )
        })?;
    let raw = std::fs::read_to_string(&pkg.index)?;
    let package_index: Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("invalid index {}: {e}", pkg.index.display()))?;
    let texts = package_texts(&package_index)?;
    let index_path = match index {
        Some(p) => p,
        None => default_index_path(&from)?,
    };
    let text_index: Value =
        serde_json::from_str(&std::fs::read_to_string(&index_path).map_err(|e| {
            anyhow::anyhow!("cannot read the text index {}: {e}", index_path.display())
        })?)
        .map_err(|e| anyhow::anyhow!("invalid text index {}: {e}", index_path.display()))?;
    let hashes: HashMap<String, String> = text_index
        .get("texts")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("{} has no `texts` map", index_path.display()))?
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|h| (k.clone(), h.to_string())))
        .collect();
    let behaviors = package_index
        .get("behaviors")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("the package index has no `behaviors` map"))?;
    let describe_text = |key: &str| -> anyhow::Result<Option<String>> {
        let Some(entry) = behaviors.get(key) else {
            return Ok(None);
        };
        let registry = [(key.to_string(), RegistryEntry::from_value(key, entry)?)];
        let facts = text_facts(&format!("{}/{key}", pkg.prefix), key)?;
        Ok(texts_of(&registry, &[facts], Some(TextKind::Organism))
            .into_iter()
            .next()
            .map(|t| t.text))
    };
    let report = import(&from, &texts, &hashes, &describe_text, &user_vectors_dir())?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, manifest: &str) -> PathBuf {
        let p = dir.join("m.json");
        std::fs::write(&p, manifest).unwrap();
        p
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_vector_is_imported_when_the_text_hash_matches_and_skipped_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        // {s:2.0, d: bytes 127,-64,0} -> base64 of [0x7f,0xc0,0x00]
        let m = fixture(
            dir.path(),
            r#"{"model":"m1","vectors":{"ok":{"s":2.0,"d":"f8AA"},"changed":{"s":2.0,"d":"f8AA"},"unindexed":{"s":2.0,"d":"f8AA"},"gone":{"s":2.0,"d":"f8AA"}}}"#,
        );
        let texts = map(&[
            ("ok", "ok text"),
            ("changed", "new text"),
            ("unindexed", "u text"),
        ]);
        let hashes = map(&[
            ("ok", &sha256_hex("ok text")),
            ("changed", &sha256_hex("old text")),
            ("gone", &sha256_hex("g")),
        ]);
        let store = dir.path().join("store");
        let none_text = |_: &str| Ok(None);
        let report = import(&m, &texts, &hashes, &none_text, &store).unwrap();
        assert_eq!(
            (
                report.imported,
                report.skipped_no_text,
                report.skipped_not_indexed,
                report.skipped_text_changed
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(report.imported_via_describe, 0);
        let s = VectorStore::open(&user_store_path(&store, "m1")).unwrap();
        assert_eq!(s.len(), 1);
        let got = s.get(&text_key("m1", "ok text")).unwrap();
        // The int8 bytes and scale survive the round trip exactly.
        assert_eq!(got.scale, 2.0);
        assert_eq!(
            got.bytes.iter().map(|b| *b as i8).collect::<Vec<_>>(),
            vec![127, -64, 0]
        );
        // Control: the changed text is not retrievable under either text.
        assert!(s.get(&text_key("m1", "new text")).is_none());
        assert!(s.get(&text_key("m1", "old text")).is_none());
    }

    #[test]
    fn importing_twice_keeps_one_record_and_an_empty_match_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let m = fixture(dir.path(), r#"{"model":"m1","vectors":{"a":[0.5,-0.25]}}"#);
        let texts = map(&[("a", "A")]);
        let hashes = map(&[("a", &sha256_hex("A"))]);
        let store = dir.path().join("store");
        let none_text = |_: &str| Ok(None);
        import(&m, &texts, &hashes, &none_text, &store).unwrap();
        import(&m, &texts, &hashes, &none_text, &store).unwrap();
        assert_eq!(
            VectorStore::open(&user_store_path(&store, "m1"))
                .unwrap()
                .len(),
            1
        );
        let none = import(
            &m,
            &map(&[]),
            &map(&[]),
            &none_text,
            &dir.path().join("other"),
        )
        .unwrap();
        assert_eq!(none.imported, 0);
        assert!(!user_store_path(&dir.path().join("other"), "m1").exists());
    }

    #[test]
    fn a_changed_index_text_is_rescued_by_the_describe_text_only_when_that_hashes_right() {
        let dir = tempfile::tempdir().unwrap();
        let m = fixture(
            dir.path(),
            r#"{"model":"m1","vectors":{"rescued":[0.5,-0.25],"wrong":[0.5,-0.25]}}"#,
        );
        let texts = map(&[("rescued", "index text"), ("wrong", "index text 2")]);
        let hashes = map(&[
            ("rescued", &sha256_hex("full text")),
            ("wrong", &sha256_hex("another full text")),
        ]);
        let describe = |key: &str| -> anyhow::Result<Option<String>> {
            Ok(Some(match key {
                "rescued" => "full text".to_string(),
                _ => "not what was embedded".to_string(),
            }))
        };
        let store = dir.path().join("store");
        let report = import(&m, &texts, &hashes, &describe, &store).unwrap();
        assert_eq!(
            (
                report.imported,
                report.imported_via_describe,
                report.skipped_text_changed
            ),
            (1, 1, 1)
        );
        let s = VectorStore::open(&user_store_path(&store, "m1")).unwrap();
        assert!(s.get(&text_key("m1", "full text")).is_some());
        assert!(s.get(&text_key("m1", "index text")).is_none());
    }
}
