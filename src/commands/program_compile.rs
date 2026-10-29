//! `orb compile` / `program/eval { generate }`: lower the program, validate it, have the shell write the project.

use crate::utils::{
    detect_language, detect_syntax, load_project_patterns, shell_programs,
    translate_schema_content, InputSyntax,
};
use crate::{HostArg, ShellMode, ShellType};
use orbital_compiler::mixed::{self, CompileReport, MixedShell};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) struct CompileRequest<'a> {
    pub path: &'a PathBuf,
    pub output: &'a PathBuf,
    pub shell: ShellType,
    pub mode: ShellMode,
    pub frontend: Option<ShellType>,
    pub backend: Option<ShellType>,
    pub backend_url: Option<String>,
    pub patterns_path: Option<&'a Path>,
    pub dev_logs: bool,
    pub host: HostArg,
    pub emit_orb: Option<&'a Path>,
    /// The program file whose folder holds its `locales/` and `public/`, when `path` was written elsewhere.
    pub source: Option<&'a Path>,
}

fn mixed_shell(shell: ShellType) -> MixedShell {
    match shell {
        ShellType::Typescript => MixedShell::Typescript,
        ShellType::Python => MixedShell::Python,
        ShellType::Rust => MixedShell::Rust,
        ShellType::Android => MixedShell::Android,
        ShellType::Ios => MixedShell::Ios,
        ShellType::ChromeExtension => MixedShell::ChromeExtension,
        ShellType::Os => MixedShell::Os,
    }
}

fn scratch_file(
    slot: &mut Option<tempfile::TempDir>,
    name: &str,
    content: &str,
) -> anyhow::Result<PathBuf> {
    let dir = match slot {
        Some(dir) => dir,
        None => slot.insert(crate::commands::dev::scratch_dir("orbital-compile-")?),
    };
    let path = dir.path().join(name);
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    Ok(path)
}

fn generate_options(
    mode: orbital_compiler::ShellMode,
) -> anyhow::Result<orbital_compiler::GenerateOptions> {
    Ok(orbital_compiler::GenerateOptions {
        compiler: shell_programs::compiler_options_or_bail()?,
        copy_template: true,
        clean: false,
        mode,
    })
}

/// The `.orb` file the compile reads: the lowered `.lolo` (written to `emit_orb` when given, else scratch), or the
/// `.orb` itself with its i18n vocabulary translated. `content` is `path`'s text.
fn program_file(
    path: &Path,
    content: &str,
    emit_orb: Option<&Path>,
    scratch: &mut Option<tempfile::TempDir>,
) -> anyhow::Result<PathBuf> {
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lolo"));
    if by_extension || matches!(detect_syntax(content), InputSyntax::Lolo) {
        let lowered = if by_extension {
            orbital_lolo::parse_and_lower(path)
        } else {
            orbital_lolo::parse_and_lower_str_at(content, &path.display().to_string())
        }
        .map_err(|e| anyhow::anyhow!("Failed to compile .lolo source: {}", e.render_with_hint()))?;
        let value = serde_json::to_value(&lowered)
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        let json = serde_json::to_string_pretty(&orbital_core::util::canonical_json(&value))
            .map_err(|e| anyhow::anyhow!("Failed to serialize lowered schema: {}", e))?;
        return match emit_orb {
            Some(emit) => {
                std::fs::write(emit, &json)
                    .map_err(|e| anyhow::anyhow!("Failed to write lowered .orb: {}", e))?;
                Ok(emit.to_path_buf())
            }
            None => scratch_file(scratch, "_orbital_lolo_lowered.orb", &json),
        };
    }
    let translated = translate_schema_content(content);
    let program = if translated != content {
        scratch_file(scratch, "_orbital_translated_schema.orb", &translated)?
    } else {
        path.to_path_buf()
    };
    if let Some(emit) = emit_orb {
        std::fs::copy(&program, emit)
            .map_err(|e| anyhow::anyhow!("Failed to copy schema to --emit-orb path: {}", e))?;
    }
    Ok(program)
}

/// `orb compile` writes only programs that pass `orb validate`: the source before anything is generated, then every
/// `.orb` the shell wrote.
fn refuse_invalid(program: &Path, source: &Path) -> anyhow::Result<()> {
    let registry = crate::utils::load_services_registry(None)?;
    let diags =
        crate::ports::validate::run_full_validation(program, Some(source), None, registry, false)?;
    if diags.errors.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{} fails `orb validate` with {} error(s), so it is not compiled:\n{}",
        if program == source {
            program.display().to_string()
        } else {
            format!("{} (compiled from {})", program.display(), source.display())
        },
        diags.errors.len(),
        diags.errors.join("\n")
    )
}

pub(crate) fn compile_run(request: CompileRequest) -> anyhow::Result<CompileReport> {
    let CompileRequest {
        path,
        output,
        shell,
        mode,
        frontend,
        backend,
        backend_url,
        patterns_path,
        dev_logs,
        host,
        emit_orb,
        source,
    } = request;
    if frontend.is_some() || backend.is_some() {
        return compile_mixed(path, output, frontend, backend);
    }
    let mut lines = vec![
        format!("Compiling schema: {}", path.display()),
        format!("Output directory: {}", output.display()),
        format!("Target shell: {:?}", shell),
        format!("Mode: {:?}", mode),
    ];
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;
    let mut scratch = None;
    let compile_path = program_file(path, &content, emit_orb, &mut scratch)?;
    let lowered = std::fs::read_to_string(&compile_path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;
    let options = generate_options(match mode {
        ShellMode::Full => orbital_compiler::ShellMode::Full,
        ShellMode::Client => orbital_compiler::ShellMode::Client,
        ShellMode::Server => orbital_compiler::ShellMode::Server,
    })?;
    let mut shell_options = BTreeMap::new();
    if let ShellType::Typescript = shell {
        shell_options.insert(
            "schemaLanguage".to_string(),
            Value::from(detect_language(&lowered).unwrap_or_else(|| "en".to_string())),
        );
        shell_options.insert(
            "devLogs".to_string(),
            Value::from(if dev_logs { "true" } else { "false" }),
        );
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
    refuse_invalid(path, path)?;
    let response = shell_programs::generate(
        shell.program(),
        &compile_path,
        source.unwrap_or(path),
        output,
        &options,
        shell_options,
    )?;
    for file in response.files.iter().filter(|f| f.ends_with(".orb")) {
        refuse_invalid(&output.join(file), path)?;
    }
    lines.extend(shell_programs::report_lines(&response));
    Ok(CompileReport {
        lines,
        files: response.files.len(),
        warnings: response.warnings.clone(),
        next_steps: response.next_steps.clone(),
    })
}

fn compile_mixed(
    path: &PathBuf,
    output: &PathBuf,
    frontend: Option<ShellType>,
    backend: Option<ShellType>,
) -> anyhow::Result<CompileReport> {
    let (front, back) = (frontend.map(mixed_shell), backend.map(mixed_shell));
    if let Some(refusal) = mixed::refuse_pair(front, back) {
        anyhow::bail!("{refusal}");
    }
    refuse_invalid(path, path)?;
    let mut lines = vec![
        format!("Compiling schema: {}", path.display()),
        format!("Output directory: {}", output.display()),
        "Mixed mode compilation:".to_string(),
    ];
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read schema file: {}", e))?;
    let mut scratch = None;
    let compile_path = program_file(path, &content, None, &mut scratch)?;
    let mut total_files = 0;
    for (shell, leg, leg_mode) in [
        (frontend, "frontend", orbital_compiler::ShellMode::Client),
        (backend, "backend", orbital_compiler::ShellMode::Server),
    ] {
        let Some(shell) = shell else { continue };
        let leg_output = output.join(leg);
        lines.push(format!(
            "  {}: {:?} -> {}",
            if leg == "frontend" {
                "Frontend"
            } else {
                "Backend"
            },
            shell,
            leg_output.display()
        ));
        let mut shell_options = BTreeMap::new();
        if leg == "frontend" {
            if let ShellType::Typescript = shell {
                shell_options.insert(
                    "schemaLanguage".to_string(),
                    Value::from(detect_language(&content).unwrap_or_else(|| "en".to_string())),
                );
            }
        }
        let response = shell_programs::generate(
            shell.program(),
            &compile_path,
            path,
            &leg_output,
            &generate_options(leg_mode)?,
            shell_options,
        )?;
        for file in response.files.iter().filter(|f| f.ends_with(".orb")) {
            refuse_invalid(&leg_output.join(file), path)?;
        }
        total_files += response.files.len();
    }
    lines.push(String::new());
    lines.push(format!("✅ Generated {} total files", total_files));
    let mut next_steps = Vec::new();
    if front.is_some() && back.is_some() {
        std::fs::write(
            output.join("docker-compose.yaml"),
            mixed::docker_compose(front, back),
        )?;
        lines.push("📦 Generated docker-compose.yaml".to_string());
        let env = mixed::frontend_env(front, back);
        if !env.is_empty() {
            std::fs::write(output.join("frontend").join(".env"), &env)?;
            lines.push("🔧 Generated frontend/.env with API_URL".to_string());
        }
        lines.push(String::new());
        lines.push("Next steps:".to_string());
        for step in mixed::run_steps(&output.display().to_string()) {
            if step.is_empty() {
                lines.push(step);
            } else {
                lines.push(format!("  {step}"));
                next_steps.push(step);
            }
        }
    }
    Ok(CompileReport {
        lines,
        files: total_files,
        warnings: Vec::new(),
        next_steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_compiles_each_read_back_their_own_lowered_program() {
        let handles: Vec<_> = (0..16)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut slot = None;
                    let own = format!("program-{i}");
                    let path = scratch_file(&mut slot, "_orbital_lolo_lowered.orb", &own).unwrap();
                    std::thread::yield_now();
                    (own, path, slot)
                })
            })
            .collect();
        let runs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut seen = std::collections::BTreeSet::new();
        for (own, path, _slot) in &runs {
            assert_eq!(&std::fs::read_to_string(path).unwrap(), own);
            assert!(seen.insert(path.clone()));
        }
        let paths: Vec<PathBuf> = runs.iter().map(|(_, path, _)| path.clone()).collect();
        drop(runs);
        assert!(
            paths.iter().all(|p| !p.exists()),
            "a finished run leaves its scratch behind"
        );
    }

    #[test]
    fn one_run_keeps_one_scratch_directory_for_all_its_files() {
        let mut slot = None;
        let a = scratch_file(&mut slot, "a.orb", "A").unwrap();
        let b = scratch_file(&mut slot, "b.orb", "B").unwrap();
        assert_eq!(a.parent(), b.parent());
    }

    #[test]
    fn a_mixed_compile_refuses_a_chrome_extension_and_an_android_backend_before_reading_the_program(
    ) {
        let (path, output) = (
            PathBuf::from("/nowhere/n.lolo"),
            PathBuf::from("/nowhere/out"),
        );
        let chrome = compile_mixed(&path, &output, Some(ShellType::ChromeExtension), None)
            .unwrap_err()
            .to_string();
        assert!(chrome.contains("whole program"), "{chrome}");
        let android = compile_mixed(
            &path,
            &output,
            Some(ShellType::Typescript),
            Some(ShellType::Android),
        )
        .unwrap_err()
        .to_string();
        assert!(
            android.contains("Android shell cannot be used as a backend"),
            "{android}"
        );
    }

    #[test]
    fn a_lolo_program_is_lowered_to_the_emit_path_and_an_orb_is_copied_through() {
        let dir = tempfile::tempdir().unwrap();
        let orb = dir.path().join("p.orb");
        std::fs::write(&orb, r#"{"name":"P","orbitals":[]}"#).unwrap();
        let emit = dir.path().join("out.orb");
        let mut scratch = None;
        let used = program_file(
            &orb,
            r#"{"name":"P","orbitals":[]}"#,
            Some(&emit),
            &mut scratch,
        )
        .unwrap();
        assert_eq!(used, orb);
        assert_eq!(
            std::fs::read_to_string(&emit).unwrap(),
            r#"{"name":"P","orbitals":[]}"#
        );
        let broken = dir.path().join("b.lolo");
        std::fs::write(&broken, "orbital {").unwrap();
        assert!(program_file(&broken, "orbital {", None, &mut scratch)
            .unwrap_err()
            .to_string()
            .contains("Failed to compile .lolo source"));
    }
}
