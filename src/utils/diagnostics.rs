//! Single merge point for the three inline-collected diagnostic families
//! (`TraitRefConfigDiagnostic` / `OrbitalRefDiagnostic` / `UsesConfigDiagnostic`)
//! into a `ValidationResult`, shared by `orb validate --json`
//! (`commands/validate.rs::validate_schema_json`) and the plain-text path
//! (`commands/validate.rs::run_full_validation`) — previously duplicated at
//! both call sites, which is how the cascade-suppression pass (below) could
//! have drifted between them.

use orbital_compiler::phases::inline::{
    OrbitalRefDiagnostic, TraitRefConfigDiagnostic, UsesConfigDiagnostic,
};
use orbital_compiler::phases::validation::orbital_ref_config::{
    suppress_cascaded_by_orbital_ref, validate_orbital_ref_diagnostics,
    validate_uses_config_diagnostics,
};
use orbital_compiler::phases::validation::orbital_ref_extend::validate_orbital_ref_extend_diagnostics;
use orbital_compiler::phases::validation::trait_ref_config::validate_trait_ref_config_diagnostics;
use orbital_compiler::phases::validation::ValidationResult;
use orbital_core::schema::OrbitalSchema;

/// Merges the trait-ref / orbital-ref / uses-config diagnostics into
/// `result`, then runs the cascade-suppression pass (owner ruling,
/// 2026-09-06): once an `ORB_O_*`/`ORB_U_*` row already names a broken
/// out-of-orbital entity, every other validator's independent rediscovery of
/// the SAME entity under the SAME orbital is the identical root cause, not a
/// second finding — dropped, with the count folded into the root row's
/// message.
pub(crate) fn merge_post_inline_diagnostics(
    result: &mut ValidationResult,
    trait_ref_diagnostics: &[TraitRefConfigDiagnostic],
    orbital_ref_diagnostics: &[OrbitalRefDiagnostic],
    uses_config_diagnostics: &[UsesConfigDiagnostic],
    schema: &OrbitalSchema,
) {
    result.merge(validate_trait_ref_config_diagnostics(trait_ref_diagnostics));
    result.merge(validate_orbital_ref_diagnostics(
        orbital_ref_diagnostics,
        schema,
    ));
    result.merge(validate_uses_config_diagnostics(uses_config_diagnostics));
    result.merge(validate_orbital_ref_extend_diagnostics(
        orbital_ref_diagnostics,
    ));
    suppress_cascaded_by_orbital_ref(result, orbital_ref_diagnostics, schema);
}
