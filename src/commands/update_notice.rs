//! The daily "updates available" notice. Once a day (cached in the user store)
//! `orb` asks the npm registry for the newest version of each orb package
//! installed for the user, and prints one line on stderr when a newer one exists
//! within its declared range — what `orb update` would install. Off with
//! `ORB_NO_UPDATE_CHECK=1`, and in CI (`CI` set) or when stderr is not a terminal.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: u64 = 24 * 60 * 60;
const CACHE: &str = ".update-check.json";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    checked_at: u64,
    /// `(package, installed, newest in range)` found at the last check.
    available: Vec<(String, String, String)>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn cache_path() -> Option<PathBuf> {
    orbital_core::behavior_packages::user_store_dir().map(|d| d.join(CACHE))
}

fn read() -> Cache {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write(cache: &Cache) {
    if let (Some(path), Ok(json)) = (cache_path(), serde_json::to_string(cache)) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(path, json).ok();
    }
}

/// After `orb update`: everything is current as of now.
pub(crate) fn record_checked() {
    write(&Cache { checked_at: now(), available: Vec::new() });
}

fn enabled() -> bool {
    std::env::var_os("ORB_NO_UPDATE_CHECK").is_none() && std::env::var_os("CI").is_none() && std::io::stderr().is_terminal()
}

/// The newest published version of `name` that satisfies `range`.
fn newest_in_range(name: &str, range: &semver::VersionReq) -> Option<semver::Version> {
    let url = format!("https://registry.npmjs.org/{}", name.replace('/', "%2F"));
    let doc: serde_json::Value = ureq::get(&url)
        .set("Accept", "application/vnd.npm.install-v1+json")
        .timeout(Duration::from_secs(2))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    doc.get("versions")?
        .as_object()?
        .keys()
        .filter_map(|v| semver::Version::parse(v).ok())
        .filter(|v| v.pre.is_empty() && range.matches(v))
        .max()
}

/// The user store's declared orb packages with their installed version and range.
fn installed() -> Vec<(String, semver::Version, semver::VersionReq)> {
    let Some(store) = orbital_core::behavior_packages::user_store_dir() else { return Vec::new() };
    let Ok(raw) = std::fs::read_to_string(store.join("package.json")) else { return Vec::new() };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&raw) else { return Vec::new() };
    let Some(deps) = manifest.get("dependencies").and_then(|d| d.as_object()) else { return Vec::new() };
    deps.iter()
        .filter(|(name, _)| name.starts_with("@almadar/"))
        .filter_map(|(name, range)| {
            let range = semver::VersionReq::parse(range.as_str()?).ok()?;
            let pkg = orbital_core::behavior_packages::find_npm_package(name, &store, &store)?;
            let version = semver::Version::parse(&pkg.version).ok()?;
            Some((name.clone(), version, range))
        })
        .collect()
}

/// Print the notice when one is due.
pub(crate) fn notice() {
    if !enabled() {
        return;
    }
    let mut cache = read();
    if now().saturating_sub(cache.checked_at) >= DAY {
        cache = Cache {
            checked_at: now(),
            available: installed()
                .into_iter()
                .filter_map(|(name, version, range)| {
                    let newest = newest_in_range(&name, &range)?;
                    (newest > version).then(|| (name, version.to_string(), newest.to_string()))
                })
                .collect(),
        };
        write(&cache);
    }
    if cache.available.is_empty() {
        return;
    }
    let list: Vec<String> = cache.available.iter().map(|(n, from, to)| format!("{n} {from} → {to}")).collect();
    eprintln!("orb: updates available ({}); run `orb update --global`", list.join(", "));
}
