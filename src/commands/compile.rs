//! `orb compile` — generate code from a schema, including mixed
//! frontend/backend compilation and docker-compose emission.

use crate::utils::{
    copy_locales, detect_language, detect_syntax, find_shells_dir, load_project_component_mappings,
    strip_json_nulls, translate_schema_content, InputSyntax,
};
use crate::{ServerFrameworkArg, ShellMode, ShellType};
use std::path::{Path, PathBuf};

/// Parse + resolve the schema a client bundles: a native shell's
/// `resolved_schema.json` (Program B, §159 — the in-app kernel mounts on-device
/// with no compiler) and the TypeScript client's `browserSchema.json` (its
/// browser-store legs). Both need the same flattened `OrbitalSchema` `orb verify`
/// and `orb dev` resolve. `source_path` is the ORIGINAL `.orb`/`.lolo` location
/// (never `compile_path`, which for a `.lolo` source is a lowered temp file),
/// so relative `uses` imports (e.g. `almadar-behaviors/*`) resolve exactly
/// like `orb verify` does. The one resolver:
/// `orbital_verify::resolve_schema_with_source` wraps the same
/// `orbital_compiler::resolve_schema_with_path` pipeline `orb verify` runs.
fn resolve_native_shell_schema(
    compile_path: &Path,
    source_path: &Path,
) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let schema = orbital_compiler::phases::parse_file(compile_path).map_err(|e| {
        anyhow::anyhow!("Failed to parse schema for resolved-schema resource: {}", e)
    })?;
    orbital_verify::resolve_schema_with_source(schema, Some(source_path)).map_err(|e| {
        anyhow::anyhow!(
            "Failed to resolve schema for resolved-schema resource: {}",
            e
        )
    })
}

/// `CompilerOptions::from_env()`, but bail with a plain error when behavior-
/// package discovery failed (a `PrefixConflict` or malformed installed
/// package) — validator/compiler infrastructure, never a schema authoring
/// error, so `orb compile` must not silently proceed as if no packages were
/// installed.
fn compiler_options_or_bail() -> anyhow::Result<orbital_compiler::CompilerOptions> {
    let options = orbital_compiler::CompilerOptions::from_env();
    if let Some(e) = &options.discovery_error {
        anyhow::bail!(
            "[{}] {} (validator environment error — the schema was NOT compiled; retry, do not edit the schema)",
            crate::commands::validate::discovery_error_json_code(e),
            e
        );
    }
    Ok(options)
}

/// Compile schema to generated code
pub(crate) fn compile_schema(
    path: &PathBuf,
    output: &PathBuf,
    shell: ShellType,
    mode: ShellMode,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
    backend_url: Option<String>,
    patterns_path: Option<&std::path::Path>,
    verbose: bool,
    dev_logs: bool,
    server_framework: ServerFrameworkArg,
    emit_orb: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use orbital_compiler::shell::{python_shell, typescript_shell, typescript_shell_hono};

    use orbital_compiler::{generate::generate_from_file_with_source, GenerateOptions};
    use orbital_shell_android::{AndroidBackend, AndroidShell};
    use orbital_shell_chrome::{ChromeBackend, ChromeShell};
    use orbital_shell_ios::{IosBackend, IosShell};
    use orbital_shell_mobile::{MobileBackend, MobileShell};
    use orbital_shell_python::PythonBackend;
    use orbital_shell_rust::{RustBackend, RustShell};
    use orbital_shell_typescript::TypeScriptBackend;
    use std::path::PathBuf;

    // Get the shells directory using runtime detection
    let shells_dir = find_shells_dir();

    // Handle mixed compilation (--frontend + --backend)
    if frontend.is_some() || backend.is_some() {
        return compile_mixed(path, output, frontend, backend, &shells_dir, verbose);
    }

    println!("Compiling schema: {}", path.display());
    println!("Output directory: {}", output.display());
    println!("Target shell: {:?}", shell);
    println!("Mode: {:?}", mode);

    // Syntax dispatch: detect LOLO vs Orb JSON syntax.
    // Explicit .lolo extension → always LOLO (no detection needed).
    // .orb files → content-based detection (first non-whitespace char).
    let source_content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let is_lolo_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("lolo"))
        .unwrap_or(false);

    let is_lolo = is_lolo_ext || matches!(detect_syntax(&source_content), InputSyntax::Lolo);

    let compile_path: PathBuf = if is_lolo {
        let schema = if is_lolo_ext {
            // .lolo extension: go straight to the parser, no detection
            orbital_lolo::parse_and_lower(path).map_err(|e| {
                anyhow::anyhow!("Failed to compile .lolo source: {}", e.render_with_hint())
            })?
        } else {
            // .orb file with LOLO syntax: parse from the already-read content
            orbital_lolo::parse_and_lower_str_at(&source_content, &path.display().to_string())
                .map_err(|e| {
                    anyhow::anyhow!("Failed to compile LOLO source: {}", e.render_with_hint())
                })?
        };
        if verbose {
            println!(
                "Detected LOLO syntax{}",
                if is_lolo_ext {
                    " (.lolo file)"
                } else {
                    " in .orb file"
                }
            );
        }
        // Round-trip through serde_json::Value and strip nulls so the
        // emitted .orb matches the sparse hand-authored shape used in
        // existing fixtures (no `"foo": null` clutter).
        let value = serde_json::to_value(&schema)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let cleaned = strip_json_nulls(value);
        let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&cleaned))
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let target = if let Some(emit) = emit_orb {
            emit.to_path_buf()
        } else {
            std::env::temp_dir().join("_orbital_lolo_lowered.orb")
        };
        std::fs::write(&target, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write lowered .orb: {}", e))?;
        if verbose {
            println!("Lowered to {}", target.display());
        }
        target
    } else {
        // Orb JSON path — translate i18n schemas to English before compilation
        let content_translated = translate_schema_content(&source_content);
        let translated_path = if content_translated != source_content {
            // Schema was translated — write to temp file for compilation
            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join("_orbital_translated_schema.orb");
            std::fs::write(&temp_path, &content_translated)
                .map_err(|e| anyhow::anyhow!("Failed to write translated schema: {}", e))?;
            temp_path
        } else {
            path.clone()
        };

        // If --emit-orb was passed for an .orb file, just copy it through.
        if let Some(emit) = emit_orb {
            std::fs::copy(&translated_path, emit)
                .map_err(|e| anyhow::anyhow!("Failed to copy schema to --emit-orb path: {}", e))?;
        }
        translated_path
    };

    // i18n: re-read content for the language detection branch below
    // (the lolo branch produces JSON which contains no Arabic/Slovenian).
    let content_raw = std::fs::read_to_string(&compile_path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let options = GenerateOptions {
        // from_env, not Default: `orb compile` must honor installed behavior
        // packages exactly like `orb emit` — a Default here silently compiled
        // against the embedded std registry only (C-COMPILE-IGNORES-DEV-REGISTRIES).
        compiler: compiler_options_or_bail()?,
        copy_template: true,
        clean: false,
        verbose,
        mode: match mode {
            ShellMode::Full => orbital_compiler::ShellMode::Full,
            ShellMode::Client => orbital_compiler::ShellMode::Client,
            ShellMode::Server => orbital_compiler::ShellMode::Server,
        },
        ..Default::default()
    };

    match shell {
        ShellType::Typescript => {
            let sf = match server_framework {
                ServerFrameworkArg::Express => orbital_shell_typescript::ServerFramework::Express,
                ServerFrameworkArg::Hono => orbital_shell_typescript::ServerFramework::Hono,
            };
            let ts_shell = match sf {
                orbital_shell_typescript::ServerFramework::Hono => {
                    typescript_shell_hono(&shells_dir)
                }
                orbital_shell_typescript::ServerFramework::Express => typescript_shell(&shells_dir),
            };
            // Load project-specific component mappings for code generation
            let extra_mappings = load_project_component_mappings(path, patterns_path);
            let detected_lang = detect_language(&content_raw).unwrap_or_else(|| "en".to_string());
            let backend = TypeScriptBackend {
                extra_mappings,
                schema_language: detected_lang,
                dev_logs,
                server_framework: sf,
                browser_schema: Some(resolve_native_shell_schema(&compile_path, path)?),
                ..TypeScriptBackend::default()
            };
            let result = generate_from_file_with_source(&compile_path, path, output, &ts_shell, &backend, &options)?;

            // Copy project locales into output so the generated App.tsx import resolves
            copy_locales(path, output);

            // `public "./public"` lands at the client's public root, served as-is.
            let lowered = orbital_compiler::parse_file(&compile_path)
                .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", compile_path.display(), e))?;
            if let Some(public) = &lowered.public_dir {
                let from = orbital_compiler::phases::public::resolve_public_dir(path, public)
                    .map_err(|message| anyhow::anyhow!("{message}"))?;
                orbital_compiler::phases::public::copy_public(&from, &output.join("packages/client/public"))
                    .map_err(|e| anyhow::anyhow!("Failed to copy public folder: {e}"))?;
            }

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            // Write .env file with backend URL if provided
            if let Some(url) = &backend_url {
                let env_path = output.join(".env");
                let env_content =
                    format!("# Backend API URL (auto-generated)\nVITE_API_URL={}\n", url);
                // Append to existing .env or create new
                if env_path.exists() {
                    let existing = std::fs::read_to_string(&env_path)?;
                    if !existing.contains("VITE_API_URL=") {
                        std::fs::write(&env_path, format!("{}\n{}", existing.trim(), env_content))?;
                    } else {
                        // Update existing VITE_API_URL
                        let updated: String = existing
                            .lines()
                            .map(|line| {
                                if line.starts_with("VITE_API_URL=") {
                                    format!("VITE_API_URL={}", url)
                                } else {
                                    line.to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        std::fs::write(&env_path, updated)?;
                    }
                } else {
                    std::fs::write(&env_path, env_content)?;
                }
                println!("🔧 Configured backend URL: {}", url);
            }
        }
        ShellType::Python => {
            let py_shell = python_shell(&shells_dir);
            let backend = PythonBackend::new();
            let result = generate_from_file_with_source(&compile_path, path, output, &py_shell, &backend, &options)?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  python -m venv .venv && source .venv/bin/activate");
            println!("  pip install -e .");
            println!("  python -m orbital_app.main");
        }
        ShellType::Mobile => {
            let mobile_shell = MobileShell::new();
            let mobile_backend = MobileBackend::new();
            let result = generate_from_file_with_source(&compile_path, path,
                output,
                &mobile_shell,
                &mobile_backend,
                &options,
            )?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  npm install");
            println!("  npx expo start");
        }
        ShellType::Rust => {
            let rust_shell = RustShell::new();
            let rust_backend = RustBackend::new();
            let result =
                generate_from_file_with_source(&compile_path, path, output, &rust_shell, &rust_backend, &options)?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  cargo check");
            println!("  cargo run");
        }
        ShellType::Android => {
            let android_shell = AndroidShell::new();
            let resolved_schema = resolve_native_shell_schema(&compile_path, path)?;
            let android_backend = AndroidBackend::new().with_resolved_schema(resolved_schema);
            let result = generate_from_file_with_source(&compile_path, path,
                output,
                &android_shell,
                &android_backend,
                &options,
            )?;

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            if std::env::var("ANDROID_HOME").is_err() && std::env::var("ANDROID_SDK_ROOT").is_err()
            {
                println!(
                    "\n⚠️  Neither ANDROID_HOME nor ANDROID_SDK_ROOT is set — local.properties was not written; set one before running ./gradlew"
                );
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  ./gradlew assembleDebug");
        }
        ShellType::Ios => {
            let ios_shell = IosShell::new();
            let resolved_schema = resolve_native_shell_schema(&compile_path, path)?;
            let ios_backend = IosBackend::new().with_resolved_schema(resolved_schema);
            let result =
                generate_from_file_with_source(&compile_path, path, output, &ios_shell, &ios_backend, &options)?;

            // Rung-3 prevention gate (no existing call site runs
            // `Shell::validate_structure`/layout checks for any shell today):
            // catch a template/generated-output layout defect right after
            // generation, using this compile's own manifest.
            if options.copy_template {
                use orbital_compiler::shell::Shell as _;
                ios_shell.validate_structure(output)?;
                let manifest: Vec<String> = result.files.iter().map(|f| f.path.clone()).collect();
                ios_shell.validate_generated_layout(output, &manifest)?;
            }

            println!("\n✅ Generated {} files", result.files.len());

            if !result.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &result.warnings {
                    println!("  ⚠️  {}", warning);
                }
            }

            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  ./orb-build.sh");
        }
        ShellType::ChromeExtension => {
            let chrome_shell = ChromeShell::new();
            let resolved_schema = resolve_native_shell_schema(&compile_path, path)?;
            let program_json = serde_json::to_value(&resolved_schema)?;
            let services = crate::utils::load_services_registry(None);
            let program = orbital_shell_chrome::check_program(&program_json, &services.runtimes)
                .map_err(|errors| anyhow::anyhow!(errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")))?;
            let chrome_backend = ChromeBackend::new(resolved_schema, program);
            let result = generate_from_file_with_source(&compile_path, path, output, &chrome_shell, &chrome_backend, &options)?;

            println!("\n✅ Generated {} files", result.files.len());
            for warning in &result.warnings {
                println!("  ⚠️  {}", warning);
            }
            println!("\nNext steps:");
            println!("  cd {}", output.display());
            println!("  pnpm install && pnpm build");
            println!("  Load dist/ as an unpacked extension at chrome://extensions");
        }
    }

    Ok(())
}

/// Compile with mixed frontend/backend shells
fn compile_mixed(
    path: &PathBuf,
    output: &PathBuf,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
    shells_dir: &PathBuf,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::shell::{python_shell, typescript_shell};

    use orbital_compiler::{generate::generate_from_file_with_source, GenerateOptions};
    use orbital_shell_android::{AndroidBackend, AndroidShell};
    use orbital_shell_ios::{IosBackend, IosShell};
    use orbital_shell_mobile::{MobileBackend, MobileShell};
    use orbital_shell_python::PythonBackend;
    use orbital_shell_rust::{RustBackend, RustShell};
    use orbital_shell_typescript::TypeScriptBackend;

    println!("Compiling schema: {}", path.display());
    println!("Output directory: {}", output.display());
    println!("Mixed mode compilation:");

    // Auto-detect LOLO vs Orb JSON syntax, handle i18n translation
    let content_raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;

    let compile_path = if matches!(detect_syntax(&content_raw), InputSyntax::Lolo) {
        // LOLO syntax in .orb file — lower to JSON temp file
        let schema =
            orbital_lolo::parse_and_lower_str_at(&content_raw, &path.display().to_string())
                .map_err(|e| {
                    anyhow::anyhow!("Failed to compile LOLO source: {}", e.render_with_hint())
                })?;
        let value = serde_json::to_value(&schema)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let cleaned = strip_json_nulls(value);
        let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&cleaned))
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("_orbital_lolo_lowered_mixed.orb");
        std::fs::write(&temp_path, &json)
            .map_err(|e| anyhow::anyhow!("Failed to write lowered .orb: {}", e))?;
        temp_path
    } else {
        // Orb JSON — apply i18n translation
        let content_translated = translate_schema_content(&content_raw);
        if content_translated != content_raw {
            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join("_orbital_translated_schema_mixed.orb");
            std::fs::write(&temp_path, &content_translated)
                .map_err(|e| anyhow::anyhow!("Failed to write translated schema: {}", e))?;
            temp_path
        } else {
            path.clone()
        }
    };

    let mut total_files = 0;

    // Compile frontend
    if let Some(frontend_shell) = frontend {
        let frontend_output = output.join("frontend");
        println!(
            "  Frontend: {:?} -> {}",
            frontend_shell,
            frontend_output.display()
        );

        let options = GenerateOptions {
            compiler: compiler_options_or_bail()?,
            copy_template: true,
            clean: false,
            verbose,
            mode: orbital_compiler::ShellMode::Client,
            ..Default::default()
        };

        let detected_lang_mixed = detect_language(&content_raw).unwrap_or_else(|| "en".to_string());
        match frontend_shell {
            ShellType::Typescript => {
                let ts_shell = typescript_shell(shells_dir);
                let backend = TypeScriptBackend {
                    schema_language: detected_lang_mixed.clone(),
                    browser_schema: Some(resolve_native_shell_schema(&compile_path, path)?),
                    ..TypeScriptBackend::new()
                };
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &ts_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Python => {
                let py_shell = python_shell(shells_dir);
                let backend = PythonBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &py_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Mobile => {
                let mob_shell = MobileShell::new();
                let mob_backend = MobileBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &mob_shell,
                    &mob_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Rust => {
                let rust_shell = RustShell::new();
                let rust_backend = RustBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &rust_shell,
                    &rust_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Android => {
                let android_shell = AndroidShell::new();
                let android_backend = AndroidBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &android_shell,
                    &android_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::ChromeExtension => {
                return Err(anyhow::anyhow!("The chrome-extension shell is a whole program; compile it without --frontend/--backend"));
            }
            ShellType::Ios => {
                let ios_shell = IosShell::new();
                let ios_backend = IosBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &frontend_output,
                    &ios_shell,
                    &ios_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
        }
    }

    // Compile backend
    if let Some(backend_shell) = backend {
        let backend_output = output.join("backend");
        println!(
            "  Backend: {:?} -> {}",
            backend_shell,
            backend_output.display()
        );

        let options = GenerateOptions {
            compiler: compiler_options_or_bail()?,
            copy_template: true,
            clean: false,
            verbose,
            mode: orbital_compiler::ShellMode::Server,
            ..Default::default()
        };

        match backend_shell {
            ShellType::Typescript => {
                let ts_shell = typescript_shell(shells_dir);
                let backend = TypeScriptBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &backend_output,
                    &ts_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Python => {
                let py_shell = python_shell(shells_dir);
                let backend = PythonBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &backend_output,
                    &py_shell,
                    &backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Rust => {
                let rust_shell = RustShell::new();
                let rust_backend = RustBackend::new();
                let result = generate_from_file_with_source(&compile_path, path,
                    &backend_output,
                    &rust_shell,
                    &rust_backend,
                    &options,
                )?;
                total_files += result.files.len();
            }
            ShellType::Mobile => {
                return Err(anyhow::anyhow!("Mobile shell cannot be used as a backend"));
            }
            ShellType::Android => {
                return Err(anyhow::anyhow!("Android shell cannot be used as a backend"));
            }
            ShellType::Ios => {
                return Err(anyhow::anyhow!("iOS shell cannot be used as a backend"));
            }
            ShellType::ChromeExtension => {
                return Err(anyhow::anyhow!("The chrome-extension shell is a whole program; compile it without --frontend/--backend"));
            }
        }
    }

    println!("\n✅ Generated {} total files", total_files);

    // Generate docker-compose for mixed deployment
    if frontend.is_some() && backend.is_some() {
        let docker_compose = generate_docker_compose(frontend, backend);
        let compose_path = output.join("docker-compose.yaml");
        std::fs::write(&compose_path, docker_compose)?;
        println!("📦 Generated docker-compose.yaml");

        // Generate .env file for frontend with API URL
        let backend_url = match backend {
            Some(ShellType::Python) => "http://localhost:8000",
            Some(ShellType::Typescript) => "http://localhost:3030",
            Some(ShellType::Rust) => "http://localhost:3030",
            Some(ShellType::Mobile) => "http://localhost:8000",
            Some(ShellType::Android) => "http://localhost:8000",
            Some(ShellType::Ios) => "http://localhost:8000",
            Some(ShellType::ChromeExtension) => "http://localhost:8000",
            None => "http://localhost:8000",
        };

        let env_content = match frontend {
            Some(ShellType::Typescript) => format!(
                "# API URL for backend server\nVITE_API_URL={}\n",
                backend_url
            ),
            Some(ShellType::Python) => {
                format!("# API URL for backend server\nAPI_URL={}\n", backend_url)
            }
            Some(ShellType::Mobile) => {
                format!(
                    "# API URL for backend server\nEXPO_PUBLIC_API_URL={}\n",
                    backend_url
                )
            }
            Some(ShellType::Rust) => {
                format!("# API URL for backend server\nAPI_URL={}\n", backend_url)
            }
            Some(ShellType::Android) => {
                format!("# API URL for backend server\nAPI_URL={}\n", backend_url)
            }
            Some(ShellType::Ios) => {
                format!("# API URL for backend server\nAPI_URL={}\n", backend_url)
            }
            Some(ShellType::ChromeExtension) => String::new(),
            None => String::new(),
        };

        if !env_content.is_empty() {
            let env_path = output.join("frontend").join(".env");
            std::fs::write(&env_path, &env_content)?;
            println!("🔧 Generated frontend/.env with API_URL");
        }

        println!("\nNext steps:");
        println!("  cd {}", output.display());
        println!("  docker-compose up");
        println!("\nOr run separately:");
        println!("  # Terminal 1: Backend");
        println!("  cd backend && python -m venv .venv && source .venv/bin/activate && pip install -e . && python -m orbital_app.main");
        println!("  # Terminal 2: Frontend");
        println!("  cd frontend && npm install && npm run dev");
    }

    Ok(())
}

/// Generate docker-compose.yaml for mixed deployment
fn generate_docker_compose(frontend: Option<ShellType>, backend: Option<ShellType>) -> String {
    let mut compose = String::from("version: '3.8'\n\nservices:\n");

    if let Some(backend_shell) = backend {
        match backend_shell {
            ShellType::Python => {
                compose.push_str("  backend:\n");
                compose.push_str("    build: ./backend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"8000:8000\"\n");
                compose.push_str("    environment:\n");
                compose.push_str("      - CORS_ORIGINS=http://localhost:3000\n");
                compose.push_str("\n");
            }
            ShellType::Typescript => {
                compose.push_str("  backend:\n");
                compose.push_str("    build: ./backend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3030:3030\"\n");
                compose.push_str("    environment:\n");
                compose.push_str("      - CORS_ORIGINS=http://localhost:3000\n");
                compose.push_str("\n");
            }
            ShellType::Rust => {
                compose.push_str("  backend:\n");
                compose.push_str("    build: ./backend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3030:3030\"\n");
                compose.push_str("    environment:\n");
                compose.push_str("      - CORS_ORIGINS=http://localhost:3000\n");
                compose.push_str("\n");
            }
            ShellType::Mobile => {
                // Mobile apps don't have a backend component
            }
            ShellType::Android => {
                // Android apps don't have a backend component
            }
            ShellType::Ios => {
                // iOS apps don't have a backend component
            }
            ShellType::ChromeExtension => {
                // A chrome extension has no backend component
            }
        }
    }

    if let Some(frontend_shell) = frontend {
        // API URL from browser perspective (localhost, not internal docker network)
        let backend_url = match backend {
            Some(ShellType::Python) => "http://localhost:8000",
            Some(ShellType::Typescript) => "http://localhost:3030",
            Some(ShellType::Rust) => "http://localhost:3030",
            Some(ShellType::Mobile) => "http://localhost:8000",
            Some(ShellType::Android) => "http://localhost:8000",
            Some(ShellType::Ios) => "http://localhost:8000",
            Some(ShellType::ChromeExtension) => "http://localhost:8000",
            None => "http://localhost:8000",
        };

        match frontend_shell {
            ShellType::Typescript => {
                compose.push_str("  frontend:\n");
                compose.push_str("    build:\n");
                compose.push_str("      context: ./frontend\n");
                compose.push_str("      args:\n");
                compose.push_str(&format!("        VITE_API_URL: {}\n", backend_url));
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3000:3000\"\n");
                compose.push_str("    depends_on:\n");
                compose.push_str("      - backend\n");
            }
            ShellType::Python => {
                compose.push_str("  frontend:\n");
                compose.push_str("    build: ./frontend\n");
                compose.push_str("    ports:\n");
                compose.push_str("      - \"8080:8080\"\n");
                compose.push_str("    environment:\n");
                compose.push_str(&format!("      - API_URL={}\n", backend_url));
                compose.push_str("    depends_on:\n");
                compose.push_str("      - backend\n");
            }
            ShellType::Mobile => {
                // Mobile apps don't run in Docker - they run on device/simulator
                // Just document the backend dependency
                compose.push_str("  # Mobile app - run locally with:\n");
                compose.push_str("  #   cd frontend && npm install && npx expo start\n");
                compose.push_str("  # Backend URL for mobile app:\n");
                compose.push_str(&format!("  #   EXPO_PUBLIC_API_URL={}\n", backend_url));
            }
            ShellType::Android => {
                // Android apps don't run in Docker - they run on device/emulator
                compose.push_str("  # Android app - run locally with:\n");
                compose.push_str(
                    "  #   cd frontend && ./gradlew assembleDebug && ./gradlew installDebug\n",
                );
                compose.push_str("  # Backend URL for Android app:\n");
                compose.push_str(&format!("  #   API_URL={}\n", backend_url));
            }
            ShellType::Rust => {
                compose.push_str("  frontend:\n");
                compose.push_str("    build:\n");
                compose.push_str("      context: ./frontend\n");
                compose.push_str("      args:\n");
                compose.push_str(&format!("        API_URL: {}\n", backend_url));
                compose.push_str("    ports:\n");
                compose.push_str("      - \"3000:3000\"\n");
                compose.push_str("    depends_on:\n");
                compose.push_str("      - backend\n");
            }
            ShellType::ChromeExtension => {
                compose.push_str("  # Chrome extension - build with: pnpm build, then load dist/ unpacked\n");
            }
            ShellType::Ios => {
                compose.push_str("  # iOS app - run locally with:\n");
                compose.push_str("  #   cd frontend && ./orb-build.sh\n");
                compose.push_str("  # Backend URL for iOS app:\n");
                compose.push_str(&format!("  #   API_URL={}\n", backend_url));
            }
        }
    }

    compose
}
