//! `orb validate` — native Rust schema validation (text + JSON output).

use crate::utils::{
    detect_language, detect_syntax, load_extra_patterns, load_services_registry, merge_post_inline_diagnostics,
    parse_schema_auto_typed_with_map, parse_schema_auto_with_map, typed_parse_error_to_json,
};
use crate::{SourcePosition, ValidationJsonError, ValidationJsonOutput, ValidationJsonWarning, ValidationStats};
use std::path::PathBuf;

/// `.lolo` source facts the lowered schema cannot carry. The pre-bracket app
/// header (a positional version, description/theme/locales/public on loose
/// lines) lowers to the same `.orb`, so only the source can report it.
pub(crate) fn source_findings(content: &str) -> Vec<Finding> {
    if !matches!(detect_syntax(content), crate::utils::InputSyntax::Lolo) {
        return Vec::new();
    }
    let Ok(program) = orbital_lolo::parse_str(content) else {
        return Vec::new();
    };
    if !program.app.legacy_lines {
        return Vec::new();
    }
    let path = format!("app:{}", program.app.span.line);
    let message = "the app header writes its modifiers outside its bracket (a positional version, or description/theme/locales/public on loose lines); declare them in the bracket: `app NAME [ version: \"1.0.0\", description: …, theme: …, locales: […], public: … ]`".to_string();
    vec![Finding {
        code: "ORB_L_APP_HEADER_LEGACY".to_string(),
        text: format!("[ORB_L_APP_HEADER_LEGACY] {path}: {message}"),
        path,
        message,
        suggestion: Some("`orb format --write <file>` rewrites the header into the bracket form".to_string()),
    }]
}

/// Validate schema using native Rust validation
pub(crate) fn validate_schema(
    path: &PathBuf,
    json_output: bool,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
    deny_warnings: bool,
) -> anyhow::Result<()> {
    // Load extra patterns (explicit path or auto-discover)
    let extra_patterns = load_extra_patterns(path, patterns_path);

    // Load services registry (explicit path or auto-discover)
    let service_registry = load_services_registry(services_path);

    if json_output {
        return validate_schema_json(path, extra_patterns, service_registry, deny_warnings);
    }

    println!("Validating schema: {}", path.display());

    // Use native Rust validation only
    validate_schema_native(path, extra_patterns, service_registry, deny_warnings)
}

/// The one pass rule: no errors, and no warnings either when `--deny-warnings` holds the 0/0 bar.
pub(crate) fn validation_passes(errors: usize, warnings: usize, deny_warnings: bool) -> bool {
    errors == 0 && (!deny_warnings || warnings == 0)
}

/// The JSON error `code` for a behavior-package `discover()` failure —
/// checked before a schema is even parsed, since `CompilerOptions::from_env`
/// runs discovery unconditionally. `PrefixConflict` gets its own `ORB_BEHAVIOR_*`
/// code; the other `DiscoverError` variants (a malformed/incomplete installed
/// package) share one generic code — none of the four behavior-lookup codes
/// (`ORB_BEHAVIOR_{PACKAGE_NOT_INSTALLED,NOT_FOUND,AMBIGUOUS}`) applies to a
/// discovery-time failure, only to a later `uses` resolution.
pub(crate) fn discovery_error_json_code(
    e: &orbital_core::behavior_packages::DiscoverError,
) -> &'static str {
    use orbital_core::behavior_packages::DiscoverError;
    match e {
        DiscoverError::PrefixConflict { .. } => "ORB_BEHAVIOR_PREFIX_CONFLICT",
        DiscoverError::MissingDeclaredPath { .. }
        | DiscoverError::InvalidPackageJson { .. }
        | DiscoverError::Io { .. } => "ORB_BEHAVIOR_PACKAGE_INVALID",
    }
}

/// True when an inline-phase error names a real, resolvable-shaped `uses`
/// import that nothing on the `discover()` walk answers
/// (`ORB_BEHAVIOR_PACKAGE_NOT_INSTALLED` / `ORB_BEHAVIOR_NOT_FOUND`) — as
/// opposed to a broken loader environment (`BehaviorAmbiguous`/
/// `PrefixConflict`/`StdLibNotConfigured`/…). Drives
/// `ValidationJsonError.needs_install` and the "install this" vs.
/// "environment is broken" suggestion text. `BehaviorPackageNotInstalled`
/// (Phase 2: a missing `@almadar/std` — `PackageNotInstalled` covers a
/// missing `<prefix>/<name>` scoped package) is install-actionable the same
/// way.
pub(crate) fn inline_error_needs_install(
    e: &orbital_compiler::phases::inline::InlineError,
) -> bool {
    use orbital_compiler::loader::LoaderError;
    use orbital_compiler::phases::inline::InlineError;
    matches!(
        e,
        InlineError::Loader(LoaderError::PackageNotInstalled { .. })
            | InlineError::Loader(LoaderError::BehaviorPackageNotInstalled { .. })
            | InlineError::Loader(LoaderError::BehaviorNotFound { .. })
    )
}

/// Validate schema with JSON output (for programmatic consumption by DeepAgent)
fn validate_schema_json(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    deny_warnings: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::phases::inline::inline_schema_with_orbital_diagnostics;
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    // Behavior-package discovery runs unconditionally in `from_env` — a
    // `PrefixConflict` (or a malformed installed package) is validator
    // infrastructure, never a schema authoring error, so it is reported
    // before the schema is even read.
    let discovery_probe = CompilerOptions::from_env();
    if let Some(discovery_err) = &discovery_probe.discovery_error {
        let output = ValidationJsonOutput {
            success: true,
            valid: false,
            errors: Some(vec![ValidationJsonError {
                code: discovery_error_json_code(discovery_err).to_string(),
                path: "".to_string(),
                message: discovery_err.to_string(),
                suggestion: Some(
                    "Validator environment error — the schema was NOT evaluated; retry, do not edit the schema"
                        .to_string(),
                ),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                position: None,
                needs_install: None,
            }]),
            warnings: None,
            stats: None,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

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
                    position: None,
                    needs_install: None,
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

    let json_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => orbital_compiler::SchemaLanguage::NonLatin(lang),
        _ => orbital_compiler::SchemaLanguage::English,
    };
    let (lazy_errors, lazy_warnings) = validate_lazy_children(
        &mut schema,
        &json_language,
        path.as_path(),
        &extra_patterns,
        &service_registry,
        &discovery_probe,
    );
    let lazy_errors: Vec<ValidationJsonError> = lazy_errors
        .into_iter()
        .chain(program_findings(&schema, path.as_path()))
        .map(finding_json_error)
        .collect();
    let lazy_warnings: Vec<ValidationJsonWarning> = lazy_warnings
        .into_iter()
        .map(|f| ValidationJsonWarning {
            code: f.code,
            path: f.path,
            message: f.message,
            suggestion: f.suggestion,
            position: None,
        })
        .collect();

    // Run the inline phase BEFORE validation. Composed .orb files carry
    // `uses:` imports and `ref:` strings; without inlining, the validator
    // reports every imported trait/page/entity as "undefined". The inline
    // phase resolves external refs via the embedded std loader and
    // filesystem — same path `orb compile` uses. Validation-only mode used
    // to skip this and produced false positives on every multi-use schema.
    let mut trait_ref_config_diagnostics = Vec::new();
    let mut orbital_ref_diagnostics = Vec::new();
    let mut uses_config_diagnostics = Vec::new();
    // from_env, NOT default: the plain-text validate path resolves imports
    // through installed behavior packages while this --json path used
    // InlineConfig::default() and silently inlined the embedded std copy of
    // any behavior an installed package overrides — rabit's
    // validateWithOrbitalCLI (--json) and a manual `orb validate` then
    // disagreed about the same schema.
    // Reuse the already-run discovery (checked for `discovery_error` above)
    // instead of re-walking `node_modules` a second time.
    let inline_config = discovery_probe.inline;
    if let Err(e) = inline_schema_with_orbital_diagnostics(
        &mut schema,
        &inline_config,
        Some(path.as_path()),
        &mut trait_ref_config_diagnostics,
        &mut orbital_ref_diagnostics,
        &mut uses_config_diagnostics,
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
        // `BehaviorAmbiguous`/`PrefixConflict` are installed-package-set
        // problems (two packages both answer the same lookup) — validator
        // infrastructure, same bucket as an unresolvable std/scoped path.
        // `PackageNotInstalled`/`BehaviorNotFound` are the opposite: the
        // schema named a real, resolvable-shaped import that nothing on the
        // walk answers — `needs_install`, never `is_infra` (do NOT collapse
        // its code to `INLINE_INFRA_ERROR`; keep `e.structured_code()` so the
        // caller can tell "install this" from "environment is broken").
        let is_infra = matches!(
            &e,
            InlineError::Loader(LoaderError::StdLibNotConfigured)
                | InlineError::Loader(LoaderError::ScopedPackageNotConfigured(_))
                | InlineError::Loader(LoaderError::BehaviorAmbiguous { .. })
                | InlineError::Loader(LoaderError::PrefixConflict { .. })
        );
        let needs_install = inline_error_needs_install(&e);
        let mut inline_message = format!("Failed to resolve external references: {}", e);
        if is_infra {
            let lc = &inline_config.loader_config;
            inline_message.push_str(&format!(
                " [loader env: installed_packages={}, std_lib_path_set={}]",
                lc.packages.len(),
                lc.std_lib_path.is_some(),
            ));
        }
        // Import-carrying loader errors have no spans; the failing import is
        // located at the `uses` line that declares it, matched exactly.
        let import_position = match &e {
            InlineError::Loader(LoaderError::InvalidPrimitiveImport(s)) => Some(s.as_str()),
            InlineError::Loader(LoaderError::StdImportNotCanonical { given, .. }) => Some(given.as_str()),
            InlineError::Loader(LoaderError::BehaviorNotFound { name, .. }) => Some(name.as_str()),
            InlineError::Loader(LoaderError::PackageNotInstalled { prefix }) => Some(prefix.as_str()),
            _ => None,
        }
        .and_then(|import| source_map.as_ref()?.import_span(import))
        .map(|span| SourcePosition::of(span, false));
        let mut inline_errors = lazy_errors;
        inline_errors.push(ValidationJsonError {
                code: if is_infra {
                    "INLINE_INFRA_ERROR".to_string()
                } else {
                    e.structured_code().to_string()
                },
                path: inline_path,
                message: inline_message,
                suggestion: Some(if is_infra {
                    "Validator environment error — the schema was NOT evaluated; retry, do not edit the schema"
                        .to_string()
                } else if needs_install {
                    "Run `orb behaviors install <package>` for the package that declares this \
                     behavior, then retry — the schema was NOT evaluated."
                        .to_string()
                } else {
                    "Check `uses:` imports resolve to known behaviors".to_string()
                }),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                position: import_position,
                needs_install: needs_install.then_some(true),
            });
        let output = ValidationJsonOutput {
            success: true,
            valid: false,
            errors: Some(inline_errors),
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
    let mut comprehensive_result = validate_comprehensive_full(
        &schema,
        extra_patterns,
        json_language,
        service_registry,
        inline_config.loader_config.packages.clone(),
    );
    merge_post_inline_diagnostics(
        &mut comprehensive_result,
        &trait_ref_config_diagnostics,
        &orbital_ref_diagnostics,
        &uses_config_diagnostics,
        &schema,
    );

    let mut all_errors: Vec<ValidationJsonError> = lazy_errors;
    let mut all_warnings: Vec<ValidationJsonWarning> = lazy_warnings;
    all_warnings.extend(source_findings(&content_raw).into_iter().map(|f| ValidationJsonWarning {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        position: None,
    }));

    // Convert comprehensive validation errors. When the schema came from
    // `.lolo`, `source_map` resolves `error.path` (an L2 JSON-pointer-style
    // path) back to the originating `.lolo` line:col — best-effort (see
    // `LoloSourceMap::lookup`'s longest-prefix fallback); `.orb` JSON input
    // carries no map, so line/column stay `None` there.
    for error in &comprehensive_result.errors {
        let resolved = source_map
            .as_ref()
            .and_then(|m| m.lookup_detailed(&error.path));
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
            position: resolved.map(|(span, exact)| SourcePosition::of(span, exact)),
            needs_install: None,
        });
    }

    for warning in &comprehensive_result.warnings {
        let resolved = source_map
            .as_ref()
            .and_then(|m| m.lookup_detailed(&warning.path));
        all_warnings.push(ValidationJsonWarning {
            code: warning.code.to_string(),
            path: warning.path.clone(),
            message: warning.message.clone(),
            suggestion: warning.suggestion.clone(),
            position: resolved.map(|(span, exact)| SourcePosition::of(span, exact)),
        });
    }

    // Step 3: Try to compile to OIR for additional validation.
    // The `Err` arm must surface — the plain-text path (`run_full_validation`)
    // pushes `Compilation error: {e}` into its errors, so an `if let Ok`
    // here silently dropped compile/resolve failures and let `--json` report
    // `valid:true` on a schema plain `orb validate` rejects (same class of
    // --json/plain divergence the inline `from_env` note above documents).
    // One reporter: when validation already reported errors the compile step
    // can only restate the same root cause as `COMPILATION_ERROR`, so it runs
    // only on a schema validation accepted.
    // Already inlined above WITH the source path; re-inlining here would
    // resolve relative `uses` against the cwd instead of the file's folder.
    let mut options = CompilerOptions::from_env();
    options.inline.skip_inline = true;
    let compile_result = if all_errors.is_empty() {
        Some(compile_schema(schema, &options))
    } else {
        None
    };
    match compile_result {
        None => {}
        Some(Ok(module)) => {
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
                        position: None,
                        needs_install: None,
                    });
                } else {
                    all_warnings.push(ValidationJsonWarning {
                        code: "OIR_WARNING".to_string(),
                        path: error.path.clone(),
                        message: error.message.clone(),
                        suggestion: None,
                        position: None,
                    });
                }
            }
        }
        Some(Err(e)) => {
            all_errors.push(ValidationJsonError {
                code: "COMPILATION_ERROR".to_string(),
                path: "".to_string(),
                message: format!("Compilation error: {e}"),
                suggestion: None,
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                position: None,
                needs_install: None,
            });
        }
    }

    let output = ValidationJsonOutput {
        success: true,
        valid: validation_passes(all_errors.len(), all_warnings.len(), deny_warnings),
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

/// `locales/<locale>/<app>.json` beside `path`, one entry per locale folder
/// plus every declared locale (absent file → `None`).
fn load_catalogs(
    path: &std::path::Path,
    name: &str,
    declared: &[String],
) -> std::collections::BTreeMap<String, Result<Option<serde_json::Value>, String>> {
    let dir = path.parent().unwrap_or(std::path::Path::new(".")).join("locales");
    let mut locales: std::collections::BTreeSet<String> = declared.iter().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().join(format!("{name}.json")).is_file() {
                locales.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    locales
        .into_iter()
        .map(|locale| {
            let file = dir.join(&locale).join(format!("{name}.json"));
            let parsed = if file.is_file() {
                std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
                    .map(Some)
            } else {
                Ok(None)
            };
            (locale, parsed)
        })
        .collect()
}

/// Checks only the whole top-level program can make (after its `uses lazy`
/// pages are recorded): its catalogs and its sign-in route. Both the text and
/// `--json` paths report them.
fn program_findings(
    schema: &orbital_core::schema::OrbitalSchema,
    path: &std::path::Path,
) -> Vec<Finding> {
    let mut findings = catalog_findings(schema, path);
    for e in orbital_compiler::phases::validation::page::validate_sign_in_route(schema).errors {
        let code = e.code.as_str().to_string();
        findings.push(Finding {
            text: format!("[{code}] {}: {}", e.path, e.message),
            code,
            path: e.path,
            message: e.message,
            suggestion: e.suggestion,
        });
    }
    findings
}

/// The `ORB_I18N_*` rules over a program's own `i18n/t` calls and its catalogs.
fn catalog_findings(schema: &orbital_core::schema::OrbitalSchema, path: &std::path::Path) -> Vec<Finding> {
    use orbital_core::messages::{check_catalogs, CatalogIssue};
    let declared = schema.locales.clone().unwrap_or_default();
    let mut findings = Vec::new();
    let mut catalogs = std::collections::BTreeMap::new();
    let mut issues = Vec::new();
    for (locale, loaded) in load_catalogs(path, &schema.name, &declared) {
        match loaded {
            Ok(value) => {
                catalogs.insert(locale, value);
            }
            Err(detail) => issues.push(CatalogIssue::CatalogInvalid { locale, detail }),
        }
    }
    let Ok(program) = serde_json::to_value(schema) else {
        return findings;
    };
    issues.extend(check_catalogs(&program, &schema.name, schema.locales.as_deref(), &catalogs));
    for issue in issues {
        let message = issue.message();
        findings.push(Finding {
            code: issue.code().to_string(),
            path: "locales".to_string(),
            text: format!("[{}] locales: {}", issue.code(), message),
            message,
            suggestion: None,
        });
    }
    findings
}

fn finding_json_error(f: Finding) -> ValidationJsonError {
    ValidationJsonError {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        fix_guidance: None,
        expected_shape: None,
        valid_values: None,
        position: None,
        needs_install: None,
    }
}

/// One validation finding. `text` is the exact human line `orb validate`
/// prints; the structured fields serve `--json` and lazy-child prefixing.
pub(crate) struct Finding {
    pub code: String,
    pub path: String,
    pub message: String,
    pub suggestion: Option<String>,
    pub text: String,
}

/// What [`validate_schema_core`] found, plus the inlined schema when the
/// inline phase succeeded (a lazy child's resolved pages come from it).
pub(crate) struct CoreValidation {
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
    pub inlined: Option<orbital_core::schema::OrbitalSchema>,
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
    use orbital_compiler::{CompilerOptions, SchemaLanguage};

    // Behavior-package discovery runs unconditionally in `from_env` — a
    // `PrefixConflict` (or a malformed installed package) is validator
    // infrastructure, checked before the schema is even read.
    let default_opts = CompilerOptions::from_env();
    if let Some(discovery_err) = &default_opts.discovery_error {
        return Ok(ValidationDiagnostics {
            errors: vec![format!(
                "[{}] {} (validator environment error — the schema was NOT evaluated; retry, do not edit the schema)",
                discovery_error_json_code(discovery_err),
                discovery_err
            )],
            warnings: Vec::new(),
        });
    }

    let content_raw = std::fs::read_to_string(path)?;
    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => SchemaLanguage::NonLatin(lang),
        _ => SchemaLanguage::English,
    };
    let (schema, source_map) =
        parse_schema_auto_with_map(&content_raw, &path.display().to_string())?;

    let core = validate_schema_core(
        schema,
        true,
        source_map.as_ref(),
        schema_language,
        path,
        extra_patterns,
        service_registry,
        &default_opts,
    );
    Ok(ValidationDiagnostics {
        errors: core.errors.into_iter().map(|f| f.text).collect(),
        warnings: source_findings(&content_raw)
            .into_iter()
            .chain(core.warnings)
            .map(|f| f.text)
            .collect(),
    })
}

/// Validates every `uses lazy` behavior split out of `schema` with the same
/// core, records its pages on `schema`, and returns its findings with paths
/// prefixed `lazy[<Orbital>]`.
pub(crate) fn validate_lazy_children(
    schema: &mut orbital_core::schema::OrbitalSchema,
    schema_language: &orbital_compiler::SchemaLanguage,
    path: &std::path::Path,
    extra_patterns: &Option<orbital_compiler::ExtraPatterns>,
    service_registry: &orbital_compiler::embedded::ServiceRegistryData,
    options: &orbital_compiler::CompilerOptions,
) -> (Vec<Finding>, Vec<Finding>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for artifact in orbital_compiler::phases::lazy::split_lazy(schema) {
        let core = validate_schema_core(
            artifact.schema.clone(),
            false,
            None,
            schema_language.clone(),
            path,
            extra_patterns.clone(),
            service_registry.clone(),
            options,
        );
        let prefix = |f: Finding| {
            let path = format!("lazy[{}].{}", artifact.orbital, f.path);
            Finding {
                text: format!("[{}] {}: {}", f.code, path, f.message),
                path,
                ..f
            }
        };
        errors.extend(core.errors.into_iter().map(prefix));
        warnings.extend(core.warnings.into_iter().map(prefix));
        if let Some(inlined) = &core.inlined {
            if let Err(e) = orbital_compiler::phases::lazy::record_lazy_pages(schema, &artifact, inlined) {
                errors.push(Finding {
                    code: e.structured_code().to_string(),
                    path: format!("lazy[{}]", artifact.orbital),
                    message: e.to_string(),
                    suggestion: None,
                    text: format!("[{}] lazy[{}]: {}", e.structured_code(), artifact.orbital, e),
                });
            }
        }
    }
    (errors, warnings)
}

/// The validation pipeline on an already-parsed schema: lazy children,
/// inline, enrich, `validate_comprehensive_full`, compile to OIR,
/// `OirValidator`.
pub(crate) fn validate_schema_core(
    mut schema: orbital_core::schema::OrbitalSchema,
    check_own_catalogs: bool,
    source_map: Option<&orbital_lolo::source_map::LoloSourceMap>,
    schema_language: orbital_compiler::SchemaLanguage,
    path: &std::path::Path,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    default_opts: &orbital_compiler::CompilerOptions,
) -> CoreValidation {
    use orbital_compiler::phases::enrich_schema;
    use orbital_compiler::phases::inline::inline_schema_with_orbital_diagnostics;
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    let (mut errors, mut warnings) = validate_lazy_children(
        &mut schema,
        &schema_language,
        path,
        &extra_patterns,
        &service_registry,
        default_opts,
    );
    if check_own_catalogs {
        errors.extend(program_findings(&schema, path));
        if let Some(public) = &schema.public_dir {
            if let Err(message) = orbital_compiler::phases::public::resolve_public_dir(path, public) {
                errors.push(Finding {
                    code: "ORB_PUBLIC_DIR_INVALID".to_string(),
                    path: "publicDir".to_string(),
                    text: format!("[ORB_PUBLIC_DIR_INVALID] publicDir: {message}"),
                    message,
                    suggestion: None,
                });
            }
        }
    }

    // Inline + enrich before validation so `uses`-based trait/entity/page refs
    // resolve (otherwise validate_comprehensive_full false-positives on every
    // multi-use schema). Mirrors the `orb validate` path.
    let mut trait_ref_config_diagnostics = Vec::new();
    let mut orbital_ref_diagnostics = Vec::new();
    let mut uses_config_diagnostics = Vec::new();
    // An inline failure is FATAL for this run: validators index Inline traits
    // only, so validating the half-inlined schema manufactures spurious
    // ORB_T_UNDEFINED_TRAIT / ORB_X_LISTEN_SOURCE_UNRESOLVED findings that
    // bury the real cause. Report the one true error and stop.
    if let Err(e) = inline_schema_with_orbital_diagnostics(
        &mut schema,
        &default_opts.inline,
        Some(path),
        &mut trait_ref_config_diagnostics,
        &mut orbital_ref_diagnostics,
        &mut uses_config_diagnostics,
    ) {
        let path_part = match e.structured_path() {
            "" => String::new(),
            p => format!(" {p}:"),
        };
        errors.push(Finding {
            code: e.structured_code().to_string(),
            path: e.structured_path().to_string(),
            message: e.to_string(),
            suggestion: None,
            text: format!(
                "[{}]{} {} (schema not fully resolved — downstream validation suppressed until this is fixed)",
                e.structured_code(),
                path_part,
                e
            ),
        });
        return CoreValidation { errors, warnings, inlined: None };
    }
    enrich_schema(&mut schema, &default_opts.enrichment);

    if let Ok(program) = serde_json::to_value(&schema) {
        for path in orbital_core::messages::render_only_violations(&program) {
            let message = "`i18n/t` and `@locale` are render-time — use them only inside `render-ui`".to_string();
            errors.push(Finding {
                code: "ORB_I18N_RENDER_ONLY".to_string(),
                text: format!("[ORB_I18N_RENDER_ONLY] {path}: {message}"),
                path,
                message,
                suggestion: None,
            });
        }
    }

    let mut comprehensive_result = validate_comprehensive_full(
        &schema,
        extra_patterns,
        schema_language,
        service_registry,
        default_opts.inline.loader_config.packages.clone(),
    );
    merge_post_inline_diagnostics(
        &mut comprehensive_result,
        &trait_ref_config_diagnostics,
        &orbital_ref_diagnostics,
        &uses_config_diagnostics,
        &schema,
    );

    // Human path: append ` @ line:col` when the L2 path resolves to a
    // `.lolo` span (`source_map` is `None` for `.orb` JSON input, and a miss
    // just leaves the message as-is — same degradations as the `--json` path).
    let human = |code: &str, path: &str, message: &str, suggestion: &Option<String>| {
        let mut s = format!("[{}] {}: {}", code, path, message);
        if let Some(span) = source_map.and_then(|m| m.lookup(path)) {
            s.push_str(&format!(" @ {}:{}", span.line, span.col));
        }
        if let Some(sug) = suggestion {
            s.push_str(&format!(" \u{1f4a1} {sug}"));
        }
        s
    };
    errors.extend(comprehensive_result.errors.iter().map(|e| Finding {
        code: e.code.to_string(),
        path: e.path.clone(),
        message: e.message.clone(),
        suggestion: e.suggestion.clone(),
        text: human(&e.code.to_string(), &e.path, &e.message, &e.suggestion),
    }));
    warnings.extend(comprehensive_result.warnings.iter().map(|w| Finding {
        code: w.code.to_string(),
        path: w.path.clone(),
        message: w.message.clone(),
        suggestion: w.suggestion.clone(),
        text: human(&w.code.to_string(), &w.path, &w.message, &w.suggestion),
    }));

    // Compile to OIR (schema already inlined+enriched above -> skip re-inline).
    let inlined = schema.clone();
    let mut options = CompilerOptions::from_env();
    options.inline.skip_inline = true;
    let compile_result = if errors.is_empty() {
        Some(compile_schema(schema, &options))
    } else {
        None
    };
    match compile_result {
        None => {}
        Some(Ok(module)) => {
            let oir = OirValidator::new(&module).validate();
            for e in &oir.errors {
                let finding = Finding {
                    code: if e.severity == ValidationSeverity::Error { "OIR_ERROR" } else { "OIR_WARNING" }.to_string(),
                    path: e.path.clone(),
                    message: e.message.clone(),
                    suggestion: None,
                    text: format!("OIR {e}"),
                };
                if e.severity == ValidationSeverity::Error {
                    errors.push(finding);
                } else {
                    warnings.push(finding);
                }
            }
            // Phase 4 (C-GUARD-NO-BOOLEAN-CHECK): warn on guards that resolve
            // to a definitively non-boolean type (arithmetic / literals /
            // number-returning operator calls) — they run with JS truthiness.
            for w in orbital_compiler::type_infer::collect_guard_type_warnings(&module) {
                warnings.push(Finding {
                    code: String::new(),
                    path: String::new(),
                    message: w.clone(),
                    suggestion: None,
                    text: w,
                });
            }
        }
        Some(Err(e)) => errors.push(Finding {
            code: "COMPILATION_ERROR".to_string(),
            path: String::new(),
            message: format!("Compilation error: {e}"),
            suggestion: None,
            text: format!("Compilation error: {e}"),
        }),
    }

    CoreValidation { errors, warnings, inlined: Some(inlined) }
}

/// Native Rust schema validation (human text output). Thin formatting layer
/// over `run_full_validation` so `orb validate` and `orb verify` share one
/// validation code path and can never diverge.
fn validate_schema_native(
    path: &PathBuf,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    deny_warnings: bool,
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
    if !validation_passes(diags.errors.len(), diags.warnings.len(), deny_warnings) {
        anyhow::bail!(
            "Schema validation failed: {} warning(s) under --deny-warnings",
            diags.warnings.len()
        );
    }

    Ok(())
}

#[cfg(test)]
mod deny_warnings_tests {
    use super::validation_passes;

    #[test]
    fn warnings_fail_only_under_deny_warnings() {
        assert!(validation_passes(0, 0, false));
        assert!(validation_passes(0, 0, true));
        assert!(
            validation_passes(0, 3, false),
            "warnings alone pass by default"
        );
        assert!(
            !validation_passes(0, 1, true),
            "one warning fails the 0/0 bar"
        );
    }

    #[test]
    fn errors_fail_either_way() {
        assert!(!validation_passes(1, 0, false));
        assert!(!validation_passes(1, 0, true));
    }
}

#[cfg(test)]
mod needs_install_classification_tests {
    use super::inline_error_needs_install;
    use orbital_compiler::loader::LoaderError;
    use orbital_compiler::phases::inline::InlineError;

    #[test]
    fn package_not_installed_and_behavior_not_found_need_install() {
        assert!(inline_error_needs_install(&InlineError::Loader(
            LoaderError::PackageNotInstalled {
                prefix: "almadar-behaviors".to_string(),
            }
        )));
        assert!(inline_error_needs_install(&InlineError::Loader(
            LoaderError::BehaviorNotFound {
                name: "std-notes".to_string(),
                searched: vec![],
            }
        )));
        // Phase 2: a missing `@almadar/std` itself (as opposed to a missing
        // `<prefix>/<name>` scoped package — `PackageNotInstalled` above) is
        // install-actionable the same way.
        assert!(inline_error_needs_install(&InlineError::Loader(
            LoaderError::BehaviorPackageNotInstalled {
                package: "@almadar/std".to_string(),
            }
        )));
    }

    #[test]
    fn a_broken_loader_environment_does_not_need_install() {
        assert!(!inline_error_needs_install(&InlineError::Loader(
            LoaderError::StdLibNotConfigured
        )));
        assert!(!inline_error_needs_install(&InlineError::Loader(
            LoaderError::PrefixConflict {
                prefix: "almadar-behaviors".to_string(),
                package_a: "@almadar-io/behaviors".to_string(),
                path_a: std::path::PathBuf::new(),
                package_b: "@fixture/pkg".to_string(),
                path_b: std::path::PathBuf::new(),
            }
        )));
    }
}

#[cfg(test)]
mod legacy_header_tests {
    use super::source_findings;

    const BODY: &str = "\norbital Foo {\n  entity Bar {\n    id : string!\n  }\n}\n";

    #[test]
    fn a_loose_line_header_is_reported_once() {
        let found = source_findings(&format!("app Foo \"1.0.0\"\n\"Desc\"\ntheme \"orb-light\"\n{BODY}"));
        assert_eq!(found.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(), vec!["ORB_L_APP_HEADER_LEGACY"]);
        assert_eq!(found[0].path, "app:1");
    }

    #[test]
    fn control_the_bracket_header_is_clean() {
        assert!(source_findings(&format!("app Foo [version: \"1.0.0\", description: \"Desc\", theme: \"orb-light\"]\n{BODY}")).is_empty());
        assert!(source_findings(&format!("app Foo\n{BODY}")).is_empty());
    }

    #[test]
    fn a_positional_version_alone_is_reported() {
        let found = source_findings(&format!("app Foo \"1.0.0\"\n{BODY}"));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn control_a_headerless_file_and_orb_json_are_clean() {
        assert!(source_findings(BODY).is_empty());
        assert!(source_findings("{\"name\":\"Foo\",\"orbitals\":[]}").is_empty());
    }
}

#[cfg(test)]
mod program_findings_tests {
    use super::program_findings;

    fn schema(site: serde_json::Value, path: &str) -> orbital_core::schema::OrbitalSchema {
        serde_json::from_value(serde_json::json!({
            "name": "app",
            "site": site,
            "orbitals": [{
                "name": "Main",
                "entity": { "name": "Item", "persistence": "runtime", "fields": [{ "name": "id", "type": "string" }] },
                "traits": [],
                "pages": [{ "name": "P", "path": path, "traits": [] }]
            }]
        }))
        .expect("schema")
    }

    fn codes(s: &orbital_core::schema::OrbitalSchema) -> Vec<String> {
        program_findings(s, std::path::Path::new("/nonexistent/app.lolo"))
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    #[test]
    fn a_page_on_the_hosts_sign_in_route_is_a_program_finding() {
        assert_eq!(
            codes(&schema(serde_json::json!({}), "/login")),
            vec!["ORB_P_HOST_RESERVED_ROUTE"]
        );
    }

    #[test]
    fn an_unknown_sign_in_route_is_a_program_finding() {
        assert_eq!(
            codes(&schema(serde_json::json!({ "signIn": "/nope" }), "/")),
            vec!["ORB_P_SIGN_IN_UNKNOWN_ROUTE"]
        );
    }

    #[test]
    fn control_the_hosts_sign_in_route_needs_no_page() {
        assert!(codes(&schema(serde_json::json!({ "signIn": "/login" }), "/")).is_empty());
    }
}
