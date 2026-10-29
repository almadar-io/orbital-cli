//! `orb validate` — native Rust schema validation (text + JSON output).

use crate::utils::{
    detect_language, detect_syntax, load_extra_patterns, load_services_registry,
    merge_post_inline_diagnostics, parse_schema_auto_typed_with_map,
    typed_parse_error_to_json,
};
use crate::{
    SourcePosition, ValidationJsonError, ValidationJsonOutput, ValidationJsonWarning,
    ValidationStats,
};
use orbital_core::logging::ns;

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
    vec![Finding::new(
        "ORB_L_APP_HEADER_LEGACY".to_string(),
        path,
        message,
        Some("`orb format --write <file>` rewrites the header into the bracket form".to_string()),
    )]
}

/// `orb validate --json`'s report as a value — what the CLI prints and `orb mcp` returns: the one [`Report`] rendered
/// as JSON rows.
pub(crate) fn validation_report(
    path: &std::path::Path,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
    deny_warnings: bool,
    final_program: bool,
    findings: &[ProgramFinding],
) -> anyhow::Result<ValidationJsonOutput> {
    let report = if let Some(refused) = language_data_finding() {
        Report::refused(refused)
    } else {
        let extra_patterns = load_extra_patterns(path, patterns_path);
        let service_registry = load_services_registry(services_path)?;
        findings_report(path, None, extra_patterns, service_registry, final_program, findings)
    };
    Ok(report.into_json(deny_warnings))
}

/// The finding for a missing/incompatible installed language data package, when there is one.
fn language_data_finding() -> Option<Finding> {
    let error = tracing::info_span!(target: ns::CLI, "language data")
        .in_scope(crate::utils::patterns::ensure_language_ready)
        .err()?;
    Some(Finding::from_json_error(language_data_json_error(&error)))
}

/// The JSON error for a missing/incompatible installed `@almadar/core` / `@almadar/std`
/// (language data) — validator infrastructure, never a schema authoring error.
pub(crate) fn language_data_json_error(
    e: &orbital_core::language_data::LanguageDataError,
) -> ValidationJsonError {
    let suggestion = match e {
        orbital_core::language_data::LanguageDataError::NotInstalled { package } => format!(
            "Run `{}`, then retry — the schema was NOT evaluated.",
            orbital_core::language_data::install_command(package)
        ),
        _ => "Validator environment error — run `orb update --global`, then retry; the schema was NOT evaluated, do not edit the schema"
            .to_string(),
    };
    ValidationJsonError {
        code: e.code().to_string(),
        path: "".to_string(),
        message: e.to_string(),
        suggestion: Some(suggestion),
        fix_guidance: None,
        expected_shape: None,
        valid_values: None,
        position: None,
        needs_install: e.needs_install().then_some(true),
    }
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
        DiscoverError::DuplicateBehaviorName { .. } => "ORB_BEHAVIOR_NAME_DUPLICATE",
        DiscoverError::PrefixConflict { .. } => "ORB_BEHAVIOR_PREFIX_CONFLICT",
        DiscoverError::IncompatibleCompiler { .. } => orbital_core::language_data::CODE_INCOMPATIBLE,
        DiscoverError::MissingDeclaredPath { .. }
        | DiscoverError::InvalidSidecarLock { .. }
        | DiscoverError::InvalidPackageJson { .. }
        | DiscoverError::Io { .. } => "ORB_BEHAVIOR_PACKAGE_INVALID",
    }
}

#[cfg(test)]
mod duplicate_behavior_diagnostic_tests {
    #[test]
    fn duplicate_behavior_names_have_a_specific_validation_code() {
        let error = orbital_core::behavior_packages::DiscoverError::DuplicateBehaviorName {
            name: "std-calendar".to_string(),
            path_a: "infra/std-calendar.lolo".into(),
            path_b: "ui/std-calendar.lolo".into(),
        };
        assert_eq!(
            super::discovery_error_json_code(&error),
            "ORB_BEHAVIOR_NAME_DUPLICATE"
        );
        assert!(error.to_string().contains("infra/std-calendar.lolo"));
        assert!(error.to_string().contains("ui/std-calendar.lolo"));
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

/// What validating one file found: the findings every renderer (text, JSON, `orb mcp`, `orb compile`'s gate) reads.
pub(crate) struct Report {
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
    pub stats: Option<ValidationStats>,
}

impl Report {
    /// A report whose one finding stopped validation before the schema was evaluated.
    fn refused(finding: Finding) -> Self {
        Self {
            errors: vec![finding],
            warnings: Vec::new(),
            stats: None,
        }
    }

    pub(crate) fn into_json(self, deny_warnings: bool) -> ValidationJsonOutput {
        let errors: Vec<ValidationJsonError> =
            self.errors.into_iter().map(finding_json_error).collect();
        let warnings: Vec<ValidationJsonWarning> =
            self.warnings.into_iter().map(finding_json_warning).collect();
        ValidationJsonOutput {
            success: true,
            valid: validation_passes(errors.len(), warnings.len(), deny_warnings),
            errors: (!errors.is_empty()).then_some(errors),
            warnings: (!warnings.is_empty()).then_some(warnings),
            stats: self.stats,
        }
    }

    pub(crate) fn into_text(self) -> ValidationDiagnostics {
        ValidationDiagnostics {
            errors: self.errors.iter().map(Finding::render).collect(),
            warnings: self.warnings.iter().map(Finding::render).collect(),
        }
    }
}

/// The one producer of a file's findings: discovery, read, parse, then [`validate_schema_core`]. Never bails on a
/// finding; a failure that stops validation is the report's one error.
pub(crate) fn findings_report(
    path: &std::path::Path,
    resources_beside: Option<&std::path::Path>,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    final_program: bool,
    program_findings: &[ProgramFinding],
) -> Report {
    use orbital_compiler::CompilerOptions;

    let discovery_probe = tracing::info_span!(target: ns::CLI, "discover packages").in_scope(CompilerOptions::from_env);
    if let Some(discovery_err) = &discovery_probe.discovery_error {
        return Report::refused(Finding {
            code: discovery_error_json_code(discovery_err).to_string(),
            message: discovery_err.to_string(),
            suggestion: Some(
                "Validator environment error — the schema was NOT evaluated; retry, do not edit the schema"
                    .to_string(),
            ),
            ..Finding::default()
        });
    }

    let content_raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return Report::refused(Finding {
                code: "FILE_READ_ERROR".to_string(),
                message: format!("Failed to read schema file: {e}"),
                ..Finding::default()
            })
        }
    };

    let (schema, source_map) = match tracing::info_span!(target: ns::CLI, "lower")
        .in_scope(|| parse_schema_auto_typed_with_map(&content_raw, &path.display().to_string()))
    {
        Ok(parsed) => parsed,
        Err(e) => {
            return Report {
                errors: typed_parse_error_to_json(&e, Some(&content_raw))
                    .into_iter()
                    .map(Finding::from_json_error)
                    .collect(),
                warnings: Vec::new(),
                stats: None,
            }
        }
    };

    let schema_language = match detect_language(&content_raw) {
        Some(lang) if lang != "en" => orbital_compiler::SchemaLanguage::NonLatin(lang),
        _ => orbital_compiler::SchemaLanguage::English,
    };

    let core = validate_schema_core(
        schema,
        Some(resources_beside.unwrap_or(path)),
        source_map.as_ref(),
        schema_language,
        path,
        extra_patterns,
        service_registry,
        &discovery_probe,
        final_program,
        program_findings.to_vec(),
    );
    let stats = core.inlined.as_ref().map(|inlined| ValidationStats {
        orbitals: inlined.orbitals.len(),
        entities: inlined.orbitals.len(),
        traits: inlined.orbitals.iter().map(|o| o.traits.len()).sum::<usize>(),
        pages: inlined
            .orbitals
            .iter()
            .filter_map(|o| o.pages.as_ref())
            .map(|p| p.len())
            .sum(),
    });
    let (errors, warnings) = reported_findings(core, &content_raw);
    Report {
        errors,
        warnings,
        stats,
    }
}

impl orbital_compiler::phases::validation::orbital_ref_config::CascadeRow for ProgramFinding {
    fn code(&self) -> &str {
        &self.code
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn message(&self) -> &str {
        &self.message
    }
}

/// A finding a std lint program reports for the file, placed in the report beside the compiler's (`program/validate
/// { findings }`).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ProgramFinding {
    pub(crate) code: String,
    pub(crate) path: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) suggestion: String,
    pub(crate) severity: FindingSeverity,
    /// The accepted values, when the finding names a closed set (a field type, an enum member).
    #[serde(default, rename = "validValues")]
    pub(crate) valid_values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FindingSeverity {
    Error,
    Warning,
}

/// A finding's fix guidance, with the approximate-location note when its source span is only the nearest tracked
/// ancestor (an expression spliced in by the inline phase points at the line that references it, C-INLINE-SPLICE-LOCATION).
fn located_guidance(resolved: Option<&(orbital_lolo::Span, bool)>, existing: Option<&str>) -> Option<String> {
    match resolved {
        Some((_, false)) => {
            let note = "Location is approximate (nearest tracked source line). If the shown line only references a config knob or another trait, the failing expression lives where that value is DECLARED (e.g. the trait's config block default) — search the file for the expression named in the message.";
            Some(match existing {
                Some(existing) => format!("{existing} {note}"),
                None => note.to_string(),
            })
        }
        _ => existing.map(str::to_string),
    }
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
    let dir = path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("locales");
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

fn secret_flow_finding(
    violation: &orbital_core::secrets::SecretFlowViolation,
    path: String,
) -> Finding {
    let code = violation.code().to_string();
    let message = violation.message();
    Finding::new(code, path, message, Some(violation.suggestion().to_string()))
}

/// The secret-flow rules over the parsed program, before inline substitutes
/// its `@config.<knob>` reads.
fn own_secret_findings(schema: &orbital_core::schema::OrbitalSchema) -> Vec<Finding> {
    let Ok(program) = serde_json::to_value(schema) else {
        return Vec::new();
    };
    orbital_core::secrets::secret_flow_violations(&program)
        .iter()
        .map(|v| secret_flow_finding(v, v.path().to_string()))
        .collect()
}

/// The secret-flow rules over every behavior file `uses` loaded: an atom types
/// its own knobs, so its program is walked as parsed, not as inlined.
fn used_secret_findings(session: &orbital_compiler::phases::inline::InlineSession) -> Vec<Finding> {
    let mut findings = Vec::new();
    session.each_loaded_schema(&mut |loaded| {
        let Ok(program) = serde_json::to_value(&loaded.schema) else {
            return;
        };
        for v in orbital_core::secrets::secret_flow_violations(&program) {
            findings.push(secret_flow_finding(
                &v,
                format!("{}: {}", loaded.schema.name, v.path()),
            ));
        }
    });
    findings
}

/// The `ORB_I18N_*` rules over a program's own `i18n/t` calls and its catalogs.
fn catalog_findings(
    schema: &orbital_core::schema::OrbitalSchema,
    path: &std::path::Path,
) -> Vec<Finding> {
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
    issues.extend(check_catalogs(
        &program,
        &schema.name,
        schema.locales.as_deref(),
        &catalogs,
    ));
    for issue in issues {
        let message = issue.message();
        findings.push(Finding::new(
            issue.code().to_string(),
            "locales".to_string(),
            message,
            None,
        ));
    }
    findings
}

fn finding_json_error(f: Finding) -> ValidationJsonError {
    ValidationJsonError {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        fix_guidance: f.fix_guidance,
        expected_shape: f.expected_shape,
        valid_values: f.valid_values,
        position: f.position,
        needs_install: f.needs_install,
    }
}

fn finding_json_warning(f: Finding) -> ValidationJsonWarning {
    ValidationJsonWarning {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        position: f.position,
    }
}

/// One validation finding: the one model every renderer reads.
#[derive(Default)]
pub(crate) struct Finding {
    pub code: String,
    pub path: String,
    pub message: String,
    pub suggestion: Option<String>,
    pub fix_guidance: Option<String>,
    pub expected_shape: Option<String>,
    pub valid_values: Option<Vec<String>>,
    pub position: Option<SourcePosition>,
    pub needs_install: Option<bool>,
}

impl Finding {
    fn from_json_error(e: ValidationJsonError) -> Self {
        Self {
            code: e.code,
            path: e.path,
            message: e.message,
            suggestion: e.suggestion,
            fix_guidance: e.fix_guidance,
            expected_shape: e.expected_shape,
            valid_values: e.valid_values,
            position: e.position,
            needs_install: e.needs_install,
        }
    }

    /// The human line: `[code] path: message @ line:col`, then the suggestion and the fix guidance. A finding with no
    /// code is its message alone.
    pub(crate) fn render(&self) -> String {
        if self.code.is_empty() {
            return self.message.clone();
        }
        let mut line = if self.path.is_empty() {
            format!("[{}] {}", self.code, self.message)
        } else {
            format!("[{}] {}: {}", self.code, self.path, self.message)
        };
        if let Some(position) = &self.position {
            line.push_str(&format!(" @ {}:{}", position.line, position.column));
        }
        if let Some(suggestion) = &self.suggestion {
            line.push_str(&format!(" \u{1f4a1} {suggestion}"));
        }
        if let Some(guidance) = &self.fix_guidance {
            line.push_str(&format!(" {guidance}"));
        }
        line
    }

    fn new(code: String, path: String, message: String, suggestion: Option<String>) -> Self {
        Self {
            code,
            path,
            message,
            suggestion,
            ..Self::default()
        }
    }
}

/// A finding at an L2 `path`, placed at its `.lolo` span when `source_map` resolves it (`.orb` JSON input carries no
/// map, and a miss leaves the text as-is).
fn located_finding(
    source_map: Option<&orbital_lolo::source_map::LoloSourceMap>,
    code: String,
    path: String,
    message: String,
    suggestion: Option<String>,
    guidance: Option<&str>,
) -> Finding {
    let resolved = source_map.and_then(|m| m.lookup_detailed(&path));
    Finding {
        fix_guidance: located_guidance(resolved.as_ref(), guidance),
        position: resolved.map(|(span, exact)| SourcePosition::of(span, exact)),
        code,
        path,
        message,
        suggestion,
        ..Finding::default()
    }
}

/// The one finding list both reports read: the pipeline's findings plus the `.lolo` source facts.
fn reported_findings(core: CoreValidation, content: &str) -> (Vec<Finding>, Vec<Finding>) {
    (
        core.errors,
        source_findings(content)
            .into_iter()
            .chain(core.warnings)
            .collect(),
    )
}

/// What [`validate_schema_core`] found, plus the inlined schema when the
/// inline phase succeeded (a lazy child's resolved pages come from it).
pub(crate) struct CoreValidation {
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
    pub inlined: Option<orbital_core::schema::OrbitalSchema>,
}

/// [`findings_report`] rendered as the human lines `orb compile`'s gate prints.
pub(crate) fn run_full_validation(
    path: &std::path::Path,
    resources_beside: Option<&std::path::Path>,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    final_program: bool,
) -> anyhow::Result<ValidationDiagnostics> {
    let report = match language_data_finding() {
        Some(refused) => Report::refused(refused),
        None => findings_report(path, resources_beside, extra_patterns, service_registry, final_program, &[]),
    };
    Ok(report.into_text())
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
            None,
            None,
            schema_language.clone(),
            path,
            extra_patterns.clone(),
            service_registry.clone(),
            options,
            false,
            Vec::new(),
        );
        let prefix = |f: Finding| {
            Finding {
                path: format!("lazy[{}].{}", artifact.orbital, f.path),
                ..f
            }
        };
        errors.extend(core.errors.into_iter().map(prefix));
        warnings.extend(core.warnings.into_iter().map(prefix));
        if let Some(inlined) = &core.inlined {
            if let Err(e) =
                orbital_compiler::phases::lazy::record_lazy_pages(schema, &artifact, inlined)
            {
                errors.push(Finding::new(
                    e.structured_code().to_string(),
                    format!("lazy[{}]", artifact.orbital),
                    e.to_string(),
                    None,
                ));
            }
        }
    }
    (errors, warnings)
}

/// The validation pipeline on an already-parsed schema: lazy children,
/// inline, enrich, `validate_comprehensive_full`, compile to OIR,
/// `OirValidator`. The one producer of findings for every report.
pub(crate) fn validate_schema_core(
    mut schema: orbital_core::schema::OrbitalSchema,
    resources_beside: Option<&std::path::Path>,
    source_map: Option<&orbital_lolo::source_map::LoloSourceMap>,
    schema_language: orbital_compiler::SchemaLanguage,
    path: &std::path::Path,
    extra_patterns: Option<orbital_compiler::ExtraPatterns>,
    service_registry: orbital_compiler::embedded::ServiceRegistryData,
    default_opts: &orbital_compiler::CompilerOptions,
    final_program: bool,
    mut program_findings: Vec<ProgramFinding>,
) -> CoreValidation {
    use orbital_compiler::phases::enrich_schema;
    use orbital_compiler::phases::inline::inline_schema_with_orbital_diagnostics;
    use orbital_compiler::{compile_schema, validate_comprehensive_full, CompilerOptions};
    use orbital_core::ir::{OirValidator, ValidationSeverity};

    let (mut errors, mut warnings) = tracing::info_span!(target: ns::CLI, "lazy children").in_scope(|| validate_lazy_children(
        &mut schema,
        &schema_language,
        path,
        &extra_patterns,
        &service_registry,
        default_opts,
    ));
    errors.extend(own_secret_findings(&schema));
    if let Some(resources) = resources_beside {
        errors.extend(catalog_findings(&schema, resources));
        if let Some(public) = &schema.public_dir {
            if let Err(message) = orbital_compiler::phases::public::resolve_public_dir(resources, public)
            {
                errors.push(Finding::new(
                    "ORB_PUBLIC_DIR_INVALID".to_string(),
                    "publicDir".to_string(),
                    message,
                    None,
                ));
            }
        }
    }

    // Inline + enrich before validation so `uses`-based trait/entity/page refs
    // resolve (otherwise validate_comprehensive_full false-positives on every
    // multi-use schema).
    let mut orbital_ref_diagnostics = Vec::new();
    let mut uses_config_diagnostics = Vec::new();
    // An inline failure is FATAL for this run: validators index Inline traits
    // only, so validating the half-inlined schema manufactures spurious
    // ORB_T_UNDEFINED_TRAIT / ORB_X_LISTEN_SOURCE_UNRESOLVED findings that
    // bury the real cause. Report the one true error and stop.
    let session = orbital_compiler::phases::inline::InlineSession::default();
    let mut inline_opts = default_opts.inline.clone();
    inline_opts.session = Some(session.clone());
    let resolved_input = match orbital_compiler::phases::already_resolved(&schema, Some(path)) {
        Ok(resolved) => resolved,
        Err(e) => {
            errors.push(Finding::new(
                "ORB_RESOLVED_CONTRACT".to_string(),
                String::new(),
                e.to_string(),
                None,
            ));
            return CoreValidation { errors, warnings, inlined: None };
        }
    };
    if !resolved_input { if let Err(e) = tracing::info_span!(target: ns::CLI, "inline").in_scope(|| inline_schema_with_orbital_diagnostics(
        &mut schema,
        &inline_opts,
        Some(path),
        &mut orbital_ref_diagnostics,
        &mut uses_config_diagnostics,
    )) {
        use orbital_compiler::loader::LoaderError;
        use orbital_compiler::phases::inline::InlineError;
        // Installed-package-set problems (two packages answer one lookup) and an
        // unresolvable std/scoped path are validator infrastructure, never schema
        // authoring; `PackageNotInstalled`/`BehaviorNotFound` name a real import
        // nothing answers (`needs_install`, keeping the structured code).
        let is_infra = matches!(
            &e,
            InlineError::Loader(LoaderError::StdLibNotConfigured)
                | InlineError::Loader(LoaderError::ScopedPackageNotConfigured(_))
                | InlineError::Loader(LoaderError::BehaviorAmbiguous { .. })
                | InlineError::Loader(LoaderError::PrefixConflict { .. })
        );
        let needs_install = inline_error_needs_install(&e);
        let mut message = format!("Failed to resolve external references: {}", e);
        if is_infra {
            let lc = &inline_opts.loader_config;
            message.push_str(&format!(
                " [loader env: installed_packages={}, std_lib_path_set={}]",
                lc.packages.len(),
                lc.std_lib_path.is_some(),
            ));
        }
        // Import-carrying loader errors have no spans; the failing import is
        // located at the `uses` line that declares it, matched exactly.
        let position = match &e {
            InlineError::Loader(LoaderError::InvalidPrimitiveImport(s)) => Some(s.as_str()),
            InlineError::Loader(LoaderError::StdImportNotCanonical { given, .. }) => {
                Some(given.as_str())
            }
            InlineError::Loader(LoaderError::BehaviorNotFound { name, .. }) => Some(name.as_str()),
            InlineError::Loader(LoaderError::PackageNotInstalled { prefix }) => {
                Some(prefix.as_str())
            }
            _ => None,
        }
        .and_then(|import| source_map?.import_span(import))
        .map(|span| SourcePosition::of(span, false));
        let code = if is_infra {
            "INLINE_INFRA_ERROR".to_string()
        } else {
            e.structured_code().to_string()
        };
        errors.push(Finding {
            code,
            path: e.structured_path().to_string(),
            message,
            suggestion: Some(if is_infra {
                "Validator environment error — the schema was NOT evaluated; retry, do not edit the schema"
                    .to_string()
            } else if needs_install {
                "Run `orb add <package>` for the package that declares this \
                 behavior, then retry — the schema was NOT evaluated."
                    .to_string()
            } else {
                "Check `uses:` imports resolve to known behaviors".to_string()
            }),
            fix_guidance: Some(
                "(schema not fully resolved — downstream validation suppressed until this is fixed)"
                    .to_string(),
            ),
            position,
            needs_install: needs_install.then_some(true),
            ..Finding::default()
        });
        return CoreValidation {
            errors,
            warnings,
            inlined: None,
        };
    }
    tracing::info_span!(target: ns::CLI, "enrich").in_scope(|| enrich_schema(&mut schema, &default_opts.enrichment));
    }

    errors.extend(used_secret_findings(&session));

    if let Ok(program) = serde_json::to_value(&schema) {
        for path in orbital_core::messages::render_only_violations(&program) {
            errors.push(Finding::new(
                "ORB_I18N_RENDER_ONLY".to_string(),
                path,
                "`i18n/t` and `@locale` are render-time — use them only inside `render-ui`"
                    .to_string(),
                None,
            ));
        }
    }

    let mut comprehensive_result = tracing::info_span!(target: ns::CLI, "validate").in_scope(|| validate_comprehensive_full(
        &schema,
        extra_patterns,
        schema_language,
        service_registry,
        default_opts.inline.loader_config.packages.clone(),
    ));
    merge_post_inline_diagnostics(
        &mut comprehensive_result,
        &orbital_ref_diagnostics,
        &uses_config_diagnostics,
        &schema,
        &mut program_findings,
    );
    if final_program {
        comprehensive_result.merge(
            tracing::info_span!(target: ns::CLI, "final program").in_scope(|| orbital_compiler::phases::validation::final_program::validate_final_program(&schema)),
        );
    }

    errors.extend(comprehensive_result.errors.iter().map(|e| {
        let mut f = located_finding(
            source_map,
            e.code.to_string(),
            e.path.clone(),
            e.message.clone(),
            e.suggestion.clone(),
            e.fix_guidance.as_deref(),
        );
        f.expected_shape = e.expected_shape.clone();
        f.valid_values = e.valid_values.clone();
        f
    }));
    warnings.extend(comprehensive_result.warnings.iter().map(|w| {
        located_finding(
            source_map,
            w.code.to_string(),
            w.path.clone(),
            w.message.clone(),
            w.suggestion.clone(),
            None,
        )
    }));

    for pf in &program_findings {
        let mut f = located_finding(
            source_map,
            pf.code.clone(),
            pf.path.clone(),
            pf.message.clone(),
            Some(pf.suggestion.clone()).filter(|s| !s.is_empty()),
            None,
        );
        f.valid_values = pf.valid_values.clone();
        match pf.severity {
            FindingSeverity::Error => errors.push(f),
            FindingSeverity::Warning => warnings.push(f),
        }
    }

    // Compile to OIR (schema already inlined+enriched above -> skip re-inline).
    // One reporter: a schema validation or a lint already rejected would only get
    // its root cause restated as `COMPILATION_ERROR`, so compile runs on an accepted one.
    let inlined = schema.clone();
    let mut options = tracing::info_span!(target: ns::CLI, "discover packages").in_scope(CompilerOptions::from_env);
    options.inline.skip_inline = true;
    let compile_result = if errors.is_empty() {
        Some(tracing::info_span!(target: ns::CLI, "compile").in_scope(|| compile_schema(schema, &options)))
    } else {
        None
    };
    match compile_result {
        None => {}
        Some(Ok(module)) => {
            let oir = OirValidator::new(&module).validate();
            for e in &oir.errors {
                let finding = Finding {
                    code: if e.severity == ValidationSeverity::Error {
                        "OIR_ERROR"
                    } else {
                        "OIR_WARNING"
                    }
                    .to_string(),
                    path: e.path.clone(),
                    message: e.message.clone(),
                    ..Finding::default()
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
                    message: w,
                    ..Finding::default()
                });
            }
        }
        Some(Err(e)) => errors.push(Finding::new(
            "COMPILATION_ERROR".to_string(),
            String::new(),
            format!("Compilation error: {e}"),
            None,
        )),
    }

    CoreValidation {
        errors,
        warnings,
        inlined: Some(inlined),
    }
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
        let found = source_findings(&format!(
            "app Foo \"1.0.0\"\n\"Desc\"\ntheme \"orb-light\"\n{BODY}"
        ));
        assert_eq!(
            found.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
            vec!["ORB_L_APP_HEADER_LEGACY"]
        );
        assert_eq!(found[0].path, "app:1");
    }

    #[test]
    fn control_the_bracket_header_is_clean() {
        assert!(source_findings(&format!(
            "app Foo [version: \"1.0.0\", description: \"Desc\", theme: \"orb-light\"]\n{BODY}"
        ))
        .is_empty());
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
mod one_finding_list_tests {
    use super::{run_full_validation, validation_report};
    use crate::{ValidationJsonError, ValidationJsonWarning};
    use std::path::PathBuf;

    const HEAD: &str = "app probe [ version: \"0.1.0\" ]\norbital Probe {\n  entity Item [runtime, shared] {\n    id : string!\n    note : string = \"\"\n    count : int = 0\n  }\n  trait Counter -> Item {\n    initial: idle\n    state idle {\n      INIT -> idle (render-ui main { type: \"typography\", content: \"probe\" })\n";
    const TAIL: &str = "    }\n  }\n  page \"/probe\" as Probe -> Counter\n}\n";

    const GHOST_STATE_ORB: &str = r#"{
  "name": "test-one-finding-list",
  "version": "1.0.0",
  "orbitals": [
    {
      "name": "Probe",
      "entity": {
        "name": "Item",
        "persistence": "runtime",
        "fields": [ { "name": "id", "type": "string", "required": true } ]
      },
      "traits": [
        {
          "name": "Counter",
          "linkedEntity": "Item",
          "stateMachine": {
            "states": [ { "name": "idle", "isInitial": true } ],
            "events": [ { "key": "GO" } ],
            "transitions": [ { "from": "idle", "to": "ghost", "event": "GO", "effects": [] } ]
          }
        }
      ]
    }
  ]
}"#;

    fn fixture(name: &str, ext: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "orb-one-finding-list-{}-{}.{}",
            std::process::id(),
            name,
            ext
        ));
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    fn lolo(extra_arms: &str) -> String {
        format!("{HEAD}{extra_arms}{TAIL}")
    }

    /// The report and the text render of one fixture, asserted to carry the same findings.
    fn report_and_text(path: &PathBuf) -> (Vec<ValidationJsonError>, Vec<ValidationJsonWarning>) {
        let report = validation_report(path, None, None, false, false, &[]).expect("report");
        let registry = crate::utils::load_services_registry(None).expect("registry");
        let text = run_full_validation(path, None, None, registry, false).expect("text");
        let errors = report.errors.unwrap_or_default();
        let warnings = report.warnings.unwrap_or_default();
        assert_eq!(errors.len(), text.errors.len(), "errors: {errors:?} vs {:?}", text.errors);
        assert_eq!(warnings.len(), text.warnings.len(), "warnings: {warnings:?} vs {:?}", text.warnings);
        for e in &errors {
            let prefix = if e.path.is_empty() { format!("[{}] ", e.code) } else { format!("[{}] {}:", e.code, e.path) };
            assert!(text.errors.iter().any(|t| t.starts_with(&prefix)), "no text line for {prefix} in {:?}", text.errors);
        }
        for w in &warnings {
            let hit = if w.code.is_empty() {
                text.warnings.iter().any(|t| *t == w.message)
            } else {
                let prefix = format!("[{}] {}:", w.code, w.path);
                text.warnings.iter().any(|t| t.starts_with(&prefix))
            };
            assert!(hit, "no text line for warning {w:?} in {:?}", text.warnings);
        }
        (errors, warnings)
    }

    #[test]
    fn a_program_that_does_not_parse_is_the_same_finding_in_both_renders() {
        let path = fixture("unparseable", "lolo", "app probe [ this does not parse");
        let (errors, _) = report_and_text(&path);
        assert!(!errors.is_empty());
        assert!(errors[0].position.is_some(), "the parse finding carries its span: {errors:?}");
    }

    #[test]
    fn a_missing_file_is_the_same_finding_in_both_renders() {
        let path = std::env::temp_dir().join(format!("orb-one-finding-list-{}-absent.lolo", std::process::id()));
        let (errors, _) = report_and_text(&path);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "FILE_READ_ERROR");
    }

    #[test]
    fn the_text_line_is_rendered_from_the_finding_alone() {
        use super::Finding;
        let finding = Finding {
            code: "ORB_X".into(),
            path: "orbitals.A".into(),
            message: "broken".into(),
            suggestion: Some("fix it".into()),
            ..Finding::default()
        };
        assert_eq!(finding.render(), "[ORB_X] orbitals.A: broken \u{1f4a1} fix it");
        let pathless = Finding { path: String::new(), suggestion: None, ..finding };
        assert_eq!(pathless.render(), "[ORB_X] broken", "control: no path, no suggestion");
        let codeless = Finding { code: String::new(), message: "guard".into(), ..pathless };
        assert_eq!(codeless.render(), "guard", "control: a finding with no code is its message");
    }

    #[test]
    fn a_clean_program_reports_the_same_empty_errors_in_both_renders() {
        let path = fixture("clean", "lolo", &lolo("      BUMP -> idle when (< @entity.count 10) (set @entity.count (+ @entity.count 1))\n"));
        let (errors, _) = report_and_text(&path);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_program_with_errors_reports_the_same_list_in_both_renders() {
        let path = fixture("ghost", "orb", GHOST_STATE_ORB);
        let (errors, _) = report_and_text(&path);
        assert!(!errors.is_empty());
    }

    #[test]
    fn i18n_outside_render_ui_is_an_error_in_the_json_report_too() {
        let path = fixture("i18n", "lolo", &lolo("      SET -> idle (set @entity.note (i18n/t \"greeting.title\"))\n"));
        let (errors, _) = report_and_text(&path);
        assert!(errors.iter().any(|e| e.code == "ORB_I18N_RENDER_ONLY"), "{errors:?}");
    }

    #[test]
    fn a_non_boolean_guard_warns_in_the_json_report_too() {
        let path = fixture("guard", "lolo", &lolo("      BUMP -> idle when (+ @entity.count 1) (set @entity.count 0)\n"));
        let (errors, warnings) = report_and_text(&path);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.iter().any(|w| w.message.contains("guard does not return boolean")), "{warnings:?}");
    }
    const SITE: &str = "app site [ version: \"1.0.0\", description: \"Site\", locales: [en, ar] ]\n\norbital Home {\n  entity HomeView [runtime] {\n    id : string!\n  }\n\n  trait HomePage -> HomeView {\n    initial: idle\n\n    state idle {\n      INIT -> idle\n        (render-ui main { type: typography, content: (i18n/t \"hero.title\") })\n    }\n  }\n\n  page \"/\" as HomeRoute -> HomePage\n}\n";

    fn site_with_catalogs() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("orb-emitted-catalogs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source_dir = root.join("source");
        let emitted_dir = root.join("emitted");
        std::fs::create_dir_all(&emitted_dir).unwrap();
        for (locale, message) in [("en", "Hello"), ("ar", "hello-ar")] {
            let catalog = source_dir.join("locales").join(locale);
            std::fs::create_dir_all(&catalog).unwrap();
            std::fs::write(catalog.join("site.json"), format!(r#"{{"hero.title":"{message}"}}"#)).unwrap();
        }
        std::fs::write(source_dir.join("site.lolo"), SITE).unwrap();
        std::fs::write(emitted_dir.join("site.lolo"), SITE).unwrap();
        (source_dir.join("site.lolo"), emitted_dir.join("site.lolo"))
    }

    fn i18n_errors(program: &PathBuf, catalogs_beside: Option<&std::path::Path>) -> Vec<String> {
        let registry = crate::utils::load_services_registry(None).expect("registry");
        run_full_validation(program, catalogs_beside, None, registry, false)
            .expect("text")
            .errors
            .into_iter()
            .filter(|e| e.contains("ORB_I18N"))
            .collect()
    }

    #[test]
    fn an_emitted_program_is_validated_against_the_catalogs_its_source_declared() {
        let (source, emitted) = site_with_catalogs();
        assert!(i18n_errors(&source, None).is_empty(), "control: the source reads its own catalogs");
        assert!(!i18n_errors(&emitted, None).is_empty(), "control: beside the output there are no catalogs");
        assert!(i18n_errors(&emitted, Some(&source)).is_empty(), "{:?}", i18n_errors(&emitted, Some(&source)));
    }

}
