//! Syntax detection and schema parsing (LOLO vs `.orb` JSON), plus the typed
//! parse-error mapping consumed by `orbital validate --json`.

use crate::utils::i18n::translate_schema_content;
use crate::ValidationJsonError;

/// Canonicalize serde_json for `.orb` emission: recursively strip `null`
/// values AND emit object keys in deterministic (sorted) order. Used by the
/// `.lolo` lowering / `emit orb` / `compile` paths before writing to disk.
///
/// The sort is what makes `.orb` output byte-stable: `serde_json` is built with
/// `preserve_order` (insertion-order `IndexMap`), and several schema/IR types
/// are backed by `HashMap<String, _>` (theme tokens, config provenance, dynamic
/// render-ui props), whose iteration order is randomized per process. Without
/// sorting here, re-emitting an unchanged `.orb` reshuffles keys every run and
/// churns the whole std registry. Inserting in sorted order yields a stable,
/// canonical form (JSON object key order is semantically irrelevant downstream).
pub(crate) fn strip_json_nulls(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, v) in entries {
                let stripped = strip_json_nulls(v);
                if !stripped.is_null() {
                    out.insert(k, stripped);
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_json_nulls).collect()),
        v => v,
    }
}

/// Syntax variant detected from file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSyntax {
    /// Standard `.orb` JSON
    OrbJson,
    /// LOLO surface language
    Lolo,
}

/// Detect whether a source string is LOLO or `.orb` JSON by inspecting the
/// first non-whitespace character. Every valid `.orb` JSON starts with `{`;
/// LOLO starts with a keyword (`app`, `orbital`, `type`), comment (`#`, `;;`),
/// or other non-brace character.
pub(crate) fn detect_syntax(content: &str) -> InputSyntax {
    let first = content
        .trim_start_matches('\u{FEFF}') // strip UTF-8 BOM
        .trim_start()
        .chars()
        .next();
    match first {
        Some('{') => InputSyntax::OrbJson,
        _ => InputSyntax::Lolo,
    }
}

/// Parse a schema from its raw content, auto-detecting LOLO vs Orb JSON syntax.
/// For Orb JSON, i18n translation is applied before parsing.
/// Returns the parsed `OrbitalSchema`.
pub(crate) fn parse_schema_auto(
    content: &str,
) -> anyhow::Result<orbital_core::schema::OrbitalSchema> {
    parse_schema_auto_typed(content).map_err(|e| anyhow::anyhow!("{}", e.describe()))
}

/// Typed parse-error for use by callers that want to map to structured
/// diagnostics (e.g., `orbital validate --json` → `ORB_T_TYPE_*` codes).
/// `parse_schema_auto` wraps this in `anyhow` for legacy callers.
pub enum TypedParseError {
    Lolo(orbital_lolo::LoloError),
    Json(String),
}

impl TypedParseError {
    /// Human-readable single-line message for CLI stderr or anyhow wrappers.
    pub fn describe(&self) -> String {
        match self {
            TypedParseError::Lolo(e) => format!("LOLO parse error: {}", e.render()),
            TypedParseError::Json(msg) => format!("JSON parse error: {msg}"),
        }
    }
}

/// Like `parse_schema_auto`, but preserves the underlying error shape so
/// callers can surface typed diagnostics (e.g., `ORB_T_TYPE_ANNOTATION_MISMATCH`
/// when the lowerer's resolver rejected a `::` annotation).
pub fn parse_schema_auto_typed(
    content: &str,
) -> Result<orbital_core::schema::OrbitalSchema, TypedParseError> {
    use orbital_core::schema::parse_schema;
    match detect_syntax(content) {
        InputSyntax::Lolo => {
            orbital_lolo::parse_and_lower_str(content).map_err(TypedParseError::Lolo)
        }
        InputSyntax::OrbJson => {
            let translated = translate_schema_content(content);
            parse_schema(&translated)
                .map(|mut schema| {
                    orbital_core::stamp::ensure_stamped(&mut schema);
                    schema
                })
                .map_err(|e| TypedParseError::Json(e.to_string()))
        }
    }
}

/// Map a `TypedParseError` into one or more `ValidationJsonError` rows so
/// `orbital validate --json` can surface typed codes. The hint format
/// matches the 💡-style suggestion column used by the rest of the validator.
pub(crate) fn typed_parse_error_to_json(e: &TypedParseError) -> Vec<ValidationJsonError> {
    use orbital_lolo::error::TypeError;
    use orbital_lolo::{LoloError, LowerError};

    match e {
        TypedParseError::Json(msg) => vec![ValidationJsonError {
            code: "JSON_PARSE_ERROR".to_string(),
            path: "".to_string(),
            message: format!("Failed to parse schema: {msg}"),
            suggestion: Some("Check JSON syntax and structure".to_string()),
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
            line: None,
            column: None,
        }],
        TypedParseError::Lolo(LoloError::Io { path, message }) => vec![ValidationJsonError {
            code: "LOLO_IO_ERROR".to_string(),
            path: path.clone(),
            message: message.clone(),
            suggestion: None,
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
            line: None,
            column: None,
        }],
        TypedParseError::Lolo(LoloError::Parse { path, inner }) => {
            let span = inner.span();
            let (suggestion, fix_guidance) = inner.hint();
            vec![ValidationJsonError {
                code: "LOLO_PARSE_ERROR".to_string(),
                path: format!("{path}:{}:{}", span.line, span.col),
                message: format!("{inner}"),
                suggestion,
                fix_guidance,
                expected_shape: None,
                valid_values: None,
                line: Some(span.line),
                column: Some(span.col),
            }]
        }
        TypedParseError::Lolo(LoloError::Lower {
            path,
            inner: LowerError::Type(te),
        }) => {
            let span = te.span();
            let path_loc = format!("{path}:{}:{}", span.line, span.col);
            let (code, suggestion) = match te {
                TypeError::UnknownType { suggestion, .. } => (
                    "ORB_T_TYPE_UNKNOWN",
                    suggestion.as_ref().map(|s| format!("did you mean {s:?}?")),
                ),
                TypeError::CircularType { .. } => (
                    "ORB_T_TYPE_CYCLE",
                    Some(
                        "break the cycle by removing one reference in the composition chain"
                            .to_string(),
                    ),
                ),
                TypeError::KindMismatch { .. } => (
                    "ORB_T_TYPE_KIND_MISMATCH",
                    Some("composed types must share a kind (all Entity, all Trait, …)".to_string()),
                ),
                TypeError::CompositionConflict { field, .. } => (
                    "ORB_T_TYPE_COMPOSITION_CONFLICT",
                    Some(format!(
                        "remove field {field:?} from one side or reconcile its type"
                    )),
                ),
                TypeError::AnnotationMismatch { .. } => (
                    "ORB_T_TYPE_ANNOTATION_MISMATCH",
                    Some(
                        "update the body to match the annotated type or drop the `::` annotation"
                            .to_string(),
                    ),
                ),
                TypeError::UnknownTypeParam { name, alias, .. } => (
                    "ORB_T_TYPE_UNKNOWN_PARAM",
                    Some(format!(
                        "type parameter {name:?} is not declared on alias {alias:?}"
                    )),
                ),
                TypeError::WrongTypeArgCount { expected, got, .. } => (
                    "ORB_T_TYPE_WRONG_ARG_COUNT",
                    Some(format!(
                        "supply exactly {expected} type argument(s), got {got}"
                    )),
                ),
                TypeError::EventTypeWrongKind { name, actual, .. } => (
                    "ORB_T_EVENT_TYPE_WRONG_KIND",
                    Some(format!(
                        "declare {name:?} with `type {name} = Event<...>`, got a {actual} instead"
                    )),
                ),
            };
            vec![ValidationJsonError {
                code: code.to_string(),
                path: path_loc,
                message: format!("{te}"),
                suggestion,
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                line: Some(span.line),
                column: Some(span.col),
            }]
        }
        TypedParseError::Lolo(LoloError::Lower { path, inner }) => vec![ValidationJsonError {
            code: "LOLO_LOWER_ERROR".to_string(),
            path: path.clone(),
            message: format!("{inner}"),
            suggestion: None,
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
            line: None,
            column: None,
        }],
    }
}
