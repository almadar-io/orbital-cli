//! `orb compile` — generate code from a schema, including mixed
//! frontend/backend compilation and docker-compose emission.

use crate::utils::{
    detect_language, detect_syntax, load_project_patterns, shell_programs,
    translate_schema_content, InputSyntax,
};
use crate::{HostArg, ServerFrameworkArg, ShellMode, ShellType};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The resolved program a shell embeds as data: `resolved_schema.json` for the native in-app
/// kernel and `browserSchema.json` for the TypeScript client's browser-store legs. It is the
/// program as the compiled path compiles it, lazy behaviors included (their pages are routes of
/// the same app), resolved against the original source (never `compile_path`, which for a
/// `.lolo` source is a lowered temp file) so relative imports find their files.
pub(crate) fn resolve_compiled_schema(
    compile_path: &Path,
    source_path: &Path,
) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let schema = orbital_compiler::phases::parse_file(compile_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse schema for the embedded resolved schema: {}",
            e
        )
    })?;
    let options = compiler_options_or_bail()?;
    orbital_compiler::phases::resolve_compiled_program_with_path(
        schema,
        &options,
        Some(source_path),
    )
    .map_err(|e| anyhow::anyhow!("Failed to resolve the embedded resolved schema: {}", e))
}

/// `CompilerOptions::from_env()`, but bail with a plain error when behavior-
/// package discovery failed (a `PrefixConflict` or malformed installed
/// package) — validator/compiler infrastructure, never a schema authoring
/// error, so `orb compile` must not silently proceed as if no packages were
/// installed.
fn compiler_options_or_bail() -> anyhow::Result<orbital_compiler::CompilerOptions> {
    let mut options = orbital_compiler::CompilerOptions::from_env();
    options.service_runtimes = Some(crate::utils::load_services_registry(None)?.runtimes);
    if let Some(e) = &options.discovery_error {
        anyhow::bail!(
            "[{}] {} (validator environment error — the schema was NOT compiled; retry, do not edit the schema)",
            crate::commands::validate::discovery_error_json_code(e),
            e
        );
    }
    Ok(options)
}

fn generate_options(verbose: bool, mode: orbital_compiler::ShellMode) -> anyhow::Result<orbital_compiler::GenerateOptions> {
    Ok(orbital_compiler::GenerateOptions {
        // from_env, not Default: `orb compile` must honor installed behavior
        // packages exactly like `orb emit` (C-COMPILE-IGNORES-DEV-REGISTRIES).
        compiler: compiler_options_or_bail()?,
        copy_template: true,
        clean: false,
        verbose,
        mode,
        ..Default::default()
    })
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
    host: HostArg,
    emit_orb: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Handle mixed compilation (--frontend + --backend)
    if frontend.is_some() || backend.is_some() {
        return compile_mixed(path, output, frontend, backend, verbose);
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
        // A schema field left unset is skipped by its type's serializer; a `null` the program authored inside an
        // expression or payload literal is a value and stays (`orb lower` and `orb resolve` keep it too).
        let value = serde_json::to_value(&schema)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))
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

    let options = generate_options(
        verbose,
        match mode {
            ShellMode::Full => orbital_compiler::ShellMode::Full,
            ShellMode::Client => orbital_compiler::ShellMode::Client,
            ShellMode::Server => orbital_compiler::ShellMode::Server,
        },
    )?;

    let mut shell_options = BTreeMap::new();
    if let ShellType::Typescript = shell {
        let framework = match server_framework {
            ServerFrameworkArg::Express => "express",
            ServerFrameworkArg::Hono => "hono",
        };
        shell_options.insert("framework".to_string(), Value::from(framework));
        shell_options.insert(
            "schemaLanguage".to_string(),
            Value::from(detect_language(&content_raw).unwrap_or_else(|| "en".to_string())),
        );
        shell_options.insert("devLogs".to_string(), Value::from(if dev_logs { "true" } else { "false" }));
        shell_options.insert(
            "host".to_string(),
            Value::from(match host {
                HostArg::Kernel => "kernel",
            }),
        );
        if let Some(patterns) = load_project_patterns(path, patterns_path) {
            shell_options.insert("projectPatterns".to_string(), patterns);
        }
        if let Some(url) = backend_url {
            shell_options.insert("backendUrl".to_string(), Value::from(url));
        }
    }

    let response = shell_programs::generate(shell.program(), &compile_path, path, output, &options, shell_options)?;
    shell_programs::report(&response);
    Ok(())
}

/// Compile with mixed frontend/backend shells
fn compile_mixed(
    path: &PathBuf,
    output: &PathBuf,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
    verbose: bool,
) -> anyhow::Result<()> {
    for shell in [frontend, backend].into_iter().flatten() {
        if let ShellType::ChromeExtension = shell {
            anyhow::bail!("The chrome-extension shell is a whole program; compile it without --frontend/--backend");
        }
    }
    match backend {
        Some(ShellType::Android) => anyhow::bail!("Android shell cannot be used as a backend"),
        Some(ShellType::Ios) => anyhow::bail!("iOS shell cannot be used as a backend"),
        _ => {}
    }

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
        let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))
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

    if let Some(frontend_shell) = frontend {
        let frontend_output = output.join("frontend");
        println!("  Frontend: {:?} -> {}", frontend_shell, frontend_output.display());
        let mut shell_options = BTreeMap::new();
        if let ShellType::Typescript = frontend_shell {
            shell_options.insert(
                "schemaLanguage".to_string(),
                Value::from(detect_language(&content_raw).unwrap_or_else(|| "en".to_string())),
            );
        }
        let options = generate_options(verbose, orbital_compiler::ShellMode::Client)?;
        let response = shell_programs::generate(frontend_shell.program(), &compile_path, path, &frontend_output, &options, shell_options)?;
        total_files += response.files.len();
    }

    if let Some(backend_shell) = backend {
        let backend_output = output.join("backend");
        println!("  Backend: {:?} -> {}", backend_shell, backend_output.display());
        let options = generate_options(verbose, orbital_compiler::ShellMode::Server)?;
        let response = shell_programs::generate(backend_shell.program(), &compile_path, path, &backend_output, &options, BTreeMap::new())?;
        total_files += response.files.len();
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
                compose.push_str(
                    "  # Chrome extension - build with: pnpm build, then load dist/ unpacked\n",
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../tools/verify-efficacy/fixtures/lazy-walk/good.lolo")
    }

    fn trait_names(schema: &orbital_core::OrbitalSchema) -> Vec<String> {
        serde_json::to_value(schema).unwrap()["orbitals"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|o| o["traits"].as_array().cloned().unwrap_or_default())
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn the_native_resolved_schema_carries_the_lazy_orbitals_traits() {
        let path = lazy_fixture();
        if !path.exists() {
            eprintln!("skipping: {} is a monorepo fixture", path.display());
            return;
        }
        orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
        let lowered = orbital_lolo::parse_and_lower(&path).expect("lower the fixture");
        let dir = std::env::temp_dir().join(format!("native-lazy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let orb = dir.join("good.orb");
        std::fs::write(&orb, serde_json::to_string(&lowered).unwrap()).unwrap();
        let names = trait_names(&resolve_compiled_schema(&orb, &path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            names.iter().any(|n| n == "HomeView"),
            "control: the eager trait: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "TeamSectionTeamRoster"),
            "the lazy trait is in the kernel schema: {names:?}"
        );
    }
}
