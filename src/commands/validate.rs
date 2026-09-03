//! `orb validate` — native Rust schema validation (text + JSON output).

use crate::utils::{
    detect_language, load_extra_patterns, load_services_registry, parse_schema_auto_typed_with_map,
    parse_schema_auto_with_map, typed_parse_error_to_json,
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
    let service_registry = load_services_registry(services_path);

    if json_output {
        return validate_schema_json(path, extra_patterns, service_registry);
    }

    println!("Validating schema: {}", path.display());

    // Use native Rust validation only
    validate_schema_native(path, extra_patterns, service_registry)
}

/// Validate schema with JSON output (for programmatic consumption by DeepAgent)
fn validate_schema_json(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
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

    let (mut schema, source_map) =
        match parse_schema_auto_typed_with_map(&content_raw, &path.display().to_string()) {
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
                    errors: Some(typed_parse_error_to_json(&e, Some(&content_raw))),
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
        // Loader-ENVIRONMENT failures (std path / package roots unresolvable)
        // are validator infrastructure, not schema authoring — a distinct code
        // so downstream agents never treat them as fixable-by-editing. The
        // message carries the effective loader env so an intermittent miss is
        // diagnosable from the consumer's log alone.
        use orbital_compiler::loader::LoaderError;
        use orbital_compiler::phases::inline::InlineError;
        let is_infra = matches!(
            &e,
            InlineError::Loader(LoaderError::StdLibNotConfigured)
                | InlineError::Loader(LoaderError::ScopedPackageNotConfigured(_))
        );
        let mut inline_message = format!("Failed to resolve external references: {}", e);
        if is_infra {
            let lc = &inline_config.loader_config;
            inline_message.push_str(&format!(
                " [loader env: dev_workspace={}, registry_roots={}, std_lib_path_set={}, auto_discover={}, ALMADAR_DEV={}]",
                lc.dev_workspace,
                lc.behaviors_registry_roots.len(),
                lc.std_lib_path.is_some(),
                lc.auto_discover_registries,
                std::env::var("ALMADAR_DEV").map(|v| !v.is_empty()).unwrap_or(false),
            ));
        }
        // Import-carrying loader errors have no spans, but the failing
        // import string appears verbatim on a `uses` line — locate it so
        // the agent has an edit anchor (run 36: an invented import stalled
        // 4 repair rounds at line null).
        let import_needle = match &e {
            InlineError::Loader(LoaderError::InvalidPrimitiveImport(s)) => Some(s.clone()),
            InlineError::Loader(LoaderError::StdImportNotCanonical { given, .. }) => {
                Some(given.clone())
            }
            _ => None,
        };
        let (inline_line, inline_column) = import_needle
            .and_then(|needle| {
                let src = std::fs::read_to_string(&path).ok()?;
                let find = |n: &str| {
                    src.lines().enumerate().find_map(|(i, l)| {
                        l.find(n)
                            .map(|c| (Some((i + 1) as u32), Some((c + 1) as u32)))
                    })
                };
                // The loader may have normalized the path before failing
                // (`@std/graphs/X` reported as `std/behaviors/X`), so fall
                // back to the last path segment, which survives rewriting.
                find(&needle).or_else(|| {
                    needle
                        .rsplit('/')
                        .next()
                        .filter(|seg| !seg.is_empty())
                        .and_then(|seg| find(seg))
                })
            })
            .unwrap_or((None, None));
        let output = ValidationJsonOutput {
            success: true,
            valid: false,
            errors: Some(vec![ValidationJsonError {
                code: if is_infra {
                    "INLINE_INFRA_ERROR".to_string()
                } else {
                    e.structured_code().to_string()
                },
                path: inline_path,
                message: inline_message,
                suggestion: Some(
                    if is_infra {
                        "Validator environment error — the schema was NOT evaluated; retry, do not edit the schema"
                    } else {
                        "Check `uses:` imports resolve to known behaviors"
                    }
                    .to_string(),
                ),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                line: inline_line,
                column: inline_column,
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
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_registry);
    comprehensive_result.merge(validate_trait_ref_config_diagnostics(
        &trait_ref_config_diagnostics,
    ));

    let mut all_errors: Vec<ValidationJsonError> = Vec::new();
    let mut all_warnings: Vec<ValidationJsonWarning> = Vec::new();

    // Convert comprehensive validation errors. When the schema came from
    // `.lolo`, `source_map` resolves `error.path` (an L2 JSON-pointer-style
    // path) back to the originating `.lolo` line:col — best-effort (see
    // `LoloSourceMap::lookup`'s longest-prefix fallback); `.orb` JSON input
    // carries no map, so line/column stay `None` there.
    let source_text: Option<String> = source_map
        .as_ref()
        .and_then(|_| std::fs::read_to_string(&path).ok());
    for error in &comprehensive_result.errors {
        let resolved = source_map
            .as_ref()
            .and_then(|m| m.lookup_detailed(&error.path));
        // Last-resort anchor when the path has no map entry at all (config
        // referrer chains, cross-orbital codes — run 37: ORB_CONFIG_DANGLING_REF
        // at line null stalled a session; C-LINE-BLIND family): our messages
        // quote the offending token in backticks, and its first source
        // occurrence is a deterministic edit anchor.
        let quoted_anchor = if resolved.is_none() {
            source_text.as_ref().and_then(|src| {
                let msg = &error.message;
                let start = msg.find('`')? + 1;
                let end = start + msg[start..].find('`')?;
                let token = &msg[start..end];
                if token.is_empty() {
                    return None;
                }
                src.lines()
                    .enumerate()
                    .find_map(|(i, l)| l.find(token).map(|c| (i as u32 + 1, c as u32 + 1)))
            })
        } else {
            None
        };
        // A longest-prefix fallback span points at the nearest tracked
        // ancestor — for expressions spliced in by the inline phase (config
        // defaults, `@trait.X` references) that line contains only the
        // REFERENCE, not the failing expression, and an agent staring at it
        // spins (C-INLINE-SPLICE-LOCATION). Say so instead of implying
        // exactness.
        let fix_guidance = match resolved {
            Some((_, false)) => {
                let note = "Location is approximate (nearest tracked source line). If the shown line only references a config knob or another trait, the failing expression lives where that value is DECLARED (e.g. the trait's config block default) — search the file for the expression named in the message.";
                Some(match &error.fix_guidance {
                    Some(existing) => format!("{existing} {note}"),
                    None => note.to_string(),
                })
            }
            None if quoted_anchor.is_some() => {
                let note = "Line points at the FIRST occurrence of the quoted token — other occurrences may need the same fix.";
                Some(match &error.fix_guidance {
                    Some(existing) => format!("{existing} {note}"),
                    None => note.to_string(),
                })
            }
            _ => error.fix_guidance.clone(),
        };
        all_errors.push(ValidationJsonError {
            code: error.code.to_string(),
            path: error.path.clone(),
            message: error.message.clone(),
            suggestion: error.suggestion.clone(),
            fix_guidance,
            expected_shape: error.expected_shape.clone(),
            valid_values: error.valid_values.clone(),
            line: resolved
                .map(|(s, _)| s.line)
                .or(quoted_anchor.map(|(l, _)| l)),
            column: resolved
                .map(|(s, _)| s.col)
                .or(quoted_anchor.map(|(_, c)| c)),
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

    // Step 3: Try to compile to OIR for additional validation.
    // The `Err` arm must surface — the plain-text path (`run_full_validation`)
    // pushes `Compilation error: {e}` into its errors, so an `if let Ok`
    // here silently dropped compile/resolve failures and let `--json` report
    // `valid:true` on a schema plain `orb validate` rejects (same class of
    // --json/plain divergence the inline `from_env` note above documents).
    let options = CompilerOptions::from_env();
    match compile_schema(schema, &options) {
        Ok(module) => {
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
        Err(e) => {
            all_errors.push(ValidationJsonError {
                code: "COMPILATION_ERROR".to_string(),
                path: "".to_string(),
                message: format!("Compilation error: {e}"),
                suggestion: None,
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                line: None,
                column: None,
            });
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
/// `extra_patterns` / `service_registry` mirror the `orb validate` inputs.
pub(crate) fn run_full_validation(
    path: &std::path::Path,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
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
    let (mut schema, source_map) =
        parse_schema_auto_with_map(&content_raw, &path.display().to_string())?;

    // Inline + enrich before validation so `uses`-based trait/entity/page refs
    // resolve (otherwise validate_comprehensive_full false-positives on every
    // multi-use schema). Mirrors the `orb validate` path. `from_env` enables
    // dev-workspace resolution (ALMADAR_DEV) so behavior edits apply without a
    // rebuild + rebake.
    let default_opts = CompilerOptions::from_env();
    let mut trait_ref_config_diagnostics = Vec::new();
    // An inline failure is FATAL for this run: validators index Inline traits
    // only, so validating the half-inlined schema manufactures spurious
    // ORB_T_UNDEFINED_TRAIT / ORB_X_LISTEN_SOURCE_UNRESOLVED findings that
    // bury the real cause. Report the one true error and stop.
    if let Err(e) = inline_schema_with_config_diagnostics(
        &mut schema,
        &default_opts.inline,
        Some(path),
        &mut trait_ref_config_diagnostics,
    ) {
        let path_part = match e.structured_path() {
            "" => String::new(),
            p => format!(" {p}:"),
        };
        return Ok(ValidationDiagnostics {
            errors: vec![format!(
                "[{}]{} {} (schema not fully resolved — downstream validation suppressed until this is fixed)",
                e.structured_code(),
                path_part,
                e
            )],
            warnings: Vec::new(),
        });
    }
    enrich_schema(&mut schema, &default_opts.enrichment);

    let mut comprehensive_result =
        validate_comprehensive_full(&schema, extra_patterns, schema_language, service_registry);
    comprehensive_result.merge(validate_trait_ref_config_diagnostics(
        &trait_ref_config_diagnostics,
    ));

    // Human path: append ` @ line:col` when the L2 path resolves to a
    // `.lolo` span (`source_map` is `None` for `.orb` JSON input, and a miss
    // just leaves the message as-is — same degradations as the `--json` path).
    let mut errors: Vec<String> = comprehensive_result
        .errors
        .iter()
        .map(|e| {
            let mut s = format!("[{}] {}: {}", e.code, e.path, e.message);
            if let Some(span) = source_map.as_ref().and_then(|m| m.lookup(&e.path)) {
                s.push_str(&format!(" @ {}:{}", span.line, span.col));
            }
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
            if let Some(span) = source_map.as_ref().and_then(|m| m.lookup(&w.path)) {
                s.push_str(&format!(" @ {}:{}", span.line, span.col));
            }
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
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
) -> anyhow::Result<()> {
    let diags = run_full_validation(path, extra_patterns, service_registry)?;

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
