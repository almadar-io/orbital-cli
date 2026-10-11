//! `orb update` · `orb add` · `orb remove` · `orb list`. Everything `orb` reads
//! is an npm package that declares its kind in its `package.json`:
//! `orbLanguage` (language data, the operator table), `orb` (behaviors),
//! `orbVocabulary` (a language pack) or `orbProgram` (a shell, or the desktop
//! program). They install into the project (or `--global`, the user store
//! `~/.orb`) with bun; each kind is checked by the loader that reads it.

use super::npm_project::{discover_from, project_dir, run_bun};
use crate::baked_engine_pins::{BAKED_CORE, BAKED_LANGUAGE_PACKS, BAKED_PROGRAMS, BAKED_STD_RANGE};
use orbital_core::behavior_packages::{find_npm_package, user_store_dir};
use orbital_core::desktop_protocol::{self, DesktopReply, DesktopRequest};
use orbital_core::language_data::{load_operator_data, CORE_PACKAGE};
use orbital_core::shell_protocol;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Kind {
    Language,
    Behaviors,
    LanguagePack,
    Shell,
    Desktop,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Language => "language",
            Kind::Behaviors => "behaviors",
            Kind::LanguagePack => "language pack",
            Kind::Shell => "shell",
            Kind::Desktop => "desktop",
        }
    }
}

/// The kinds a package declares, from its `package.json`.
pub(crate) fn kinds(manifest: &serde_json::Value) -> Vec<Kind> {
    let mut out = Vec::new();
    if manifest.get("orbLanguage").is_some() {
        out.push(Kind::Language);
    }
    if manifest.get("orb").is_some() {
        out.push(Kind::Behaviors);
    }
    if manifest.get("orbVocabulary").is_some() {
        out.push(Kind::LanguagePack);
    }
    match manifest.pointer("/orbProgram/kind").and_then(|k| k.as_str()) {
        Some("shell") => out.push(Kind::Shell),
        Some("desktop") => out.push(Kind::Desktop),
        _ => {}
    }
    out
}

#[derive(Serialize)]
struct Row {
    package: String,
    kinds: Vec<Kind>,
    version: String,
    path: String,
    link: bool,
    /// A behavior package's `orb.prefix` (what `uses … from "<prefix>/…"` names).
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    /// How many behaviors a behavior package carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    behaviors: Option<usize>,
    /// `None` when every kind it declares loads for this orb; else why not.
    problem: Option<String>,
}

fn store_for(dir: &Path) -> PathBuf {
    user_store_dir().unwrap_or_else(|| dir.to_path_buf())
}

/// The packages installed for `dir`: those its `package.json` depends on, and
/// those present in its `node_modules` (`name` and `@scope/name`).
fn installed_names(dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    let raw = std::fs::read_to_string(dir.join("package.json"))?;
    let manifest: serde_json::Value = serde_json::from_str(&raw)?;
    let mut names: BTreeSet<String> = ["dependencies", "devDependencies", "optionalDependencies"]
        .iter()
        .filter_map(|k| manifest.get(*k).and_then(|d| d.as_object()))
        .flat_map(|d| d.keys().cloned())
        .collect();
    let entries = |p: &Path| -> Vec<String> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| !n.starts_with('.'))
            .collect()
    };
    let modules = dir.join("node_modules");
    for entry in entries(&modules) {
        if entry.starts_with('@') {
            names.extend(entries(&modules.join(&entry)).into_iter().map(|n| format!("{entry}/{n}")));
        } else {
            names.insert(entry);
        }
    }
    Ok(names)
}

/// Why `kind` of the package at `root` does not load for this orb, if it does not.
fn problem(kind: Kind, name: &str, root: &Path, dir: &Path) -> Option<String> {
    let store = store_for(dir);
    match kind {
        Kind::Language if name == CORE_PACKAGE => super::language::check_core(dir).err(),
        Kind::Language => load_operator_data(dir, &store).err().map(|e| format!("{}: {e}", e.code())),
        Kind::Behaviors => {
            let packages = match discover_from(dir) {
                Ok(p) => p,
                Err(e) => return Some(e.to_string()),
            };
            match packages.iter().find(|p| p.name == name) {
                Some(pkg) => super::behaviors::verify_package(pkg, &packages).err().map(|e| e.to_string()),
                None => Some(format!("{name} declares an `orb` block, but behavior-package discovery did not find it")),
            }
        }
        Kind::LanguagePack => match super::language::check_packs(dir) {
            Ok(packs) if packs.iter().any(|p| p.starts_with(&format!("{name}@"))) => None,
            Ok(_) => Some(format!("{name} declares `orbVocabulary`, but it is not a loadable language pack")),
            Err(e) => Some(e),
        },
        Kind::Shell => {
            let manifest = read_manifest(root).ok()?;
            let shell = manifest.pointer("/orbProgram/name").and_then(|n| n.as_str())?;
            let binary = shell_protocol::find_shell(shell, None, dir, &store).map_err(|f| f.to_string());
            let described = binary.and_then(|b| shell_protocol::describe(&b).map_err(|f| f.to_string()));
            match described {
                Ok(d) if d.ir_versions.accepts(shell_protocol::IR_VERSION) => None,
                Ok(d) => Some(format!(
                    "the {} shell {} accepts IR {}..={}, this orb emits IR {}",
                    d.name, d.version, d.ir_versions.min, d.ir_versions.max, shell_protocol::IR_VERSION
                )),
                Err(e) => Some(e),
            }
        }
        Kind::Desktop => {
            let binary = match shell_protocol::find_program(&desktop_protocol::program(), None, dir, &store) {
                Ok(b) => b,
                Err(f) => return Some(f.to_string()),
            };
            match desktop_protocol::request(&binary, DesktopRequest::Coverage { renderer: "dioxus".to_string() }) {
                Ok(DesktopReply::Coverage { .. }) => None,
                Ok(other) => Some(format!("orb-desktop answered {other:?} to a coverage request")),
                Err(f) => Some(f.to_string()),
            }
        }
    }
}

fn read_manifest(root: &Path) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(root.join("package.json"))?)?)
}

/// Every orb package `dir` depends on, with whether it loads.
fn rows(dir: &Path) -> anyhow::Result<Vec<Row>> {
    let store = store_for(dir);
    let mut out = Vec::new();
    for name in installed_names(dir)? {
        let Some(pkg) = find_npm_package(&name, dir, &store) else { continue };
        let Ok(manifest) = read_manifest(&pkg.root) else { continue };
        let kinds = kinds(&manifest);
        if kinds.is_empty() {
            continue;
        }
        let problems: Vec<String> = kinds.iter().filter_map(|k| problem(*k, &name, &pkg.root, dir)).collect();
        let behavior_package = kinds
            .contains(&Kind::Behaviors)
            .then(|| discover_from(dir).ok()?.into_iter().find(|p| p.name == name))
            .flatten();
        out.push(Row {
            prefix: behavior_package.as_ref().map(|p| p.prefix.clone()),
            behaviors: behavior_package.as_ref().map(|p| orbital_compiler::loader::index_registry_source(&p.registry).len()),
            package: name,
            kinds,
            version: pkg.version.clone(),
            path: pkg.root.display().to_string(),
            link: pkg.is_link,
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
        });
    }
    Ok(out)
}

/// What a working orb needs, as bun specs: the language data it pins, std within
/// its major (the operator table), the first-party language packs and the default
/// programs (the TypeScript shell, orb-desktop).
fn baseline() -> Vec<(String, String)> {
    let (core, core_pin) = BAKED_CORE;
    let (std, std_range) = BAKED_STD_RANGE;
    [(core, core_pin), (std, std_range)]
        .into_iter()
        .chain(BAKED_LANGUAGE_PACKS.iter().copied())
        .chain(BAKED_PROGRAMS.iter().copied())
        .map(|(name, range)| (name.to_string(), format!("{name}@{range}")))
        .collect()
}

fn print_rows(rows: &[Row]) {
    for row in rows {
        let kinds: Vec<&str> = row.kinds.iter().map(|k| k.label()).collect();
        let behaviors = match (&row.prefix, row.behaviors) {
            (Some(prefix), Some(n)) => format!(" [{prefix}/, {n} behaviors]"),
            _ => String::new(),
        };
        println!(
            "{:<34} {:<12} {:<26} {}{}{behaviors}",
            row.package,
            row.version,
            kinds.join(", "),
            row.problem.as_deref().unwrap_or("ok"),
            if row.link { " (linked)" } else { "" }
        );
    }
}

fn fail_on_problems(rows: &[Row]) -> anyhow::Result<()> {
    let problems: Vec<String> = rows
        .iter()
        .filter_map(|r| r.problem.as_ref().map(|p| format!("{}: {p}", r.package)))
        .collect();
    if problems.is_empty() {
        return Ok(());
    }
    anyhow::bail!("these packages do not load for this orb:\n  {}", problems.join("\n  "))
}

/// Install what orb needs and is missing, update every orb package within its
/// range, then check each one loads. Also the first-run setup.
pub(crate) fn run_update(global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let store = store_for(&dir);
    let missing: Vec<(String, String)> = baseline()
        .into_iter()
        .filter(|(name, _)| find_npm_package(name, &dir, &store).is_none())
        .collect();
    if !missing.is_empty() {
        let mut args = vec!["add"];
        args.extend(missing.iter().map(|(_, spec)| spec.as_str()));
        run_bun(&dir, &args)?;
        let absent: Vec<&str> = missing
            .iter()
            .filter(|(name, _)| find_npm_package(name, &dir, &store).is_none())
            .map(|(_, spec)| spec.as_str())
            .collect();
        if !absent.is_empty() {
            anyhow::bail!("bun did not install {}", absent.join(", "));
        }
    }
    // A linked package (a workspace checkout) is never updated from a registry.
    let ours: Vec<String> = rows(&dir)?.into_iter().filter(|r| !r.link).map(|r| r.package).collect();
    if !ours.is_empty() {
        let mut args = vec!["update"];
        args.extend(ours.iter().map(String::as_str));
        run_bun(&dir, &args)?;
    }
    let rows = rows(&dir)?;
    print_rows(&rows);
    // What this call added and does not load is removed again; what was there is reported.
    let undo: Vec<&str> = rows
        .iter()
        .filter(|r| r.problem.is_some() && missing.iter().any(|(name, _)| *name == r.package))
        .map(|r| r.package.as_str())
        .collect();
    if !undo.is_empty() {
        let mut args = vec!["remove"];
        args.extend(undo);
        run_bun(&dir, &args).ok();
    }
    super::update_notice::record_checked();
    fail_on_problems(&rows)
}

/// Install packages (bun specs), then check each declares an orb kind and loads;
/// a package that does not is removed again if this call added it.
pub(crate) fn run_add(specs: Vec<String>, global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let store = store_for(&dir);
    let before = installed_names(&dir)?;
    let named: Vec<String> = specs.iter().map(|s| super::language::package_name(s).to_string()).collect();
    let mut args = vec!["add"];
    args.extend(specs.iter().map(String::as_str));
    run_bun(&dir, &args)?;
    // What appeared is what this call added (a path or tarball spec names no package).
    let new: Vec<String> = installed_names(&dir)?.difference(&before).cloned().collect();
    let appeared = new.clone();
    let names: Vec<String> = named
        .into_iter()
        .filter(|n| find_npm_package(n, &dir, &store).is_some())
        .chain(appeared)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    if names.is_empty() {
        anyhow::bail!("bun installed nothing for {}", specs.join(" "));
    }
    let rows = rows(&dir)?;
    let mut refused = Vec::new();
    for name in &names {
        match rows.iter().find(|r| &r.package == name) {
            Some(row) => match &row.problem {
                None => println!("added {}@{} ({})", row.package, row.version, row.kinds.iter().map(|k| k.label()).collect::<Vec<_>>().join(", ")),
                Some(p) => refused.push(format!("{name}: {p}")),
            },
            None => refused.push(format!(
                "{name}: not an orb package (its package.json declares none of orbLanguage, orb, orbVocabulary, orbProgram)"
            )),
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    let undo: Vec<&str> = new.iter().map(String::as_str).filter(|n| refused.iter().any(|r| r.starts_with(&format!("{n}:")))).collect();
    if !undo.is_empty() {
        let mut args = vec!["remove"];
        args.extend(undo);
        run_bun(&dir, &args).ok();
    }
    anyhow::bail!("not added:\n  {}", refused.join("\n  "))
}

/// The `.lolo`/`.orb` files under `dir` that import from `prefix/`.
fn references_prefix(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let needle = format!("\"{prefix}/");
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("node_modules" | ".git" | "dist" | "target")) {
                    stack.push(path);
                }
                continue;
            }
            let program = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e == "lolo" || e == "orb");
            if program && std::fs::read_to_string(&path).is_ok_and(|c| c.contains(&needle)) {
                hits.push(path);
            }
        }
    }
    hits.sort();
    hits
}

/// Uninstall packages. A behavior package the project's programs still import is refused.
pub(crate) fn run_remove(names: Vec<String>, global: bool) -> anyhow::Result<()> {
    let dir = project_dir(global)?;
    let packages = discover_from(&dir)?;
    for name in &names {
        let Some(pkg) = packages.iter().find(|p| &p.name == name) else { continue };
        let referencing = references_prefix(&dir, &pkg.prefix);
        if !referencing.is_empty() {
            let files: Vec<String> = referencing.iter().map(|p| p.display().to_string()).collect();
            anyhow::bail!(
                "refusing to remove '{name}': {} file(s) still import '{}/':\n  {}",
                files.len(),
                pkg.prefix,
                files.join("\n  ")
            );
        }
    }
    let mut args = vec!["remove"];
    args.extend(names.iter().map(String::as_str));
    run_bun(&dir, &args)?;
    for name in &names {
        println!("removed {name}");
    }
    Ok(())
}

/// Every orb package installed for the project (or `--global`), with its kinds and status.
pub(crate) fn run_list(json: bool, global: bool) -> anyhow::Result<()> {
    let dir = if global { project_dir(true)? } else { project_dir(false).or_else(|_| project_dir(true))? };
    let rows = rows(&dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_rows(&rows);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_declares_its_kinds_in_its_manifest() {
        let std = serde_json::json!({ "orbLanguage": {}, "orb": {} });
        assert_eq!(kinds(&std), vec![Kind::Language, Kind::Behaviors]);
        assert_eq!(kinds(&serde_json::json!({ "orbVocabulary": {} })), vec![Kind::LanguagePack]);
        assert_eq!(kinds(&serde_json::json!({ "orbProgram": { "kind": "shell", "name": "python" } })), vec![Kind::Shell]);
        assert_eq!(kinds(&serde_json::json!({ "orbProgram": { "kind": "desktop" } })), vec![Kind::Desktop]);
    }

    #[test]
    fn control_a_package_declaring_none_is_no_orb_package() {
        assert!(kinds(&serde_json::json!({ "name": "left-pad", "main": "index.js" })).is_empty());
        assert!(kinds(&serde_json::json!({ "orbProgram": { "kind": "plugin" } })).is_empty(), "an unknown program kind is none");
    }

    #[test]
    fn the_baseline_is_every_pin_orb_carries() {
        let specs: Vec<String> = baseline().into_iter().map(|(_, s)| s).collect();
        assert!(specs.contains(&format!("{}@{}", BAKED_CORE.0, BAKED_CORE.1)));
        assert!(specs.iter().any(|s| s.starts_with("@almadar/std@")));
        assert!(specs.iter().any(|s| s.starts_with("@almadar/orb-shell-typescript@")));
        assert!(specs.iter().any(|s| s.starts_with("@almadar/orb-desktop@")));
        assert_eq!(specs.len(), 2 + BAKED_LANGUAGE_PACKS.len() + BAKED_PROGRAMS.len());
    }
}
