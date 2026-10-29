//! `orb` runs every shell as a program (`orbital_core::shell_protocol`); it links none.

use orbital_compiler::generate::{generate_from_file_with_source_using, GenerateError};
use orbital_compiler::GenerateOptions;
use orbital_core::ir::OirModule;
use orbital_core::shell_protocol::{
    self, GenerateRequest, GenerateResponse, ShellDescription, ShellFailure, CODE_IR_INCOMPATIBLE, IR_VERSION,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) struct ShellProgram {
    pub binary: PathBuf,
    pub description: ShellDescription,
}

fn failure(f: ShellFailure) -> anyhow::Error {
    anyhow::anyhow!("[{}] {}", f.code, f.message)
}

/// The directory of the running `orb`, where a release ships its shells.
pub(crate) fn own_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.canonicalize().ok()?.parent().map(Path::to_path_buf)
}

/// Find the `name` shell and check it accepts this orb's IR.
pub(crate) fn locate(name: &str) -> anyhow::Result<ShellProgram> {
    let cwd = std::env::current_dir()?;
    let user_store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    let binary = shell_protocol::find_shell(name, own_dir().as_deref(), &cwd, &user_store).map_err(failure)?;
    let description = shell_protocol::describe(&binary).map_err(failure)?;
    if !description.ir_versions.accepts(IR_VERSION) {
        anyhow::bail!(
            "[{CODE_IR_INCOMPATIBLE}] the {} shell {} ({}) accepts IR {}..={}, this orb emits IR {IR_VERSION}; run `orb update`",
            description.name,
            description.version,
            binary.display(),
            description.ir_versions.min,
            description.ir_versions.max,
        );
    }
    Ok(ShellProgram { binary, description })
}

fn request(
    module: &OirModule,
    resolved: Option<orbital_core::OrbitalSchema>,
    served: Option<orbital_core::OrbitalSchema>,
    source_path: &Path,
    options: &GenerateOptions,
    shell_options: BTreeMap<String, Value>,
) -> GenerateRequest {
    GenerateRequest {
        ir_version: IR_VERSION,
        module: module.clone(),
        resolved,
        served,
        source_path: Some(source_path.to_path_buf()),
        write: options.write_options(),
        options: shell_options,
    }
}

/// Compile `compile_path` (lowered from `source_path`) and have the `name` shell write the project.
pub(crate) fn generate(
    name: &str,
    compile_path: &Path,
    source_path: &Path,
    output: &Path,
    options: &GenerateOptions,
    shell_options: BTreeMap<String, Value>,
) -> anyhow::Result<GenerateResponse> {
    let shell = locate(name)?;
    let resolved = if shell.description.needs_resolved || shell.description.needs_served {
        Some(resolve_compiled_schema(compile_path, source_path)?)
    } else {
        None
    };
    // A server is the program's own declaration: compile never composes one beside it.
    let served = match (&resolved, shell.description.needs_served && options.mode != orbital_compiler::ShellMode::Client) {
        (Some(app), true) if crate::commands::program_command::declares_server_endpoint(app) => Some(app.clone()),
        (Some(_), true) => anyhow::bail!(
            "{} declares no http endpoint, so there is no server to compile: compose one in the program (`orbital Api = Server.orbitals.HttpServerOrbital {{ config {{ host: …, port: … }} }}`) or compile `--mode client`",
            source_path.display()
        ),
        _ => None,
    };
    let resolved = resolved.filter(|_| shell.description.needs_resolved);
    let response = generate_from_file_with_source_using(compile_path, source_path, output, options, |module, out| {
        shell_protocol::invoke(&shell.binary, &request(module, resolved, served, source_path, options, shell_options), out)
            .map_err(GenerateError::ShellProgram)
    })?;
    Ok(response)
}

/// The `name` shell's files for `module`, written nowhere.
pub(crate) fn emit(
    name: &str,
    module: &OirModule,
    resolved: Option<orbital_core::OrbitalSchema>,
    served: Option<orbital_core::OrbitalSchema>,
    source_path: &Path,
    options: &GenerateOptions,
    shell_options: BTreeMap<String, Value>,
) -> anyhow::Result<orbital_core::generate::GeneratedOutput> {
    let shell = locate(name)?;
    shell_protocol::invoke_emit(&shell.binary, &request(module, resolved, served, source_path, options, shell_options)).map_err(failure)
}

/// The lines `orb compile` reports for a shell's reply.
pub(crate) fn report_lines(response: &GenerateResponse) -> Vec<String> {
    let mut lines = vec![String::new(), format!("✅ Generated {} files", response.files.len())];
    if !response.warnings.is_empty() {
        lines.push(String::new());
        lines.push("Warnings:".to_string());
        lines.extend(response.warnings.iter().map(|warning| format!("  ⚠️  {warning}")));
    }
    if !response.next_steps.is_empty() {
        lines.push(String::new());
        lines.push("Next steps:".to_string());
        lines.extend(response.next_steps.iter().map(|step| format!("  {step}")));
    }
    lines
}

/// The resolved program a shell embeds as data, as the compiled path compiles it (lazy behaviors included), resolved
/// against the original `source_path` so relative imports find their files.
pub(crate) fn resolve_compiled_schema(compile_path: &Path, source_path: &Path) -> anyhow::Result<orbital_core::OrbitalSchema> {
    let schema = orbital_compiler::phases::parse_file(compile_path)
        .map_err(|e| anyhow::anyhow!("Failed to parse schema for the embedded resolved schema: {}", e))?;
    orbital_compiler::phases::resolve_compiled_program_with_path(schema, &compiler_options_or_bail()?, Some(source_path))
        .map_err(|e| anyhow::anyhow!("Failed to resolve the embedded resolved schema: {}", e))
}

/// `CompilerOptions::from_env()`, bailing when behavior-package discovery failed: that is environment, never a schema
/// authoring error, so a compile must not proceed as if no packages were installed.
pub(crate) fn compiler_options_or_bail() -> anyhow::Result<orbital_compiler::CompilerOptions> {
    use orbital_core::behavior_packages::DiscoverError;
    let mut options = orbital_compiler::CompilerOptions::from_env();
    options.service_runtimes = Some(crate::utils::load_services_registry(None)?.runtimes);
    if let Some(e) = &options.discovery_error {
        let code = match e {
            DiscoverError::DuplicateBehaviorName { .. } => "ORB_BEHAVIOR_NAME_DUPLICATE",
            DiscoverError::PrefixConflict { .. } => "ORB_BEHAVIOR_PREFIX_CONFLICT",
            DiscoverError::IncompatibleCompiler { .. } => orbital_core::language_data::CODE_INCOMPATIBLE,
            DiscoverError::MissingDeclaredPath { .. }
            | DiscoverError::InvalidSidecarLock { .. }
            | DiscoverError::InvalidPackageJson { .. }
            | DiscoverError::Io { .. } => "ORB_BEHAVIOR_PACKAGE_INVALID",
        };
        anyhow::bail!("[{code}] {e} (validator environment error — the schema was NOT compiled; retry, do not edit the schema)");
    }
    Ok(options)
}
