//! `orb behaviors vectors snapshot --package <dir>`: write the package's `dist/vectors.vec`, a
//! `VectorStore` holding exactly the vectors of its organism and `#capabilities` texts (D-A5,
//! D-A14), warmed from the layered store and embedding misses into the user store (D-A7).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use orbital_core::vector_store::{text_key, Key, Layered, VectorStore};

use super::behaviors_catalog::scoped_behaviors;
use super::behaviors_texts::{describe_all, texts_of, RegistryEntry, TextKind};
use super::behaviors_vectors::{
    lookup, user_store_path, user_vectors_dir, Embedder, ProviderEmbedder, Sources, Wanted,
};
use super::npm_project::discover_from;

pub(crate) const DEFAULT_OUT: &str = "dist/vectors.vec";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SnapshotReport {
    pub model: String,
    pub records: usize,
}

/// Write the snapshot of `wanted` to `out`. Vectors come from `out` itself (when it exists) and the
/// user store under `user_dir`; a text in neither is embedded by `embedder` into the user store.
/// Records are sorted by key, so the same inputs give the same bytes.
pub(crate) fn write_snapshot(
    user_dir: &Path,
    wanted: &[Wanted],
    embedder: Option<&dyn Embedder>,
    out: &Path,
) -> anyhow::Result<SnapshotReport> {
    if wanted.is_empty() {
        anyhow::bail!("the package has no organism or capability text to snapshot");
    }
    let sources = Sources {
        snapshots: if out.is_file() {
            vec![out.to_path_buf()]
        } else {
            Vec::new()
        },
        user_dir: user_dir.to_path_buf(),
    };
    let (model, _) = lookup(&sources, wanted, embedder)?;
    let mut stores = Vec::new();
    for snapshot in &sources.snapshots {
        stores.push(VectorStore::open(snapshot)?);
    }
    stores.push(VectorStore::open(&user_store_path(user_dir, &model))?);
    let layers = Layered::new(stores)?;
    let dims = layers
        .dims()
        .ok_or_else(|| anyhow::anyhow!("no vector was found or embedded"))?;
    let mut seen: HashSet<Key> = HashSet::new();
    let mut records = Vec::new();
    for w in wanted {
        let key = text_key(&model, &w.text);
        if !seen.insert(key) {
            continue;
        }
        let q = layers
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("no vector for `{}` after embedding", w.key))?;
        records.push((key, q.to_owned()));
    }
    let store = VectorStore::from_records(&model, dims, records)?;
    store.write(out)?;
    Ok(SnapshotReport {
        model,
        records: store.len(),
    })
}

/// The package's organism and capability texts, from the same describe path as
/// `orb behaviors texts --kind organism,capability`.
fn package_wanted(prefix: &str) -> anyhow::Result<Vec<Wanted>> {
    let mut wanted = Vec::new();
    for package in scoped_behaviors(&[prefix.to_string()])? {
        let registry = package
            .behaviors
            .iter()
            .map(|(name, v)| Ok((name.clone(), RegistryEntry::from_value(name, v)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let facts = describe_all(&package.prefix, &registry, Some(TextKind::Organism))?;
        for kind in [TextKind::Organism, TextKind::Capability] {
            wanted.extend(
                texts_of(&registry, &facts, Some(kind))
                    .into_iter()
                    .map(|t| Wanted {
                        key: t.key,
                        text: t.text,
                    }),
            );
        }
    }
    Ok(wanted)
}

pub(crate) fn run_snapshot(package: PathBuf, out: Option<PathBuf>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let dir = std::fs::canonicalize(&package)
        .map_err(|e| anyhow::anyhow!("cannot read --package {}: {e}", package.display()))?;
    let packages = discover_from(&cwd)?;
    let pkg = packages.iter().find(|p| p.root == dir).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not an installed behavior package reachable from {}",
            dir.display(),
            cwd.display()
        )
    })?;
    let out = out.unwrap_or_else(|| dir.join(DEFAULT_OUT));
    let wanted = package_wanted(&pkg.prefix)?;
    let embedder = ProviderEmbedder::from_env();
    let report = write_snapshot(
        &user_vectors_dir(),
        &wanted,
        embedder.as_ref().map(|e| e as &dyn Embedder),
        &out,
    )?;
    println!(
        "{}: {} vectors ({}) -> {}",
        pkg.prefix,
        report.records,
        report.model,
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::behaviors_vectors::CANONICAL_MODEL;
    use super::*;
    use std::sync::Mutex;

    struct Fake(Mutex<usize>);
    impl Embedder for Fake {
        fn model(&self) -> &str {
            CANONICAL_MODEL
        }
        fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            *self.0.lock().unwrap() += texts.len();
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32, 1.0, -2.0, 0.5])
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

    #[test]
    fn the_snapshot_holds_exactly_the_wanted_texts_and_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let out = dir.path().join("pkg/dist/vectors.vec");
        let fake = Fake(Mutex::new(0));
        // A vector in the user store that the package does not want must not leak into the snapshot.
        super::lookup(
            &Sources {
                snapshots: vec![],
                user_dir: user.clone(),
            },
            &wanted(&[("x", "unrelated")]),
            Some(&fake),
        )
        .unwrap();
        let w = wanted(&[
            ("a", "alpha"),
            ("a#capabilities", "caps"),
            ("b", "beta"),
            ("c", "beta"),
        ]);
        let report = write_snapshot(&user, &w, Some(&fake), &out).unwrap();
        assert_eq!(report.records, 3, "duplicate texts share one record");
        assert_eq!(report.model, CANONICAL_MODEL);
        let first = std::fs::read(&out).unwrap();
        let store = VectorStore::open(&out).unwrap();
        assert!(store.get(&text_key(CANONICAL_MODEL, "alpha")).is_some());
        assert!(store.get(&text_key(CANONICAL_MODEL, "unrelated")).is_none());
        // Control: a rerun (now answered from the stores, no provider) writes the same bytes.
        write_snapshot(&user, &w, None, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), first);
    }

    #[test]
    fn a_stale_record_is_dropped_and_a_missing_provider_fails_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let out = dir.path().join("dist/vectors.vec");
        let fake = Fake(Mutex::new(0));
        write_snapshot(
            &user,
            &wanted(&[("a", "old"), ("b", "keep")]),
            Some(&fake),
            &out,
        )
        .unwrap();
        let report = write_snapshot(&user, &wanted(&[("b", "keep")]), None, &out).unwrap();
        assert_eq!(report.records, 1);
        let before = std::fs::read(&out).unwrap();
        let err = write_snapshot(&user, &wanted(&[("n", "never embedded")]), None, &out)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ORB_VECTOR_PROVIDER_MISSING"), "{err}");
        assert_eq!(std::fs::read(&out).unwrap(), before);
    }

    #[test]
    fn nothing_to_snapshot_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_snapshot(dir.path(), &[], None, &dir.path().join("v.vec")).unwrap_err();
        assert!(err.to_string().contains("no organism or capability text"));
    }
}
