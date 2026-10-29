//! `orb validate --json`'s report: the parse (this crate owns the `.lolo` parser) around the compiler's
//! [`validate_parsed`] pipeline, with source positions handed over through [`SourceLocator`].

use super::{
    detect_language, detect_syntax, load_extra_patterns, load_services_registry, typed_parse_error_to_json,
};
use crate::{SourcePosition, ValidationJsonError, ValidationJsonOutput, ValidationJsonWarning, ValidationStats};
use orbital_compiler::phases::validation::report::{
    passes, validate_parsed, Finding, FindingSeverity, ProgramFinding, Report, SourceLocator, SourcePosition as Position, SourceSpan,
    ValidationInput,
};

struct LoloLocator<'a>(&'a orbital_lolo::LoloSourceMap);

fn span(s: orbital_lolo::Span) -> SourceSpan {
    SourceSpan { line: s.line, col: s.col, len: u32::from(s.len) }
}

impl SourceLocator for LoloLocator<'_> {
    fn locate(&self, path: &str) -> Option<(SourceSpan, bool)> {
        self.0.lookup_detailed(path).map(|(s, exact)| (span(s), exact))
    }
    fn import_span(&self, import: &str) -> Option<SourceSpan> {
        self.0.import_span(import).map(span)
    }
}

/// `.lolo` source facts the lowered schema cannot carry: the pre-bracket app header (a positional version,
/// description/theme/locales/public on loose lines) lowers to the same `.orb`, so only the source can report it.
pub(crate) fn source_findings(content: &str) -> Vec<Finding> {
    if !matches!(detect_syntax(content), super::InputSyntax::Lolo) {
        return Vec::new();
    }
    let Ok(program) = orbital_lolo::parse_str(content) else {
        return Vec::new();
    };
    if !program.app.legacy_lines {
        return Vec::new();
    }
    vec![Finding::new(
        "ORB_L_APP_HEADER_LEGACY".to_string(),
        format!("app:{}", program.app.span.line),
        "the app header writes its modifiers outside its bracket (a positional version, or description/theme/locales/public on loose lines); declare them in the bracket: `app NAME [ version: \"1.0.0\", description: …, theme: …, locales: […], public: … ]`".to_string(),
        Some("`orb format --write <file>` rewrites the header into the bracket form".to_string()),
    )]
}

fn position(p: Position) -> SourcePosition {
    SourcePosition { line: p.line, column: p.column, end_line: p.end_line, end_column: p.end_column, approximate: p.approximate }
}

fn from_json_error(e: ValidationJsonError) -> Finding {
    Finding {
        code: e.code,
        path: e.path,
        message: e.message,
        suggestion: e.suggestion,
        fix_guidance: e.fix_guidance,
        expected_shape: e.expected_shape,
        valid_values: e.valid_values,
        position: e.position.map(|p| Position {
            line: p.line,
            column: p.column,
            end_line: p.end_line,
            end_column: p.end_column,
            approximate: p.approximate,
        }),
        needs_install: e.needs_install,
    }
}

fn json_error(f: Finding) -> ValidationJsonError {
    ValidationJsonError {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        fix_guidance: f.fix_guidance,
        expected_shape: f.expected_shape,
        valid_values: f.valid_values,
        position: f.position.map(position),
        needs_install: f.needs_install,
    }
}

fn json_warning(f: Finding) -> ValidationJsonWarning {
    ValidationJsonWarning {
        code: f.code,
        path: f.path,
        message: f.message,
        suggestion: f.suggestion,
        position: f.position.map(position),
    }
}

/// The finding for a missing/incompatible installed language data package, when there is one.
fn language_data_finding() -> Option<Finding> {
    let error = super::patterns::ensure_language_ready().err()?;
    Some(from_json_error(super::detect::language_data_json_error(&error)))
}

/// The report for the file at `path`: discovery, read, parse, then the compiler pipeline. Never bails on a finding;
/// a failure that stops validation is the report's one error.
pub(crate) fn findings_report(
    path: &std::path::Path,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
    program_findings: &[ProgramFinding],
) -> anyhow::Result<Report> {
    if let Some(refused) = language_data_finding() {
        return Ok(Report::refused(refused));
    }
    let extra_patterns = load_extra_patterns(path, patterns_path);
    let service_registry = load_services_registry(services_path)?;
    let options = orbital_compiler::CompilerOptions::from_env();
    if let Some(refused) = orbital_compiler::phases::validation::report::discovery_refusal(&options) {
        return Ok(Report::refused(refused));
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Report::refused(Finding {
                code: "FILE_READ_ERROR".to_string(),
                message: format!("Failed to read schema file: {e}"),
                ..Finding::default()
            }))
        }
    };
    let (schema, source_map) = match super::parse_schema_auto_typed_with_map(&content, &path.display().to_string()) {
        Ok(parsed) => parsed,
        Err(e) => {
            let mut report = Report {
                errors: typed_parse_error_to_json(&e, Some(&content)).into_iter().map(from_json_error).collect(),
                ..Report::default()
            };
            for pf in program_findings {
                let mut f = Finding::new(pf.code.clone(), pf.path.clone(), pf.message.clone(), Some(pf.suggestion.clone()).filter(|s| !s.is_empty()));
                f.valid_values = pf.valid_values.clone();
                match pf.severity {
                    FindingSeverity::Error => report.errors.push(f),
                    FindingSeverity::Warning => report.warnings.push(f),
                }
            }
            return Ok(report);
        }
    };
    let language = match detect_language(&content) {
        Some(lang) if lang != "en" => orbital_compiler::SchemaLanguage::NonLatin(lang),
        _ => orbital_compiler::SchemaLanguage::English,
    };
    let locator = source_map.as_ref().map(LoloLocator);
    let mut report = validate_parsed(
        ValidationInput {
            schema,
            resources_beside: Some(path),
            locator: locator.as_ref().map(|l| l as &dyn SourceLocator),
            language,
            path,
            extra_patterns,
            service_registry,
            program_findings,
        },
        &options,
    );
    let mut warnings = source_findings(&content);
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    Ok(report)
}

/// `orb validate --json`'s report as a value: what the CLI prints and `orb mcp` returns.
pub(crate) fn validation_report(
    path: &std::path::Path,
    patterns_path: Option<&std::path::Path>,
    services_path: Option<&std::path::Path>,
    deny_warnings: bool,
    program_findings: &[ProgramFinding],
) -> anyhow::Result<ValidationJsonOutput> {
    let report = findings_report(path, patterns_path, services_path, program_findings)?;
    let errors: Vec<ValidationJsonError> = report.errors.into_iter().map(json_error).collect();
    let warnings: Vec<ValidationJsonWarning> = report.warnings.into_iter().map(json_warning).collect();
    Ok(ValidationJsonOutput {
        success: true,
        valid: passes(errors.len(), warnings.len(), deny_warnings),
        errors: (!errors.is_empty()).then_some(errors),
        warnings: (!warnings.is_empty()).then_some(warnings),
        stats: report.stats.map(|s| ValidationStats {
            orbitals: s.orbitals,
            entities: s.entities,
            traits: s.traits,
            pages: s.pages,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(source_findings("{\"name\":\"Foo\",\"orbitals\":[]}").is_empty());
    }

    #[test]
    fn a_program_that_does_not_parse_carries_its_span() {
        let path = std::env::temp_dir().join(format!("orb-validation-{}-unparseable.lolo", std::process::id()));
        std::fs::write(&path, "app probe [ this does not parse").unwrap();
        let report = validation_report(&path, None, None, false, &[]).expect("report");
        let errors = report.errors.expect("errors");
        assert!(errors[0].position.is_some(), "{errors:?}");
        assert!(!report.valid);
    }

    fn lint_finding(severity: FindingSeverity) -> ProgramFinding {
        ProgramFinding { code: "LINT_PASSED".into(), path: "app".into(), message: "the lint ran".into(), suggestion: String::new(), severity, valid_values: None }
    }

    #[test]
    fn a_program_that_does_not_parse_still_reports_the_findings_the_lint_passed() {
        let path = std::env::temp_dir().join(format!("orb-validation-{}-unparseable-lint.lolo", std::process::id()));
        std::fs::write(&path, "app probe [ this does not parse").unwrap();
        let findings = [lint_finding(FindingSeverity::Error), lint_finding(FindingSeverity::Warning)];
        let report = validation_report(&path, None, None, false, &findings).expect("report");
        let errors = report.errors.expect("errors");
        assert!(errors.iter().any(|e| e.code == "LINT_PASSED"), "{errors:?}");
        assert!(errors.iter().any(|e| e.position.is_some()), "{errors:?}");
        assert!(report.warnings.expect("warnings").iter().any(|w| w.code == "LINT_PASSED"));
    }

    #[test]
    fn control_a_program_that_does_not_parse_reports_no_lint_findings_when_the_lint_passed_none() {
        let path = std::env::temp_dir().join(format!("orb-validation-{}-unparseable-nolint.lolo", std::process::id()));
        std::fs::write(&path, "app probe [ this does not parse").unwrap();
        let report = validation_report(&path, None, None, false, &[]).expect("report");
        assert!(report.errors.expect("errors").iter().all(|e| e.code != "LINT_PASSED"));
        assert!(report.warnings.is_none());
    }

    #[test]
    fn a_missing_file_is_one_file_read_error() {
        let path = std::env::temp_dir().join(format!("orb-validation-{}-absent.lolo", std::process::id()));
        let errors = validation_report(&path, None, None, false, &[]).expect("report").errors.expect("errors");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "FILE_READ_ERROR");
    }
}
