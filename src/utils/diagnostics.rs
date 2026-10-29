//! Single merge point for the inline-collected diagnostic families
//! (`OrbitalRefDiagnostic` / `UsesConfigDiagnostic`)
//! into a `ValidationResult`, shared by `orb validate --json`
//! (`commands/validate.rs::validate_schema_json`) and the plain-text path
//! (`commands/validate.rs::run_full_validation`) — previously duplicated at
//! both call sites, which is how the cascade-suppression pass (below) could
//! have drifted between them.

use orbital_compiler::phases::inline::{OrbitalRefDiagnostic, UsesConfigDiagnostic};
use orbital_compiler::phases::validation::orbital_ref_config::{
    suppress_cascaded_with, validate_orbital_ref_diagnostics,
    validate_uses_config_diagnostics,
};
use orbital_compiler::phases::validation::ValidationResult;
use orbital_core::schema::OrbitalSchema;

/// Merges the orbital-ref / uses-config diagnostics into
/// `result`, then runs the cascade-suppression pass (owner ruling,
/// 2026-09-06): once an `ORB_O_*`/`ORB_U_*` row already names a broken
/// out-of-orbital entity, every other validator's independent rediscovery of
/// the SAME entity under the SAME orbital is the identical root cause, not a
/// second finding — dropped, with the count folded into the root row's
/// message. The std lint `findings` merged into the report are held to the same rule.
pub(crate) fn merge_post_inline_diagnostics(
    result: &mut ValidationResult,
    orbital_ref_diagnostics: &[OrbitalRefDiagnostic],
    uses_config_diagnostics: &[UsesConfigDiagnostic],
    schema: &OrbitalSchema,
    findings: &mut Vec<crate::ports::validate::ProgramFinding>,
) {
    result.merge(validate_orbital_ref_diagnostics(
        orbital_ref_diagnostics,
        schema,
    ));
    result.merge(validate_uses_config_diagnostics(uses_config_diagnostics));
    suppress_cascaded_with(result, findings, orbital_ref_diagnostics, schema);
}
