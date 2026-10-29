//! The bun-free repair path of `orb update`: when no installed program answers it (std is absent or broken), the
//! bootloader reinstalls the std package the lockfile pins, then hands the command to `std-orb-packages`.

use anyhow::{anyhow, bail, Context};
use orbital_core::codec::{gzip_decompress, hex, sha256, tar_files};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub(crate) const STD_PACKAGE: &str = "@almadar/std";
const LOCKFILE: &str = "orb.lock.json";
const MAX_TARBALL: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Pin {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) url: String,
    pub(crate) integrity: String,
}

#[derive(serde::Deserialize)]
struct Lock {
    packages: Vec<Pin>,
}

/// The std pin of the lockfile in `dir`; `None` when there is no lockfile or it pins no std.
pub(crate) fn pinned(dir: &Path) -> anyhow::Result<Option<Pin>> {
    let path = dir.join(LOCKFILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("Failed to read {}: {e}", path.display()),
    };
    let lock: Lock = serde_json::from_str(&text).with_context(|| format!("{} is not a valid lockfile", path.display()))?;
    Ok(lock.packages.into_iter().find(|pin| pin.name == STD_PACKAGE))
}

/// Verify `tarball` against the pin's sha256 and unpack it into `<dir>/node_modules/<name>`, dropping the tarball's
/// one top-level folder; nothing is written when the digest differs or a path leaves the package. The file count.
pub(crate) fn unpack(pin: &Pin, tarball: &[u8], dir: &Path) -> anyhow::Result<usize> {
    let digest = hex(&sha256(tarball));
    if digest != pin.integrity {
        bail!("integrity-mismatch: {}@{} hashes to {digest}, the lockfile pins {}", pin.name, pin.version, pin.integrity);
    }
    let archive = gzip_decompress(tarball, MAX_TARBALL).map_err(|code| anyhow!("{}: the tarball is not gzip ({code})", pin.name))?;
    let root = dir.join("node_modules").join(&pin.name);
    let mut files = Vec::new();
    for file in tar_files(&archive).map_err(|e| anyhow!("{}: {e}", pin.name))? {
        let relative: PathBuf = Path::new(&file.path).components().skip(1).collect();
        if relative.as_os_str().is_empty() || relative.components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("{}: the tarball path {:?} leaves the package", pin.name, file.path);
        }
        files.push((root.join(relative), file));
    }
    for (path, file) in &files {
        std::fs::create_dir_all(path.parent().unwrap_or(&root)).with_context(|| format!("Failed to create {}", path.display()))?;
        std::fs::write(path, &file.data).with_context(|| format!("Failed to write {}", path.display()))?;
        #[cfg(unix)]
        if file.executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(files.len())
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("{url} is not an https URL"));
    }
    let response = ureq::get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => format!("transport error: {}", t.kind()),
    })?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_TARBALL as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("download interrupted: {:?}", e.kind()))?;
    if bytes.len() > MAX_TARBALL {
        return Err("the tarball is larger than the limit".to_string());
    }
    Ok(bytes)
}

/// Reinstall the std package the lockfile of the first of `dirs` that pins one names; the pin and the file count.
pub(crate) fn repair(dirs: &[&Path]) -> anyhow::Result<(Pin, usize)> {
    for dir in dirs {
        if let Some(pin) = pinned(dir)? {
            let tarball = download(&pin.url).map_err(|e| anyhow!("{}@{}: {e}", pin.name, pin.version))?;
            let count = unpack(&pin, &tarball, dir)?;
            return Ok((pin, count));
        }
    }
    bail!("ORB_BOOTSTRAP_NO_PIN: no {LOCKFILE} in {} pins {STD_PACKAGE}; install the std package once with the installer", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(" or "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbital_core::codec::gzip_compress;

    fn tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        header[156] = b'0';
        let mut out = header;
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar: Vec<u8> = entries.iter().flat_map(|(name, data)| tar_entry(name, data)).collect();
        tar.extend_from_slice(&[0u8; 1024]);
        gzip_compress(&tar)
    }

    fn pin_of(tarball: &[u8]) -> Pin {
        Pin { name: STD_PACKAGE.into(), version: "1.0.0".into(), url: "https://registry.example/std.tgz".into(), integrity: hex(&sha256(tarball)) }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orb-bootstrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_verified_tarball_unpacks_under_node_modules_without_its_top_folder() {
        let dir = scratch("ok");
        let bytes = tarball(&[("package/package.json", b"{}"), ("package/behaviors/a.lolo", b"app a")]);
        assert_eq!(unpack(&pin_of(&bytes), &bytes, &dir).unwrap(), 2);
        assert_eq!(std::fs::read(dir.join("node_modules/@almadar/std/behaviors/a.lolo")).unwrap(), b"app a");
        assert!(dir.join("node_modules/@almadar/std/package.json").is_file());
    }

    #[test]
    fn control_a_different_digest_or_a_path_leaving_the_package_writes_nothing() {
        let dir = scratch("refused");
        let bytes = tarball(&[("package/package.json", b"{}")]);
        let mut pin = pin_of(&bytes);
        pin.integrity = "0".repeat(64);
        assert!(unpack(&pin, &bytes, &dir).unwrap_err().to_string().starts_with("integrity-mismatch"));
        let escaping = tarball(&[("package/ok.txt", b"x"), ("package/../../evil.txt", b"x")]);
        assert!(unpack(&pin_of(&escaping), &escaping, &dir).unwrap_err().to_string().contains("leaves the package"));
        assert!(!dir.join("node_modules").exists(), "a refused tarball writes nothing");
        assert!(unpack(&pin_of(b"not gzip"), b"not gzip", &dir).is_err());
    }

    #[test]
    fn the_std_pin_is_read_from_the_lockfile_and_absence_is_none() {
        let dir = scratch("lock");
        assert_eq!(pinned(&dir).unwrap(), None);
        std::fs::write(
            dir.join(LOCKFILE),
            r#"{ "packages": [{ "name": "@almadar/core", "version": "1", "url": "u", "integrity": "i" }, { "name": "@almadar/std", "version": "2", "url": "https://x/std.tgz", "integrity": "abc" }] }"#,
        )
        .unwrap();
        assert_eq!(pinned(&dir).unwrap().unwrap().version, "2");
        std::fs::write(dir.join(LOCKFILE), r#"{ "packages": [] }"#).unwrap();
        assert_eq!(pinned(&dir).unwrap(), None);
        assert!(repair(&[dir.as_path()]).unwrap_err().to_string().starts_with("ORB_BOOTSTRAP_NO_PIN"));
    }

    #[test]
    fn only_https_urls_are_fetched() {
        assert!(download("http://registry.example/std.tgz").unwrap_err().contains("not an https URL"));
    }
}
