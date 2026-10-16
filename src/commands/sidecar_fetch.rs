//! The network half of the sidecar store: a `ureq` fetcher injected into
//! `orbital_core::sidecar_store`, and the vector snapshot resolver the catalog readers call.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use orbital_core::behavior_packages::BehaviorPackage;
use orbital_core::sidecar_store::{ensure_sidecar, SidecarFetcher};

pub(crate) struct UreqFetcher;

impl SidecarFetcher for UreqFetcher {
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<(), String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(300))
            .build();
        // ureq's own error text embeds the request url, which carries the capability token.
        let response = agent.get(url).call().map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            ureq::Error::Transport(t) => format!("transport error: {}", t.kind()),
        })?;
        io::copy(&mut response.into_reader(), sink)
            .map(|_| ())
            .map_err(|e| format!("download interrupted: {:?}", e.kind()))
    }
}

/// The readable path of the package's `orb.vectors` snapshot: a package file as-is, a lock-resolved
/// one from the cache, downloaded and verified first when absent. `None` when none is declared.
pub(crate) fn resolve_vectors(
    pkg: &BehaviorPackage,
    fetcher: &dyn SidecarFetcher,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = &pkg.vectors else {
        return Ok(None);
    };
    match pkg.sidecars.iter().find(|s| &s.cache_path == path) {
        Some(sidecar) => ensure_sidecar(&pkg.name, &pkg.version, sidecar, fetcher)
            .map(Some)
            .map_err(anyhow::Error::from),
        None => Ok(Some(path.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbital_core::behavior_packages::discover_with_cache;
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;

    const TOKEN: &str = "TOPSECRETTOKEN";

    struct Fake {
        body: Vec<u8>,
        fail: Option<String>,
        calls: Cell<u32>,
    }
    impl SidecarFetcher for Fake {
        fn fetch(&self, _url: &str, sink: &mut dyn Write) -> Result<(), String> {
            self.calls.set(self.calls.get() + 1);
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            sink.write_all(&self.body).map_err(|e| e.to_string())
        }
    }

    fn fake(body: &[u8]) -> Fake {
        Fake {
            body: body.to_vec(),
            fail: None,
            calls: Cell::new(0),
        }
    }

    fn sha(body: &[u8]) -> String {
        Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// An installed package whose only vectors snapshot lives in the lock, not the package.
    fn locked_package(root: &Path, body: &[u8], bytes: u64) -> (BehaviorPackage, PathBuf) {
        let pkg_dir = root.join("node_modules/pkg-a");
        for (rel, content) in [("behaviors/registry/x.orb", "{}")] {
            let path = pkg_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"pkg-a","version":"1.0.0","orb":{"prefix":"pa","registry":"behaviors/registry","signatures":"behaviors/signatures.lolo","vectors":"dist/vectors.vec","compiler":">=0.0.0"}}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("sidecars.lock.json"),
            format!(
                r#"{{"version":1,"files":{{"dist/vectors.vec":{{"sha256":"{}","bytes":{bytes},"url":"https://storage.example.test/o/x?token={TOKEN}"}}}}}}"#,
                sha(body)
            ),
        )
        .unwrap();
        let cache = root.join("cache");
        let pkg = discover_with_cache(root, &root.join("store"), &cache)
            .unwrap()
            .remove(0);
        (pkg, cache)
    }

    fn cache_is_empty(cache: &Path) -> bool {
        fs::read_dir(cache)
            .map(|mut r| r.next().is_none())
            .unwrap_or(true)
    }

    #[test]
    fn cache_hit_does_not_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let body = br#"{"model":"m"}"#;
        let (pkg, cache) = locked_package(dir.path(), body, body.len() as u64);
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(sha(body)), body).unwrap();
        let f = fake(b"unused");
        let paths = resolve_vectors(&pkg, &f).unwrap();
        assert_eq!(paths, Some(cache.join(sha(body))));
        assert_eq!(f.calls.get(), 0);
    }

    #[test]
    fn miss_fetches_verifies_and_stores() {
        let dir = tempfile::tempdir().unwrap();
        let body = br#"{"model":"m"}"#;
        let (pkg, cache) = locked_package(dir.path(), body, body.len() as u64);
        let f = fake(body);
        let path = resolve_vectors(&pkg, &f).unwrap().unwrap();
        assert_eq!(f.calls.get(), 1);
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(path, cache.join(sha(body)));
        resolve_vectors(&pkg, &f).unwrap();
        assert_eq!(f.calls.get(), 1);
    }

    #[test]
    fn hash_mismatch_errors_and_leaves_no_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = br#"{"model":"m"}"#;
        let (pkg, cache) = locked_package(dir.path(), body, body.len() as u64);
        let err = resolve_vectors(&pkg, &fake(br#"{"model":"X"}"#)).unwrap_err();
        assert!(
            err.to_string().starts_with("ORB_SIDECAR_HASH_MISMATCH"),
            "{err}"
        );
        assert!(cache_is_empty(&cache));
    }

    #[test]
    fn fetch_failure_reports_host_not_token() {
        let dir = tempfile::tempdir().unwrap();
        let body = br#"{"model":"m"}"#;
        let (pkg, cache) = locked_package(dir.path(), body, body.len() as u64);
        let f = Fake {
            body: Vec::new(),
            fail: Some(format!(
                "boom https://storage.example.test/o/x?token={TOKEN}"
            )),
            calls: Cell::new(0),
        };
        let msg = resolve_vectors(&pkg, &f).unwrap_err().to_string();
        assert!(msg.starts_with("ORB_SIDECAR_FETCH_FAILED"), "{msg}");
        assert!(msg.contains("storage.example.test"));
        assert!(msg.contains("dist/vectors.vec"));
        assert!(!msg.contains(TOKEN), "{msg}");
        assert!(cache_is_empty(&cache));
    }

    #[test]
    fn a_package_file_not_in_the_lock_is_returned_untouched_and_none_when_undeclared() {
        let dir = tempfile::tempdir().unwrap();
        let (mut pkg, _) = locked_package(dir.path(), b"x", 1);
        let plain = dir.path().join("plain.vec");
        pkg.vectors = Some(plain.clone());
        let f = fake(b"x");
        assert_eq!(resolve_vectors(&pkg, &f).unwrap(), Some(plain));
        assert_eq!(f.calls.get(), 0);
        pkg.vectors = None;
        assert_eq!(resolve_vectors(&pkg, &f).unwrap(), None);
    }
}
