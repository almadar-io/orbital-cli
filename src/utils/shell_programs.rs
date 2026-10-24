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

/// The client a compiled server serves, relative to the project root the server runs from.
const SERVED_CLIENT_DIR: &str = "packages/client/dist";

/// Where a compiled server listens: the `std-http1` host and port knobs; `PORT` in its environment replaces the port.
pub(crate) struct ServedAddress<'a> {
    pub host: &'a str,
    pub port: u16,
}

/// Compile `compile_path` (lowered from `source_path`) and have the `name` shell write the project.
pub(crate) fn generate(
    name: &str,
    compile_path: &Path,
    source_path: &Path,
    output: &Path,
    options: &GenerateOptions,
    shell_options: BTreeMap<String, Value>,
    address: &ServedAddress,
) -> anyhow::Result<GenerateResponse> {
    let shell = locate(name)?;
    let resolved = if shell.description.needs_resolved || shell.description.needs_served {
        Some(crate::commands::compile::resolve_compiled_schema(compile_path, source_path)?)
    } else {
        None
    };
    let served = match (&resolved, shell.description.needs_served && options.mode != orbital_compiler::ShellMode::Client) {
        (Some(app), true) => Some(crate::commands::program_command::served_schema(
            app,
            &crate::commands::program_command::server_overrides(address.host, address.port, true, Some(SERVED_CLIENT_DIR)),
        )?),
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
