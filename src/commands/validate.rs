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
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_compiler::phases::{inline_schema, InlineConfig};
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
    if let Err(e) = inline_schema(&mut schema, &InlineConfig::default(), Some(path.as_path())) {
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
    let comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);

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
    let options = CompilerOptions::default();
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

/// Native Rust schema validation (no kflow dependency)
fn validate_schema_native(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_actions: std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    use orbital_compiler::{
        compile_schema, validate_comprehensive_full, CompilerOptions, SchemaLanguage,
    };
    use orbital_compiler::phases::{inline_schema, enrich_schema};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    // Step 1: Parse schema (auto-detect LOLO vs JSON syntax)
    let content_raw = std::fs::read_to_string(path)?;
    // Detect i18n language BEFORE translation for validation context (GAP-V001)
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => SchemaLanguage::NonLatin(lang),
        _ => SchemaLanguage::English,
    };
    let mut schema = parse_schema_auto(&content_raw)?;

    // Step 1b: Inline + enrich before validation so that `uses`-based trait/entity/page
    // references are resolved and default collection names are present. Without this,
    // validate_comprehensive_full raises false ORB_T_UNDEFINED_TRAIT / ORB_E_MISSING_COLLECTION
    // errors for any schema that uses `uses` declarations.
    let default_opts = CompilerOptions::default();
    // Best-effort: inline errors (e.g. missing std behavior) surface in Step 3 compile.
    let _ = inline_schema(&mut schema, &default_opts.inline, Some(path));
    enrich_schema(&mut schema, &default_opts.enrichment);

    // Step 2: Run comprehensive schema validation (all semantic checks, on resolved schema)
    let comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_actions);

    let mut has_errors = false;

    if !comprehensive_result.is_valid() {
        eprintln!("Schema validation errors:");
        for error in &comprehensive_result.errors {
            eprintln!("  ❌ [{}] {}: {}", error.code, error.path, error.message);
            if let Some(suggestion) = &error.suggestion {
                eprintln!("     💡 {}", suggestion);
            }
        }
        has_errors = true;
    }

    if comprehensive_result.has_warnings() {
        println!("Schema validation warnings:");
        for warning in &comprehensive_result.warnings {
            println!(
                "  ⚠️  [{}] {}: {}",
                warning.code, warning.path, warning.message
            );
            if let Some(suggestion) = &warning.suggestion {
                println!("     💡 {}", suggestion);
            }
        }
    }

    // Step 3: Compile to OIR (validates references)
    // Skip inline + enrich since Step 1b already ran them on this schema.
    let mut options = CompilerOptions::default();
    options.inline.skip_inline = true;
    let module = match compile_schema(schema, &options) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Compilation error: {}", e);
            anyhow::bail!("Schema compilation failed: {}", e);
        }
    };

    // Step 4: Validate OIR (post-resolution semantic validation)
    let validator = OirValidator::new(&module);
    let oir_result = validator.validate();

    if !oir_result.is_valid() {
        eprintln!("OIR validation errors:");
        for error in &oir_result.errors {
            eprintln!("  ❌ {}", error);
        }
        has_errors = true;
    } else if oir_result.has_warnings() {
        println!("OIR warnings:");
        for warning in oir_result
            .errors
            .iter()
            .filter(|e| e.severity == ValidationSeverity::Warning)
        {
            println!("  ⚠️  {}", warning);
        }
    }

    if has_errors {
        anyhow::bail!(
            "Schema validation failed with {} error(s)",
            comprehensive_result.error_count() + oir_result.error_count()
        )
    } else {
        Ok(())
    }
}
