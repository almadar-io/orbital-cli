//! The bun-free repair path of `orb update`: when no installed program answers it (the store is empty, broken, or does not
//! load on this orb), the bootloader installs the packages the update program itself needs to load, @almadar/core and
//! @almadar/std, then hands the command to `std-orb-packages`. Each comes from the lockfile pin that names it, else from the
//! npm registry: core's newest stable, non-deprecated version providing a language this orb reads (`LANGUAGE_READS`),
//! then std's newest whose `language` range admits the language that core provides (P-022).

use anyhow::{anyhow, bail, Context};
use orbital_core::codec::{base64_encode, gzip_decompress, sha512, tar_files};
use orbital_core::language_data::{language_satisfies, CORE_PACKAGE, LANGUAGE_READS, STD_PACKAGE};
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
struct LanguageBlock {
    /// The language version core provides.
    version: Option<String>,
    /// The language versions a package is written for.
    language: Option<String>,
}

#[derive(serde::Deserialize)]
struct Dist {
    tarball: String,
    integrity: Option<String>,
}

/// The blocks that carry the language contract: `orbLanguage.version` on core, a `language` range on every other block.
#[derive(serde::Deserialize, Default)]
struct Contract {
    orb: Option<LanguageBlock>,
    #[serde(rename = "orbLanguage")]
    orb_language: Option<LanguageBlock>,
    #[serde(rename = "orbVocabulary")]
    orb_vocabulary: Option<LanguageBlock>,
    #[serde(rename = "orbProgram")]
    orb_program: Option<LanguageBlock>,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    deprecated: Option<String>,
    #[serde(flatten)]
    contract: Contract,
    dist: Dist,
}

/// What a picked version must meet: core provides a language this orb reads; any other package's range admits the
/// language the installed core provides.
pub(crate) enum Need<'a> {
    Reads,
    Language(&'a str),
}

impl Contract {
    fn blocks(&self) -> impl Iterator<Item = &LanguageBlock> {
        [&self.orb, &self.orb_language, &self.orb_vocabulary, &self.orb_program].into_iter().flatten()
    }

    fn meets(&self, need: &Need<'_>) -> bool {
        match need {
            Need::Reads => self
                .orb_language
                .as_ref()
                .and_then(|b| b.version.as_deref())
                .is_some_and(|language| language_satisfies(LANGUAGE_READS, language).unwrap_or(false)),
            Need::Language(language) => self
                .blocks()
                .find_map(|b| b.language.as_deref())
                .is_some_and(|range| language_satisfies(range, language).unwrap_or(false)),
        }
    }

    /// Whether the manifest predates the language contract: core without `orbLanguage.version`, or any other package
    /// with a block that declares no `language` range.
    fn predates(&self, name: &str) -> bool {
        if name == CORE_PACKAGE {
            return self.orb_language.as_ref().and_then(|b| b.version.as_deref()).is_none();
        }
        self.blocks().any(|b| b.language.is_none())
    }
}

/// The contract blocks of the package `name` installed under `dir`; `None` when it is not installed there.
fn installed_contract(dir: &Path, name: &str) -> anyhow::Result<Option<Contract>> {
    let path = dir.join("node_modules").join(name).join("package.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("Failed to read {}: {e}", path.display()),
    };
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?))
}

/// The packages the lockfile in `dir` pins, other than the boot packages, whose installed manifest predates the language
/// contract (P-022): the ones a repair replaces.
fn predating(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for pin in read_lock(dir)? {
        if BOOT_PACKAGES.contains(&pin.name.as_str()) {
            continue;
        }
        if installed_contract(dir, &pin.name)?.is_some_and(|c| c.predates(&pin.name)) {
            out.push(pin.name);
        }
    }
    Ok(out)
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

/// The newest stable, non-deprecated version of the packument that meets `need`; a version declaring no language, an
/// unreadable one or no integrity is never picked.
fn pick(name: &str, packument: &str, need: &Need<'_>) -> anyhow::Result<Pin> {
    let packument: Packument = serde_json::from_str(packument).with_context(|| format!("{name}: the registry answer is not a packument"))?;
    packument
        .versions
        .values()
        .filter(|m| m.deprecated.is_none())
        .filter_map(|m| Some((stable(&m.version)?, m)))
        .filter(|(_, m)| m.contract.meets(need))
        .filter_map(|(key, m)| Some((key, Pin { name: name.to_string(), version: m.version.clone(), url: m.dist.tarball.clone(), integrity: m.dist.integrity.clone()? })))
        .max_by_key(|(key, _)| *key)
        .map(|(_, pin)| pin)
        .ok_or_else(|| match need {
            Need::Reads => anyhow!("ORB_BOOTSTRAP_NO_COMPATIBLE: no stable version of {name} on {REGISTRY} provides a language this orb reads ({LANGUAGE_READS})"),
            Need::Language(language) => anyhow!("ORB_BOOTSTRAP_NO_COMPATIBLE: no stable version of {name} on {REGISTRY} declares a language range that admits language {language}"),
        })
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
    std::fs::create_dir_all(target.parent().unwrap_or(&modules)).with_context(|| format!("Failed to create {}", modules.display()))?;
    swap_in(&staging, &target, &staging.with_extension("previous"))?;
    Ok(entries.len())
}

/// Move `staging` to `target`, keeping the package already at `target` as `backup` until the move succeeded: a failed
/// move puts it back, so the store never holds neither version.
fn swap_in(staging: &Path, target: &Path, backup: &Path) -> anyhow::Result<()> {
    remove_dir_if_present(backup)?;
    let had_previous = target.exists();
    if had_previous {
        std::fs::rename(target, backup).with_context(|| format!("Failed to set {} aside", target.display()))?;
    }
    if let Err(e) = std::fs::rename(staging, target) {
        if had_previous {
            std::fs::rename(backup, target)
                .with_context(|| format!("Failed to restore {} after a failed install ({e}); it is at {}", target.display(), backup.display()))?;
        }
        bail!("Failed to move {} to {}: {e}; the installed version is kept", staging.display(), target.display());
    }
    remove_dir_if_present(backup)
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

/// Set on the run an older orb hands to a newer installed one, so the newer one never hands off again; a user sets it to
/// run this binary as it is.
pub(crate) const HANDED_OFF_ENV: &str = "ORB_HANDED_OFF";

/// This platform's orb package, as `@almadar/orb` installs it.
pub(crate) fn platform_package() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("@almadar/orb-darwin-arm64"),
        ("macos", "x86_64") => Some("@almadar/orb-darwin-x64"),
        ("linux", "x86_64") => Some("@almadar/orb-linux-x64"),
        ("linux", "aarch64") => Some("@almadar/orb-linux-arm64"),
        ("windows", "x86_64") => Some("@almadar/orb-windows-x64"),
        _ => None,
    }
}

/// The orb binary `store` holds for `package` when it is a newer stable release than `own`: the bootloader hands the run
/// to it (P-022: orb updates itself as a package, installed by `orb update` into the user store).
pub(crate) fn newer_orb(store: &Path, package: &str, own: &str) -> Option<PathBuf> {
    let dir = store.join("node_modules").join(package);
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
    let installed = manifest.get("version")?.as_str()?;
    let binary = dir.join(if cfg!(windows) { "orb.exe" } else { "orb" });
    (stable(installed)? > stable(own)? && binary.is_file()).then_some(binary)
}

/// The registry's packument of `name`, as text.
fn packument(name: &str) -> anyhow::Result<String> {
    let url = format!("{REGISTRY}/{}", name.replace('/', "%2f"));
    let bytes = get(&url, "application/json").map_err(|e| anyhow!("{name}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("{name}: the registry answer is not text"))
}

/// Download `pin` and swap it in under `dir`; the file count.
fn install(pin: &Pin, dir: &Path) -> anyhow::Result<usize> {
    let tarball = get(&pin.url, "application/octet-stream").map_err(|e| anyhow!("{}@{}: {e}", pin.name, pin.version))?;
    unpack(pin, &tarball, dir)
}

/// Install every boot package: from the first of `dirs` whose lockfile pins it, when what that pin installs meets the
/// language contract; else resolved from the registry into that directory (or `store` when nothing pins it) and recorded
/// there. Then every package an `orb.lock.json` in `dirs` pins whose installed manifest predates the contract is replaced
/// by the registry's newest version whose `language` range admits the installed core's language. The pins and file
/// counts, in install order.
pub(crate) fn repair(dirs: &[&Path], store: &Path) -> anyhow::Result<Vec<(Pin, usize)>> {
    let start = dirs.first().copied().unwrap_or(store);
    let core_language = || {
        orbital_core::language_data::installed_language(start, store)
            .map_err(|e| anyhow!("the installed {CORE_PACKAGE} does not answer its language: {e}"))
    };
    let mut installed = Vec::new();
    for name in BOOT_PACKAGES {
        let language = if name == CORE_PACKAGE { None } else { Some(core_language()?) };
        let need = language.as_deref().map_or(Need::Reads, Need::Language);
        let mut target = store.to_path_buf();
        let mut found = None;
        for dir in dirs {
            if let Some(pin) = pinned(dir, name)? {
                found = Some((pin, dir.to_path_buf()));
                break;
            }
        }
        if let Some((pin, dir)) = found {
            let count = install(&pin, &dir)?;
            let meets = installed_contract(&dir, name)?.is_some_and(|c| c.meets(&need));
            installed.push((pin, count));
            if meets {
                continue;
            }
            target = dir;
        }
        let pin = pick(name, &packument(name)?, &need)?;
        let count = install(&pin, &target)?;
        record(&target, &pin)?;
        installed.push((pin, count));
    }
    let language = core_language()?;
    let mut swept: Vec<&Path> = Vec::new();
    for dir in dirs.iter().copied().chain([store]) {
        if swept.contains(&dir) {
            continue;
        }
        swept.push(dir);
        for name in predating(dir)? {
            let pin = pick(&name, &packument(&name)?, &Need::Language(&language))?;
            let count = install(&pin, dir)?;
            record(dir, &pin)?;
            installed.push((pin, count));
        }
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
    fn std_is_the_newest_stable_undeprecated_version_whose_language_range_admits_the_installed_core() {
        let packument = r#"{ "versions": {
            "16.0.0": { "version": "16.0.0", "orb": { "language": "^1" }, "dist": { "tarball": "https://r/std-16.tgz", "integrity": "sha512-a" } },
            "17.0.0": { "version": "17.0.0", "orb": { "language": "^1.3" }, "dist": { "tarball": "https://r/std-17.tgz", "integrity": "sha512-b" } },
            "17.1.0": { "version": "17.1.0", "orb": { "language": "^1.3" }, "deprecated": "broken", "dist": { "tarball": "https://r/std-171.tgz", "integrity": "sha512-c" } },
            "18.0.0-rc.1": { "version": "18.0.0-rc.1", "orb": { "language": "^1.3" }, "dist": { "tarball": "https://r/std-rc.tgz", "integrity": "sha512-d" } },
            "18.0.0": { "version": "18.0.0", "orb": { "language": "^2" }, "dist": { "tarball": "https://r/std-18.tgz", "integrity": "sha512-e" } },
            "18.1.0": { "version": "18.1.0", "orb": { "compiler": ">=19.0.0" }, "dist": { "tarball": "https://r/std-181.tgz", "integrity": "sha512-f" } }
        } }"#;
        let pin = pick(STD_PACKAGE, packument, &Need::Language("1.4.0")).unwrap();
        assert_eq!((pin.version.as_str(), pin.url.as_str(), pin.integrity.as_str()), ("17.0.0", "https://r/std-17.tgz", "sha512-b"));
        assert_eq!(pick(STD_PACKAGE, packument, &Need::Language("1.2.0")).unwrap().version, "16.0.0", "control: an older language gets the version its range admits");
        assert!(pick(STD_PACKAGE, packument, &Need::Language("3.0.0")).unwrap_err().to_string().starts_with("ORB_BOOTSTRAP_NO_COMPATIBLE"));
    }

    #[test]
    fn core_is_the_newest_version_providing_a_language_this_orb_reads_whatever_its_release() {
        let core = r#"{ "versions": {
            "11.0.0": { "version": "11.0.0", "orbLanguage": { "version": "1.0.0" }, "dist": { "tarball": "https://r/core-11.tgz", "integrity": "sha512-g" } },
            "12.0.0": { "version": "12.0.0", "orbLanguage": { "version": "1.4.0" }, "dist": { "tarball": "https://r/core-12.tgz", "integrity": "sha512-h" } },
            "13.0.0": { "version": "13.0.0", "orbLanguage": { "version": "2.0.0" }, "dist": { "tarball": "https://r/core-13.tgz", "integrity": "sha512-i" } },
            "14.0.0": { "version": "14.0.0", "orbLanguage": { "compiler": ">=17.0.0" }, "dist": { "tarball": "https://r/core-14.tgz", "integrity": "sha512-j" } }
        } }"#;
        assert_eq!(pick(CORE_PACKAGE, core, &Need::Reads).unwrap().version, "12.0.0", "a language outside LANGUAGE_READS or none at all is never picked");
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

    fn installed(dir: &Path, name: &str, manifest: serde_json::Value) {
        let pkg = dir.join("node_modules").join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), manifest.to_string()).unwrap();
        record(dir, &Pin { name: name.to_string(), version: "1.0.0".into(), url: "https://r/x.tgz".into(), integrity: "sha512-x".into() }).unwrap();
    }

    #[test]
    fn a_repair_replaces_every_pinned_package_whose_manifest_predates_the_language_contract() {
        let dir = tempfile::tempdir().unwrap();
        installed(dir.path(), CORE_PACKAGE, serde_json::json!({ "name": CORE_PACKAGE, "orbLanguage": { "compiler": ">=17.0.0" } }));
        installed(dir.path(), STD_PACKAGE, serde_json::json!({ "name": STD_PACKAGE, "orb": { "prefix": "almadar-std", "compiler": ">=4.10.0" } }));
        installed(dir.path(), "@almadar/lolo-ar", serde_json::json!({ "name": "@almadar/lolo-ar", "orbVocabulary": { "compiler": ">=17.11.2" } }));
        installed(dir.path(), "@almadar/orb-shell-typescript", serde_json::json!({ "name": "s", "orbProgram": { "kind": "shell", "compiler": ">=18.0.0" } }));
        installed(dir.path(), "@x/behaviors", serde_json::json!({ "name": "@x/behaviors", "orb": { "prefix": "x", "language": "^1" } }));
        installed(dir.path(), "@x/plain", serde_json::json!({ "name": "@x/plain" }));
        assert_eq!(
            predating(dir.path()).unwrap(),
            vec!["@almadar/lolo-ar".to_string(), "@almadar/orb-shell-typescript".to_string()],
            "boot packages are repaired by their own step; a package on the contract or with no orb block is kept"
        );
    }

    #[test]
    fn control_a_directory_without_a_lockfile_is_never_swept() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules/@almadar/lolo-ar");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{ "orbVocabulary": { "compiler": ">=17.11.2" } }"#).unwrap();
        assert!(predating(dir.path()).unwrap().is_empty(), "only what orb installed and pinned is orb's to replace");
    }

    #[test]
    fn an_installed_boot_package_meets_the_contract_only_with_its_contract_field() {
        let dir = tempfile::tempdir().unwrap();
        installed(dir.path(), CORE_PACKAGE, serde_json::json!({ "orbLanguage": { "compiler": ">=17.0.0" } }));
        let old = installed_contract(dir.path(), CORE_PACKAGE).unwrap().unwrap();
        assert!(old.predates(CORE_PACKAGE) && !old.meets(&Need::Reads));
        installed(dir.path(), CORE_PACKAGE, serde_json::json!({ "orbLanguage": { "version": "1.0.0" } }));
        let new = installed_contract(dir.path(), CORE_PACKAGE).unwrap().unwrap();
        assert!(!new.predates(CORE_PACKAGE) && new.meets(&Need::Reads));
        installed(dir.path(), STD_PACKAGE, serde_json::json!({ "orb": { "prefix": "s", "language": "^1" }, "orbLanguage": { "language": "^1" } }));
        let std = installed_contract(dir.path(), STD_PACKAGE).unwrap().unwrap();
        assert!(std.meets(&Need::Language("1.0.0")) && !std.meets(&Need::Language("2.0.0")));
        assert!(installed_contract(dir.path(), "@x/absent").unwrap().is_none(), "edge: not installed is none, not an error");
    }

    fn store_orb(store: &Path, version: &str, with_binary: bool) {
        let dir = store.join("node_modules/@almadar/orb-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), serde_json::json!({ "name": "@almadar/orb-test", "version": version }).to_string()).unwrap();
        let binary = dir.join(if cfg!(windows) { "orb.exe" } else { "orb" });
        if with_binary {
            std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        } else {
            let _ = std::fs::remove_file(&binary);
        }
    }

    #[test]
    fn a_newer_stable_orb_in_the_store_is_the_one_a_run_hands_off_to() {
        let store = tempfile::tempdir().unwrap();
        store_orb(store.path(), "19.1.0", true);
        let found = newer_orb(store.path(), "@almadar/orb-test", "19.0.0").unwrap();
        assert!(found.ends_with(if cfg!(windows) { "orb.exe" } else { "orb" }));
    }

    #[test]
    fn control_the_same_or_an_older_release_keeps_this_binary() {
        let store = tempfile::tempdir().unwrap();
        store_orb(store.path(), "19.0.0", true);
        assert!(newer_orb(store.path(), "@almadar/orb-test", "19.0.0").is_none());
        store_orb(store.path(), "18.9.9", true);
        assert!(newer_orb(store.path(), "@almadar/orb-test", "19.0.0").is_none());
    }

    #[test]
    fn edge_a_prerelease_a_missing_binary_or_no_package_is_never_handed_off_to() {
        let store = tempfile::tempdir().unwrap();
        assert!(newer_orb(store.path(), "@almadar/orb-test", "19.0.0").is_none(), "nothing installed");
        store_orb(store.path(), "20.0.0-rc.1", true);
        assert!(newer_orb(store.path(), "@almadar/orb-test", "19.0.0").is_none(), "a prerelease is not stable");
        store_orb(store.path(), "20.0.0", false);
        assert!(newer_orb(store.path(), "@almadar/orb-test", "19.0.0").is_none(), "a package without its binary");
    }

    #[test]
    fn a_swap_replaces_the_package_and_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, target, backup) = (dir.path().join("staging"), dir.path().join("pkg"), dir.path().join("pkg.previous"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("v"), "new").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("v"), "old").unwrap();
        swap_in(&staging, &target, &backup).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("v")).unwrap(), "new");
        assert!(!backup.exists() && !staging.exists());
    }

    #[test]
    fn control_a_failed_move_restores_the_installed_package() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, target, backup) = (dir.path().join("missing-staging"), dir.path().join("pkg"), dir.path().join("pkg.previous"));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("v"), "old").unwrap();
        let error = swap_in(&staging, &target, &backup).unwrap_err().to_string();
        assert!(error.contains("the installed version is kept"), "{error}");
        assert_eq!(std::fs::read_to_string(target.join("v")).unwrap(), "old", "the old version is back in place");
        assert!(!backup.exists());
    }

    #[test]
    fn edge_a_first_install_has_nothing_to_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, target, backup) = (dir.path().join("staging"), dir.path().join("pkg"), dir.path().join("pkg.previous"));
        std::fs::create_dir_all(&staging).unwrap();
        swap_in(&staging, &target, &backup).unwrap();
        assert!(target.is_dir() && !backup.exists());
        assert!(swap_in(&dir.path().join("gone"), &dir.path().join("fresh"), &backup).is_err(), "a failed first install leaves nothing");
        assert!(!dir.path().join("fresh").exists());
    }
}
