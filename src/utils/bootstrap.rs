//! The bun-free repair path of `orb update`: when no installed program answers it (the store is empty, broken, or does not
//! load on this orb), the bootloader installs the packages the update program itself needs to load, @almadar/core and
//! @almadar/std, then hands the command to `std-orb-packages`. Each comes from the lockfile pin that names it, else from the
//! npm registry: the newest stable, non-deprecated version whose orb compiler range admits this orb.

use anyhow::{anyhow, bail, Context};
use orbital_core::codec::{base64_encode, gzip_decompress, sha512, tar_files};
use orbital_core::language_data::{compiler_satisfies, CORE_PACKAGE, STD_PACKAGE};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// What the update program needs to load, in install order.
pub(crate) const BOOT_PACKAGES: [&str; 2] = [CORE_PACKAGE, STD_PACKAGE];
const REGISTRY: &str = "https://registry.npmjs.org";
const LOCKFILE: &str = "orb.lock.json";
const MAX_TARBALL: usize = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const STAGING: &str = ".orb-staging";

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct Pin {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) url: String,
    /// The npm subresource integrity, `sha512-<base64>`.
    pub(crate) integrity: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Lock {
    packages: Vec<Pin>,
}

#[derive(serde::Deserialize)]
struct Packument {
    versions: BTreeMap<String, Manifest>,
}

#[derive(serde::Deserialize)]
struct CompilerRange {
    compiler: Option<String>,
}

#[derive(serde::Deserialize)]
struct Dist {
    tarball: String,
    integrity: Option<String>,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    deprecated: Option<String>,
    orb: Option<CompilerRange>,
    #[serde(rename = "orbLanguage")]
    orb_language: Option<CompilerRange>,
    #[serde(rename = "orbVocabulary")]
    orb_vocabulary: Option<CompilerRange>,
    dist: Dist,
}

impl Manifest {
    fn compiler(&self) -> Option<&str> {
        [&self.orb, &self.orb_language, &self.orb_vocabulary]
            .into_iter()
            .flatten()
            .find_map(|block| block.compiler.as_deref())
    }
}

fn read_lock(dir: &Path) -> anyhow::Result<Vec<Pin>> {
    let path = dir.join(LOCKFILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("Failed to read {}: {e}", path.display()),
    };
    let lock: Lock = serde_json::from_str(&text).with_context(|| format!("{} is not a valid lockfile", path.display()))?;
    Ok(lock.packages)
}

/// The pin of `name` in the lockfile in `dir`; `None` when there is no lockfile or it pins no such package.
pub(crate) fn pinned(dir: &Path, name: &str) -> anyhow::Result<Option<Pin>> {
    Ok(read_lock(dir)?.into_iter().find(|pin| pin.name == name))
}

/// Record `pin` in the lockfile in `dir`, replacing the entry of the same name and keeping every other entry.
pub(crate) fn record(dir: &Path, pin: &Pin) -> anyhow::Result<()> {
    let mut packages: Vec<Pin> = read_lock(dir)?.into_iter().filter(|p| p.name != pin.name).collect();
    packages.push(pin.clone());
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    let path = dir.join(LOCKFILE);
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&Lock { packages })?))
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// A stable `major.minor.patch` as a sortable triple; prereleases and build metadata are not stable.
fn stable(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let triple = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(triple)
}

/// The newest stable, non-deprecated version of the packument whose orb compiler range admits `running`; a version with no
/// range, an unreadable range or no integrity is never picked.
fn pick(name: &str, packument: &str, running: &str) -> anyhow::Result<Pin> {
    let packument: Packument = serde_json::from_str(packument).with_context(|| format!("{name}: the registry answer is not a packument"))?;
    packument
        .versions
        .values()
        .filter(|m| m.deprecated.is_none())
        .filter_map(|m| Some((stable(&m.version)?, m)))
        .filter(|(_, m)| m.compiler().is_some_and(|range| compiler_satisfies(range, running).unwrap_or(false)))
        .filter_map(|(key, m)| Some((key, Pin { name: name.to_string(), version: m.version.clone(), url: m.dist.tarball.clone(), integrity: m.dist.integrity.clone()? })))
        .max_by_key(|(key, _)| *key)
        .map(|(_, pin)| pin)
        .ok_or_else(|| anyhow!("ORB_BOOTSTRAP_NO_COMPATIBLE: no stable version of {name} on {REGISTRY} declares an orb compiler range that admits orb {running}"))
}

/// Verify `tarball` against the pin's integrity and swap it in as `<dir>/node_modules/<name>`: the files are written into a
/// staging directory first (the tarball's `package/` folder dropped), then the old package is removed and the staging
/// directory renamed into place. Nothing under node_modules changes when the digest differs, the archive is refused or
/// an entry lies outside `package/`. The file count.
pub(crate) fn unpack(pin: &Pin, tarball: &[u8], dir: &Path) -> anyhow::Result<usize> {
    let digest = format!("sha512-{}", base64_encode(&sha512(tarball)));
    if digest != pin.integrity {
        bail!("integrity-mismatch: {}@{} hashes to {digest}, expected {}", pin.name, pin.version, pin.integrity);
    }
    let archive = gzip_decompress(tarball, MAX_TARBALL).map_err(|code| anyhow!("{}: the tarball is not gzip ({code})", pin.name))?;
    let files = tar_files(&archive, MAX_ENTRIES).map_err(|code| anyhow!("{}: the tarball is refused ({code})", pin.name))?;
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let Some(relative) = file.path.strip_prefix("package/") else {
            bail!("{}: the tarball entry {:?} is not under package/", pin.name, file.path);
        };
        entries.push((relative.to_string(), file));
    }
    let modules = dir.join("node_modules");
    let staging = modules.join(STAGING).join(format!("{}@{}", pin.name.replace('/', "+"), pin.version));
    remove_dir_if_present(&staging)?;
    for (relative, file) in &entries {
        let path = staging.join(relative);
        std::fs::create_dir_all(path.parent().unwrap_or(&staging)).with_context(|| format!("Failed to create {}", path.display()))?;
        std::fs::write(&path, &file.data).with_context(|| format!("Failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(if file.executable { 0o755 } else { 0o644 }))?;
        }
    }
    let target: PathBuf = modules.join(&pin.name);
    remove_dir_if_present(&target)?;
    std::fs::create_dir_all(target.parent().unwrap_or(&modules)).with_context(|| format!("Failed to create {}", modules.display()))?;
    std::fs::rename(&staging, &target).with_context(|| format!("Failed to move {} to {}", staging.display(), target.display()))?;
    Ok(entries.len())
}

fn remove_dir_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("Failed to remove {}: {e}", path.display())),
    }
}

fn get(url: &str, accept: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("{url} is not an https URL"));
    }
    let response = ureq::get(url).set("accept", accept).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => format!("transport error: {}", t.kind()),
    })?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_TARBALL as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("download interrupted: {:?}", e.kind()))?;
    if bytes.len() > MAX_TARBALL {
        return Err(format!("{url} answered more than the limit"));
    }
    Ok(bytes)
}

/// Install every boot package: from the first of `dirs` whose lockfile pins it, into that directory; else resolved from the
/// registry for orb `running` and installed into, and recorded in, `store`. The pins and file counts, in install order.
pub(crate) fn repair(dirs: &[&Path], store: &Path, running: &str) -> anyhow::Result<Vec<(Pin, usize)>> {
    let mut installed = Vec::new();
    for name in BOOT_PACKAGES {
        let mut found = None;
        for dir in dirs {
            if let Some(pin) = pinned(dir, name)? {
                found = Some((pin, dir.to_path_buf()));
                break;
            }
        }
        let (pin, dir, fresh) = match found {
            Some((pin, dir)) => (pin, dir, false),
            None => {
                let url = format!("{REGISTRY}/{}", name.replace('/', "%2f"));
                let packument = get(&url, "application/json").map_err(|e| anyhow!("{name}: {e}"))?;
                let text = String::from_utf8(packument).map_err(|_| anyhow!("{name}: the registry answer is not text"))?;
                (pick(name, &text, running)?, store.to_path_buf(), true)
            }
        };
        let tarball = get(&pin.url, "application/octet-stream").map_err(|e| anyhow!("{}@{}: {e}", pin.name, pin.version))?;
        let count = unpack(&pin, &tarball, &dir)?;
        if fresh {
            record(&dir, &pin)?;
        }
        installed.push((pin, count));
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbital_core::codec::gzip_compress;

    fn tar_entry(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
        header[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        header[156] = b'0';
        header[148..156].copy_from_slice(b"        ");
        let sum: usize = header.iter().map(|b| usize::from(*b)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut out = header;
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn tarball(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut tar: Vec<u8> = entries.iter().flat_map(|(name, mode, data)| tar_entry(name, *mode, data)).collect();
        tar.extend_from_slice(&[0u8; 1024]);
        gzip_compress(&tar)
    }

    fn pin_of(tarball: &[u8]) -> Pin {
        Pin {
            name: STD_PACKAGE.into(),
            version: "1.0.0".into(),
            url: "https://registry.npmjs.org/@almadar/std/-/std-1.0.0.tgz".into(),
            integrity: format!("sha512-{}", base64_encode(&sha512(tarball))),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orb-bootstrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_verified_tarball_replaces_the_package_without_its_top_folder_and_keeps_the_mode() {
        let dir = scratch("ok");
        let stale = dir.join("node_modules/@almadar/std/stale.lolo");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"old").unwrap();
        let bytes = tarball(&[("package/package.json", 0o644, b"{}"), ("package/behaviors/a.lolo", 0o644, b"app a"), ("package/bin/run", 0o755, b"#!")]);
        assert_eq!(unpack(&pin_of(&bytes), &bytes, &dir).unwrap(), 3);
        assert_eq!(std::fs::read(dir.join("node_modules/@almadar/std/behaviors/a.lolo")).unwrap(), b"app a");
        assert!(!stale.exists(), "the old package is replaced, not overlaid");
        assert!(!dir.join("node_modules").join(STAGING).join("@almadar+std@1.0.0").exists(), "staging is renamed away");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("node_modules/@almadar/std/bin/run")).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(std::fs::metadata(dir.join("node_modules/@almadar/std/package.json")).unwrap().permissions().mode() & 0o777, 0o644);
        }
    }

    #[test]
    fn control_a_different_digest_a_refused_archive_or_an_entry_outside_package_changes_nothing() {
        let dir = scratch("refused");
        let bytes = tarball(&[("package/package.json", 0o644, b"{}")]);
        let mut pin = pin_of(&bytes);
        pin.integrity = format!("sha512-{}", base64_encode(&sha512(b"other")));
        assert!(unpack(&pin, &bytes, &dir).unwrap_err().to_string().starts_with("integrity-mismatch"));
        let escaping = tarball(&[("package/ok.txt", 0o644, b"x"), ("../evil.txt", 0o644, b"x")]);
        assert!(unpack(&pin_of(&escaping), &escaping, &dir).unwrap_err().to_string().contains("refused (path)"));
        let outside = tarball(&[("package/ok.txt", 0o644, b"x"), ("other/x.txt", 0o644, b"x")]);
        assert!(unpack(&pin_of(&outside), &outside, &dir).unwrap_err().to_string().contains("not under package/"));
        assert!(unpack(&pin_of(b"not gzip"), b"not gzip", &dir).is_err());
        assert!(!dir.join("node_modules").exists(), "a refused tarball writes nothing");
    }

    #[test]
    fn the_pick_is_the_newest_stable_undeprecated_version_whose_range_admits_the_running_orb() {
        let packument = r#"{ "versions": {
            "16.0.0": { "version": "16.0.0", "orb": { "compiler": ">=4.10.0" }, "dist": { "tarball": "https://r/std-16.tgz", "integrity": "sha512-a" } },
            "17.0.0": { "version": "17.0.0", "orb": { "compiler": ">=19.0.0" }, "dist": { "tarball": "https://r/std-17.tgz", "integrity": "sha512-b" } },
            "17.1.0": { "version": "17.1.0", "orb": { "compiler": ">=19.0.0" }, "deprecated": "broken", "dist": { "tarball": "https://r/std-171.tgz", "integrity": "sha512-c" } },
            "18.0.0-rc.1": { "version": "18.0.0-rc.1", "orb": { "compiler": ">=19.0.0" }, "dist": { "tarball": "https://r/std-rc.tgz", "integrity": "sha512-d" } },
            "18.0.0": { "version": "18.0.0", "orb": { "compiler": ">=20.0.0" }, "dist": { "tarball": "https://r/std-18.tgz", "integrity": "sha512-e" } },
            "18.1.0": { "version": "18.1.0", "dist": { "tarball": "https://r/std-181.tgz", "integrity": "sha512-f" } }
        } }"#;
        let pin = pick(STD_PACKAGE, packument, "19.0.0").unwrap();
        assert_eq!((pin.version.as_str(), pin.url.as_str(), pin.integrity.as_str()), ("17.0.0", "https://r/std-17.tgz", "sha512-b"));
        assert_eq!(pick(STD_PACKAGE, packument, "18.0.5").unwrap().version, "16.0.0", "control: an older orb gets the version its range admits");
        assert!(pick(STD_PACKAGE, packument, "3.0.0").unwrap_err().to_string().starts_with("ORB_BOOTSTRAP_NO_COMPATIBLE"));
        let core = r#"{ "versions": { "11.0.0": { "version": "11.0.0", "orbLanguage": { "compiler": ">=19.0.0" }, "dist": { "tarball": "https://r/core.tgz", "integrity": "sha512-g" } } } }"#;
        assert_eq!(pick(CORE_PACKAGE, core, "19.0.0").unwrap().version, "11.0.0", "core's range is under orbLanguage");
    }

    #[test]
    fn a_pin_is_read_from_and_recorded_into_the_lockfile_keeping_other_entries() {
        let dir = scratch("lock");
        assert_eq!(pinned(&dir, STD_PACKAGE).unwrap(), None);
        let other = Pin { name: "@almadar/lolo-ar".into(), version: "2.0.0".into(), url: "https://r/ar.tgz".into(), integrity: "sha512-x".into() };
        record(&dir, &other).unwrap();
        let std_pin = Pin { name: STD_PACKAGE.into(), version: "17.0.0".into(), url: "https://r/std.tgz".into(), integrity: "sha512-y".into() };
        record(&dir, &std_pin).unwrap();
        let newer = Pin { version: "17.0.1".into(), ..std_pin };
        record(&dir, &newer).unwrap();
        assert_eq!(pinned(&dir, STD_PACKAGE).unwrap(), Some(newer));
        assert_eq!(pinned(&dir, "@almadar/lolo-ar").unwrap(), Some(other));
        assert_eq!(read_lock(&dir).unwrap().len(), 2);
    }

    #[test]
    fn only_https_urls_are_fetched_and_stable_versions_are_whole_triples() {
        assert!(get("http://registry.example/std.tgz", "application/json").unwrap_err().contains("not an https URL"));
        assert_eq!(stable("17.0.0"), Some((17, 0, 0)));
        assert_eq!(stable("17.0.0-rc.1"), None);
        assert_eq!(stable("17.0"), None);
        assert_eq!(stable("17.0.0.1"), None);
    }
}
