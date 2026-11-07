//! Raw reads of the installed behavior packages: which file a specifier names, its program and source text, the
//! expectations it derives, a behavior's description and the rows a catalog lists. Ranking, topics, embedding texts,
//! vectors and the index cache policy are std's (`std-orb-behaviors`, `std-orb-vectors`); these functions return
//! structured data and decide nothing about how it is shown.

mod describe;
mod index;

pub(crate) use describe::{describe, DescribeTarget};
use index::{package_rows, BehaviorRow};
pub(crate) use index::RowDepth;

use orbital_compiler::loader::{ExternalOrbitalLoader, ImportChain, LoaderConfig, LoaderError};
use orbital_core::behavior_index::{header_tags, indexed_file_at, lolo_path_of};
use orbital_core::behavior_packages::BehaviorPackage;
use serde_json::Value;
use std::path::{Path, PathBuf};

const TRAITS_SEGMENT: &str = ".traits.";

pub(crate) struct Resolved {
    pub(crate) path: PathBuf,
    pub(crate) package: Option<BehaviorPackage>,
    pub(crate) schema: orbital_core::schema::OrbitalSchema,
}

/// The behavior a specifier names, without a `.traits.<Trait>` suffix.
fn behavior_part(spec: &str) -> &str {
    match spec.rfind(TRAITS_SEGMENT) {
        Some(i) if i > 0 => &spec[..i],
        _ => spec,
    }
}

fn loader_for(cwd: PathBuf) -> anyhow::Result<(ExternalOrbitalLoader, Vec<BehaviorPackage>)> {
    let packages = crate::commands::program_host::discover_packages(&cwd)?;
    let loader = ExternalOrbitalLoader::new(LoaderConfig {
        base_path: cwd,
        packages: packages.clone(),
        ..Default::default()
    });
    Ok((loader, packages))
}

fn owner_of(packages: &[BehaviorPackage], path: &Path) -> Option<BehaviorPackage> {
    packages.iter().find(|p| path.starts_with(p.registry.root())).cloned()
}

/// `spec` through the loader, a relative specifier resolving from the program file `from`.
pub(crate) fn resolve_from(spec: &str, from: Option<&Path>) -> anyhow::Result<Resolved> {
    let cwd = std::env::current_dir()?;
    let from = from.map(|f| cwd.join(f));
    let (mut loader, packages) = loader_for(cwd)?;
    let loaded = loader
        .load(spec, from.as_deref(), &mut ImportChain::new())
        .map_err(|e| anyhow::anyhow!("cannot resolve `{spec}`: {e}"))?;
    Ok(Resolved {
        package: owner_of(&packages, &loaded.source_path),
        path: loaded.source_path.clone(),
        schema: loaded.schema.clone(),
    })
}

/// The source file a behavior specifier resolves to.
pub(crate) fn behavior_source_path(spec: &str) -> anyhow::Result<PathBuf> {
    Ok(resolve_from(behavior_part(spec), None)?.path)
}

/// The behavior a specifier names as a program value; a `.orb` file is read as written.
pub(crate) fn source_value_of(spec: &str) -> anyhow::Result<orbital_core::Value> {
    program_value(&resolve_from(behavior_part(spec), None)?)
}

pub(crate) fn source(spec: &str) -> anyhow::Result<Value> {
    source_value_of(spec).map(|v| orbital_core::json_value::value_to_json(&v))
}

fn program_value(resolved: &Resolved) -> anyhow::Result<orbital_core::Value> {
    if resolved.path.extension().is_some_and(|e| e == "orb") {
        let raw =
            std::fs::read_to_string(&resolved.path).map_err(|e| anyhow::anyhow!("failed to read {}: {e}", resolved.path.display()))?;
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", resolved.path.display()))
    } else {
        Ok(serde_json::from_value(serde_json::to_value(&resolved.schema)?)?)
    }
}

/// A behavior one `uses` names as the loader resolved it.
pub(crate) struct ImportedBehavior {
    pub(crate) package: Option<BehaviorPackage>,
    pub(crate) pattern: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) program: Value,
}

/// One `uses` specifier through `loader`, from the importing file `from`; `None` when no file answers it.
pub(crate) fn resolve_import(
    loader: &mut ExternalOrbitalLoader,
    packages: &[BehaviorPackage],
    spec: &str,
    from: Option<&Path>,
) -> anyhow::Result<Option<ImportedBehavior>> {
    let loaded = match loader.load(spec, from, &mut ImportChain::new()) {
        Ok(loaded) => loaded,
        Err(LoaderError::FileNotFound(_)) => return Ok(None),
        Err(e) => anyhow::bail!("cannot resolve `{spec}`: {e}"),
    };
    let package = owner_of(packages, &loaded.source_path);
    let pattern = match &package {
        Some(pkg) => {
            let name = loaded
                .source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let file = indexed_file_at(pkg, name, loaded.source_path.clone());
            match lolo_path_of(pkg, &file) {
                Some(p) => match std::fs::read_to_string(&p) {
                    Ok(text) => header_tags(&text).map_err(|e| anyhow::anyhow!(e))?.pattern,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => anyhow::bail!("failed to read {}: {e}", p.display()),
                },
                None => None,
            }
        }
        None => None,
    };
    let resolved = Resolved {
        path: loaded.source_path.clone(),
        package: package.clone(),
        schema: loaded.schema.clone(),
    };
    let program = orbital_core::json_value::value_to_json(&program_value(&resolved)?);
    Ok(Some(ImportedBehavior {
        program,
        package,
        pattern,
        path: loaded.source_path.clone(),
    }))
}

/// Each orbital's derived expectations for the `.orb` file or behavior `spec` names.
pub(crate) fn expectations(spec: &str) -> anyhow::Result<Value> {
    let file = Path::new(spec);
    let schema: Value = if file.is_file() {
        serde_json::from_str(&std::fs::read_to_string(file)?).map_err(|e| anyhow::anyhow!("`{spec}` is not a .orb program: {e}"))?
    } else {
        serde_json::to_value(&resolve_from(spec, None)?.schema)?
    };
    let loaded: std::cell::RefCell<std::collections::HashMap<String, Option<Value>>> = Default::default();
    let load = |from: &str| -> Option<Value> {
        if let Some(hit) = loaded.borrow().get(from) {
            return hit.clone();
        }
        let value = resolve_from(from, None).ok().and_then(|r| serde_json::to_value(&r.schema).ok());
        loaded.borrow_mut().insert(from.to_string(), value.clone());
        value
    };
    let names: Vec<String> = schema["orbitals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["name"].as_str().map(str::to_string))
        .collect();
    let mut out = serde_json::Map::new();
    for name in names {
        let derived = orbital_core::expects::derive_expectations(&schema, &name, Some(&load))
            .map_err(|e| anyhow::anyhow!("cannot derive `{name}` expectations: {e}"))?;
        out.insert(name, serde_json::to_value(derived)?);
    }
    Ok(Value::Object(out))
}

/// One catalog row with the package it came from.
pub(crate) struct CatalogRow {
    pub(crate) package: String,
    pub(crate) prefix: String,
    pub(crate) row: BehaviorRow,
    /// The package's place among the packages the scopes name, in first-mention order.
    pub(crate) rank: usize,
    /// The row's place among its package's rows, in path order.
    pub(crate) order: usize,
}

/// The installed behaviors under `paths` (`<prefix>[/<dir>…]`, `std` naming the std package), narrowed to `exposure`
/// when given, in `<prefix>/<name>` order. A behavior that does not load is reported through `skipped`.
pub(crate) fn catalog_rows(
    paths: &[String],
    exposure: &[String],
    depth: index::RowDepth,
    skipped: &mut dyn FnMut(&str, &str, &str),
) -> anyhow::Result<Vec<CatalogRow>> {
    use orbital_core::behavior_index::EXPOSURES;
    use orbital_core::behavior_packages::STD_PACKAGE;
    if paths.is_empty() {
        anyhow::bail!("give at least one scope path, e.g. `almadar-std/ui/core/molecules`");
    }
    if let Some(e) = exposure.iter().find(|e| !EXPOSURES.contains(&e.as_str())) {
        anyhow::bail!("unknown exposure `{e}` (one of {})", EXPOSURES.join(", "));
    }
    let packages = crate::commands::program_host::discover_packages(&std::env::current_dir()?)?;
    let mut scopes: Vec<(usize, Vec<String>)> = Vec::new();
    for raw in paths {
        let mut parts = raw.split('/').filter(|p| !p.is_empty());
        let head = parts.next().ok_or_else(|| anyhow::anyhow!("empty scope path"))?;
        let package = packages
            .iter()
            .position(|p| if head == "std" { p.name == STD_PACKAGE } else { p.prefix == head })
            .ok_or_else(|| {
                let installed: Vec<&str> = packages.iter().map(|p| p.prefix.as_str()).collect();
                anyhow::anyhow!(
                    "scope `{raw}` names no installed behavior package prefix (installed: {})",
                    installed.join(", ")
                )
            })?;
        scopes.push((package, parts.map(str::to_string).collect()));
    }
    let mut derived: std::collections::BTreeMap<usize, index::PackageRows> = Default::default();
    for (package, _) in &scopes {
        if !derived.contains_key(package) {
            let rows = package_rows(&packages, *package, depth)?;
            for (name, error) in &rows.skipped {
                skipped(&packages[*package].prefix, name, error);
            }
            derived.insert(*package, rows);
        }
    }
    let mut first_mention: Vec<usize> = Vec::new();
    for (package, _) in &scopes {
        if !first_mention.contains(package) {
            first_mention.push(*package);
        }
    }
    let mut out: std::collections::BTreeMap<String, CatalogRow> = Default::default();
    for (package, dirs) in &scopes {
        let pkg = &packages[*package];
        let rank = first_mention.iter().position(|p| p == package).unwrap_or(0);
        for (order, row) in derived[package].rows.iter().enumerate() {
            let in_scope = row.dirs.len() >= dirs.len() && row.dirs[..dirs.len()] == dirs[..];
            let exposed = exposure.is_empty() || row.exposure.as_deref().is_some_and(|e| exposure.iter().any(|x| x == e));
            if in_scope && exposed {
                out.insert(
                    format!("{}/{}", pkg.prefix, row.name),
                    CatalogRow {
                        package: pkg.name.clone(),
                        prefix: pkg.prefix.clone(),
                        row: row.clone(),
                        rank,
                        order,
                    },
                );
            }
        }
    }
    Ok(out.into_values().collect())
}
