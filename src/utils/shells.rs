//! Runtime detection of the shells/templates directory.

use std::path::PathBuf;

/// Find shells/templates directory using runtime detection
///
/// Priority:
/// 1. ORBITAL_SHELLS_DIR environment variable
/// 2. Relative to executable location (covers npm global installs and local builds)
///    - npm global: <prefix>/bin/orb → <prefix>/lib/node_modules/@almadar/shell
///    - pnpm/yarn global: similar patterns
///    - local monorepo build: relative paths to packages/ or templates/
/// 3. Compile-time path (for development in the monorepo)
///
/// Note: Supports "almadar-shell" (current) and "typescript-shell" (legacy)
pub(crate) fn find_shells_dir() -> PathBuf {
    // 1. Check ORBITAL_SHELLS_DIR environment variable (trust it if set)
    if let Ok(shells_dir) = std::env::var("ORBITAL_SHELLS_DIR") {
        return PathBuf::from(shells_dir);
    }

    // 2. Check monorepo first: if cwd has packages/almadar-shell, use it.
    //    This ensures local development always uses the monorepo shell
    //    (which matches the locally-built @almadar/ui), not a stale npm-installed one.
    if let Ok(cwd) = std::env::current_dir() {
        let monorepo_packages = cwd.join("packages");
        if monorepo_packages.join("almadar-shell").exists() {
            return monorepo_packages;
        }
        // Walk up from cwd to find monorepo root (handles running from subdirectories)
        let mut dir = cwd.as_path();
        for _ in 0..5 {
            if let Some(parent) = dir.parent() {
                let packages_path = parent.join("packages");
                if packages_path.join("almadar-shell").exists() {
                    return packages_path;
                }
                dir = parent;
            }
        }
    }

    // 3. Check compile-time paths (monorepo dev when cwd doesn't resolve).
    //    Shells now live as submodules under orbital-rust/templates/, so prefer
    //    that; fall back to packages/ only while the legacy layout still exists.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(orbital_root) = manifest_dir.parent().and_then(|p| p.parent()) {
        let templates_path = orbital_root.join("templates");
        if templates_path.join("almadar-shell").exists() {
            return templates_path;
        }
    }
    if let Some(repo_root) = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
    {
        let packages_path = repo_root.join("packages");
        if packages_path.join("almadar-shell").exists() {
            return packages_path;
        }
    }

    // 4. Check relative to executable location (npm global installs)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // npm global install layout:
            //   {prefix}/bin/orb
            //   {prefix}/lib/node_modules/@almadar/shell
            for nm_rel in [
                "../lib/node_modules",
                "../node_modules",
                "../../node_modules",
            ] {
                let npm_shell = exe_dir.join(nm_rel).join("@almadar/shell");
                if npm_shell.exists() {
                    if let Some(parent) = npm_shell.parent() {
                        return parent.to_path_buf();
                    }
                }
            }

            // Local build / CI / install layout: look for templates/ or shells/ nearby
            for dir_name in ["templates", "shells"] {
                for prefix in ["", "../", "../../", "../../../"] {
                    let path = exe_dir.join(format!("{}{}", prefix, dir_name));
                    if path.join("almadar-shell").exists() || path.join("typescript-shell").exists()
                    {
                        return path;
                    }
                }
            }
        }
    }

    // Check orbital-rust relative paths
    if let Some(parent) = manifest_dir.parent().and_then(|p| p.parent()) {
        let templates_path = parent.join("templates");
        if templates_path.exists() {
            return templates_path;
        }
        let shells_path = parent.join("shells");
        if shells_path.exists() {
            return shells_path;
        }
    }

    // Default fallback — will produce a clear "Shell not found" error with guidance
    PathBuf::from("packages")
}
