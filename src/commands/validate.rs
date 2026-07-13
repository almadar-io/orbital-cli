//! `orb validate` — native Rust schema validation (text + JSON output).

use crate::utils::{
    detect_language, load_extra_patterns, load_services_registry, parse_schema_auto,
    parse_schema_auto_typed, typed_parse_error_to_json,
};
use crate::{ValidationJsonError, ValidationJsonOutput, ValidationJsonWarning, ValidationStats};
use std::path::PathBuf;

/// Validate schema using native Rust validation
pub(crate) fn validate_schema(
    path: &PathBuf,
    json_output: bool,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Load extra patterns (explicit path or auto-discover)
    let extra_patterns = load_extra_patterns(path, patterns_path);

    // Load services registry (explicit path or auto-discover)
    let service_actions = load_services_registry(services_path);

    if json_output {
        return validate_schema_json(path, extra_patterns, service_actions);
    }

    println!("Validating schema: {}", path.display());

    // Use native Rust validation only
    validate_schema_native(path, extra_patterns, service_actions)
}

/// Validate schema with JSON output (for programmatic consumption by DeepAgent)
fn validate_schema_json(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    use orbital_compiler::phases::inline::inline_schema_with_config_diagnostics;
    use orbital_compiler::phases::validation::trait_ref_config::validate_trait_ref_config_diagnostics;
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    // Step 1: Parse schema (auto-detect LOLO vs JSON syntax)
    let content_raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            // Handle file read error similar to parse error
            let output = ValidationJsonOutput {
                success: true,
                valid: false,
                errors: Some(vec![ValidationJsonError {
                    code: "FILE_READ_ERROR".to_string(),
                    path: "".to_string(),
                    message: format!("Failed to read schema file: {}", e),
                    suggestion: None,
                    fix_guidance: None,
                    expected_shape: None,
                    valid_values: None,
                    line: None,
                    column: None,
                }]),
                warnings: None,
                stats: None,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }
    };

    let mut schema = match parse_schema_auto_typed(&content_raw) {
        Ok(s) => s,
        Err(e) => {
            // Map parse/lower/type errors into typed `ValidationJsonError`
            // rows — includes the `ORB_T_TYPE_*` family for `.lolo` type
            // check failures so consumers (agent retry loops, editor
            // tooling) can switch on the code instead of regexing a
            // human-readable string.
            let output = ValidationJsonOutput {
                success: true,
                valid: false,
                errors: Some(typed_parse_error_to_json(&e)),
                warnings: None,
                stats: None,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }
    };

    // Run the inline phase BEFORE validation. Composed .orb files carry
    // `uses:` imports and `ref:` strings; without inlining, the validator
    // reports every imported trait/page/entity as "undefined". The inline
    // phase resolves external refs via the embedded std loader and
    // filesystem — same path `orb compile` uses. Validation-only mode used
    // to skip this and produced false positives on every multi-use schema.
    let mut trait_ref_config_diagnostics = Vec::new();
    // from_env, NOT default: the plain-text validate path resolves imports
    // through dev-workspace registries (ALMADAR_DEV) while this --json path
    // used InlineConfig::default() and silently inlined the STALE baked
    // copy of any behavior edited since the last bake — rabit's
    // validateWithOrbitalCLI (--json) and a manual `orb validate` then
    // disagreed about the same schema.
    let inline_config = CompilerOptions::from_env().inline;
    if let Err(e) = inline_schema_with_config_diagnostics(
        &mut schema,
        &inline_config,
        Some(path.as_path()),
        &mut trait_ref_config_diagnostics,
    ) {
        // Source-tag from the inline-phase variant: when the producer
        // populated a structured path (e.g. DuplicateTraitName carries
        // `orbitals[name=Foo].traits[idx]`), surface it on the JSON
        // error so consumers (rabit's dispatch-updates owner-routing)
        // get concrete coordinates instead of an empty string. Variants
        // without a path return "" — same as before.
        let inline_path = e.structured_path().to_string();
        let output = ValidationJsonOutput {
            success: true,
            valid: false,
            errors: Some(vec![ValidationJsonError {
                code: "INLINE_ERROR".to_string(),
                path: inline_path,
                message: format!("Failed to resolve external references: {}", e),
                suggestion: Some("Check `uses:` imports resolve to known behaviors".to_string()),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                line: None,
                column: None,
            }]),
            warnings: None,
            stats: None,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Collect stats
    // Note: Traits are now only defined inline within orbitals, not at schema level.
    let stats = ValidationStats {
        orbitals: schema.orbitals.len(),
        entities: schema.orbitals.len(), // Each orbital has one entity
        traits: schema
            .orbitals
            .iter()
            .map(|o| o.traits.len())
            .sum::<usize>(),
        pages: schema
            .orbitals
            .iter()
            .filter_map(|o| o.pages.as_ref())
            .map(|p| p.len())
            .sum(),
    };

    // Step 2: Run comprehensive schema validation (with language awareness)
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => orbital_compiler::SchemaLanguage::NonLatin(lang),
        _ => orbital_compiler::SchemaLanguage::English,
    };
    let mut comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);
    comprehensive_result.merge(validate_trait_ref_config_diagnostics(
        &trait_ref_config_diagnostics,
    ));

    let mut all_errors: Vec<ValidationJsonError> = Vec::new();
    let mut all_warnings: Vec<ValidationJsonWarning> = Vec::new();

    // Convert comprehensive validation errors
    for error in &comprehensive_result.errors {
        all_errors.push(ValidationJsonError {
            code: error.code.to_string(),
            path: error.path.clone(),
            message: error.message.clone(),
            suggestion: error.suggestion.clone(),
            fix_guidance: error.fix_guidance.clone(),
            expected_shape: error.expected_shape.clone(),
            valid_values: error.valid_values.clone(),
            line: None,
            column: None,
        });
    }

    for warning in &comprehensive_result.warnings {
        all_warnings.push(ValidationJsonWarning {
            code: warning.code.to_string(),
            path: warning.path.clone(),
            message: warning.message.clone(),
            suggestion: warning.suggestion.clone(),
        });
    }

    // Step 3: Try to compile to OIR for additional validation
    let options = CompilerOptions::from_env();
    if let Ok(module) = compile_schema(schema, &options) {
        let validator = OirValidator::new(&module);
        let oir_result = validator.validate();

        for error in &oir_result.errors {
            if error.severity == ValidationSeverity::Error {
                all_errors.push(ValidationJsonError {
                    code: "OIR_ERROR".to_string(),
                    path: error.path.clone(),
                    message: error.message.clone(),
                    suggestion: None,
                    fix_guidance: None,
                    expected_shape: None,
                    valid_values: None,
                    line: None,
                    column: None,
                });
            } else {
                all_warnings.push(ValidationJsonWarning {
                    code: "OIR_WARNING".to_string(),
                    path: error.path.clone(),
                    message: error.message.clone(),
                    suggestion: None,
                });
            }
        }
    }

    let output = ValidationJsonOutput {
        success: true,
        valid: all_errors.is_empty(),
        errors: if all_errors.is_empty() {
            None
        } else {
            Some(all_errors)
        },
        warnings: if all_warnings.is_empty() {
            None
        } else {
            Some(all_warnings)
        },
        stats: Some(stats),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Collected validation diagnostics (formatted, self-describing strings).
/// Shared by `orb validate` (human formatting) and `orb verify` (Layer 0 verdict)
/// so the two commands can never diverge on what counts as a validation error.
pub(crate) struct ValidationDiagnostics {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Run the full static validation pipeline on `path` — the SAME pipeline
/// `orb validate` runs: parse -> inline -> enrich -> `validate_comprehensive_full`
/// -> compile to OIR -> `OirValidator`. Returns collected diagnostics; never
/// bails on validation errors (those live in `errors`), only on I/O failures.
/// `extra_patterns` / `service_actions` mirror the `orb validate` inputs.
pub(crate) fn run_full_validation(
    path: &std::path::Path,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<ValidationDiagnostics> {
    use orbital_compiler::phases::enrich_schema;
    use orbital_compiler::phases::inline::inline_schema_with_config_diagnostics;
    use orbital_compiler::phases::validation::trait_ref_config::validate_trait_ref_config_diagnostics;
    use orbital_compiler::{
        compile_schema, validate_comprehensive_full, CompilerOptions, SchemaLanguage,
    };
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    let content_raw = std::fs::read_to_string(path)?;
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => SchemaLanguage::NonLatin(lang),
        _ => SchemaLanguage::English,
    };
    let mut schema = parse_schema_auto(&content_raw)?;

    // Inline + enrich before validation so `uses`-based trait/entity/page refs
    // resolve (otherwise validate_comprehensive_full false-positives on every
    // multi-use schema). Mirrors the `orb validate` path. `from_env` enables
    // dev-workspace resolution (ALMADAR_DEV) so behavior edits apply without a
    // rebuild + rebake.
    let default_opts = CompilerOptions::from_env();
    let mut trait_ref_config_diagnostics = Vec::new();
    let _ = inline_schema_with_config_diagnostics(
        &mut schema,
        &default_opts.inline,
        Some(path),
        &mut trait_ref_config_diagnostics,
    );
    enrich_schema(&mut schema, &default_opts.enrichment);

    let mut comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);
    comprehensive_result.merge(validate_trait_ref_config_diagnostics(
        &trait_ref_config_diagnostics,
    ));

    let mut errors: Vec<String> = comprehensive_result
        .errors
        .iter()
        .map(|e| {
            let mut s = format!("[{}] {}: {}", e.code, e.path, e.message);
            if let Some(sug) = &e.suggestion {
                s.push_str(&format!(" \u{1f4a1} {sug}"));
            }
            s
        })
        .collect();
    let mut warnings: Vec<String> = comprehensive_result
        .warnings
        .iter()
        .map(|w| {
            let mut s = format!("[{}] {}: {}", w.code, w.path, w.message);
            if let Some(sug) = &w.suggestion {
                s.push_str(&format!(" \u{1f4a1} {sug}"));
            }
            s
        })
        .collect();

    // Compile to OIR (schema already inlined+enriched above -> skip re-inline).
    let mut options = CompilerOptions::from_env();
    options.inline.skip_inline = true;
    match compile_schema(schema, &options) {
        Ok(module) => {
            let oir = OirValidator::new(&module).validate();
            for e in &oir.errors {
                if e.severity == ValidationSeverity::Error {
                    errors.push(format!("OIR {e}"));
                } else {
                    warnings.push(format!("OIR {e}"));
                }
            }
            // Phase 4 (C-GUARD-NO-BOOLEAN-CHECK): warn on guards that resolve
            // to a definitively non-boolean type (arithmetic / literals /
            // number-returning operator calls) — they run with JS truthiness.
            for w in orbital_compiler::type_infer::collect_guard_type_warnings(&module) {
                warnings.push(w);
            }
        }
        Err(e) => errors.push(format!("Compilation error: {e}")),
    }

    Ok(ValidationDiagnostics { errors, warnings })
}

/// Native Rust schema validation (human text output). Thin formatting layer
/// over `run_full_validation` so `orb validate` and `orb verify` share one
/// validation code path and can never diverge.
fn validate_schema_native(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    let diags = run_full_validation(path, extra_patterns, service_actions)?;

    if !diags.errors.is_empty() {
        eprintln!("Schema validation errors:");
        for e in &diags.errors {
            eprintln!("  \u{274c} {e}");
        }
        anyhow::bail!(
            "Schema validation failed with {} error(s)",
            diags.errors.len()
        );
    }

    if !diags.warnings.is_empty() {
        println!("Schema validation warnings:");
        for w in &diags.warnings {
            println!("  \u{26a0}\u{fe0f}  {w}");
        }
    }

    Ok(())
}
