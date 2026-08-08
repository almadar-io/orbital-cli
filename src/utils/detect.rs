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
/// For Orb JSON, i18n translation is applied before parsing. `source_path`
/// tags `.lolo` parse errors with the real file path (or `"<stdin>"` /
/// `"<input>"` for callers with no path) instead of a placeholder.
/// Returns the parsed `OrbitalSchema`.
pub(crate) fn parse_schema_auto(
    content: &str,
    source_path: &str,
) -> anyhow::Result<orbital_core::schema::OrbitalSchema> {
    parse_schema_auto_typed(content, source_path).map_err(|e| anyhow::anyhow!("{}", e.describe()))
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
            TypedParseError::Lolo(e) => format!("LOLO parse error: {}", e.render_with_hint()),
            TypedParseError::Json(msg) => format!("JSON parse error: {msg}"),
        }
    }
}

/// Like `parse_schema_auto`, but preserves the underlying error shape so
/// callers can surface typed diagnostics (e.g., `ORB_T_TYPE_ANNOTATION_MISMATCH`
/// when the lowerer's resolver rejected a `::` annotation).
pub fn parse_schema_auto_typed(
    content: &str,
    source_path: &str,
) -> Result<orbital_core::schema::OrbitalSchema, TypedParseError> {
    use orbital_core::schema::parse_schema;
    match detect_syntax(content) {
        InputSyntax::Lolo => {
            orbital_lolo::parse_and_lower_str_at(content, source_path).map_err(TypedParseError::Lolo)
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

/// [`parse_schema_auto_typed`] additionally returning the L2 source map (see
/// [`orbital_lolo::source_map::LoloSourceMap`]) on the `.lolo` branch; `None`
/// for `.orb` JSON input (there's no `.lolo` source to map back to).
pub fn parse_schema_auto_typed_with_map(
    content: &str,
    source_path: &str,
) -> Result<
    (
        orbital_core::schema::OrbitalSchema,
        Option<orbital_lolo::LoloSourceMap>,
    ),
    TypedParseError,
> {
    use orbital_core::schema::parse_schema;
    match detect_syntax(content) {
        InputSyntax::Lolo => orbital_lolo::parse_and_lower_str_with_map(content, source_path)
            .map(|(schema, map)| (schema, Some(map)))
            .map_err(TypedParseError::Lolo),
        InputSyntax::OrbJson => {
            let translated = translate_schema_content(content);
            parse_schema(&translated)
                .map(|mut schema| {
                    orbital_core::stamp::ensure_stamped(&mut schema);
                    (schema, None)
                })
                .map_err(|e| TypedParseError::Json(e.to_string()))
        }
    }
}

/// [`parse_schema_auto`] additionally returning the L2 source map. See
/// [`parse_schema_auto_typed_with_map`].
pub(crate) fn parse_schema_auto_with_map(
    content: &str,
    source_path: &str,
) -> anyhow::Result<(
    orbital_core::schema::OrbitalSchema,
    Option<orbital_lolo::LoloSourceMap>,
)> {
    parse_schema_auto_typed_with_map(content, source_path)
        .map_err(|e| anyhow::anyhow!("{}", e.describe()))
}

/// Map a `TypedParseError` into one or more `ValidationJsonError` rows so
/// `orbital validate --json` can surface typed codes. The hint format
/// matches the 💡-style suggestion column used by the rest of the validator.
pub(crate) fn typed_parse_error_to_json(
    e: &TypedParseError,
    source: Option<&str>,
) -> Vec<ValidationJsonError> {
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
            // A `//` "comment" is not lexable: the lexer walks INTO the
            // comment text and reports the first illegal character it meets
            // (an apostrophe in prose, a `?`, …) — a token-level hint about
            // THAT character misdirects (observed: the single-quote hint sent
            // a model rewriting comment prose instead of the `//`). When the
            // error line itself is a `//` comment, the line-grounded fact
            // outranks any token hint.
            let source_grounded = source.and_then(|s| {
                let msg = format!("{inner}");
                slash_comment_line_hint(s, span.line as usize)
                    .or_else(|| {
                        if msg.contains("object key expected, got LParen") {
                            js_call_syntax_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("unexpected token in s-expression: Colon") {
                            swallowed_block_entry_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
            });
            let (suggestion, fix_guidance) = match source_grounded {
                Some((s, f)) => (Some(s), Some(f)),
                None => inner.hint(),
            };
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
                TypeError::UnknownType { suggestion, name, .. } => (
                    "ORB_T_TYPE_UNKNOWN",
                    Some(match (suggestion, foreign_type_translation(name)) {
                        (Some(s), _) => format!("did you mean {s:?}?"),
                        (None, Some(t)) => t.to_string(),
                        (None, None) => format!(
                            "no type named {name:?} is declared — declare it at orbital level (`type {name} = {{ field : type … }}`), or use a primitive (string/number/boolean/any) or a declared type/entity name"
                        ),
                    }),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.lolo` parse error tagged with the real caller-supplied path must
    /// render `path:line:col:` (not the `<input>` placeholder), and
    /// `describe()` must carry the 💡 teaching hint through to CLI stderr.
    #[test]
    fn parse_error_renders_with_real_path_and_hint() {
        // Semicolon comments aren't `.lolo` syntax — a reliable parse error
        // with a known hint (see LexError::hint).
        let bad = "orbital X {\n  entity X [runtime] { id : string! } ; oops\n}\n";
        let err = parse_schema_auto_typed(bad, "foo/bar.lolo")
            .expect_err("semicolon comment must fail to parse");
        let TypedParseError::Lolo(lolo_err) = &err else {
            panic!("expected a Lolo parse error, got: {}", err.describe());
        };
        assert!(
            lolo_err.render().starts_with("foo/bar.lolo:"),
            "expected real path in render(), got: {}",
            lolo_err.render()
        );
        assert!(
            err.describe().contains("\u{1f4a1}"),
            "describe() must carry the teaching hint: {}",
            err.describe()
        );
    }
}

/// Deterministic translation for type names imported from mainstream
/// languages — the constructive declare-it fallback is wrong advice for
/// these (a model writing `array` wants `[ElementType]`, not a new alias).
fn foreign_type_translation(name: &str) -> Option<&'static str> {
    match name {
        "array" | "Array" | "list" | "List" => {
            Some("a list type is written `[ElementType]` — e.g. `[string]`, `[number]`, or `[MyType]`")
        }
        "int" | "integer" | "Int" | "float" | "double" | "Number" => {
            Some("numeric fields use the `number` type")
        }
        "bool" | "Bool" | "Boolean" => Some("use `boolean`"),
        "str" | "String" => Some("use `string`"),
        "object" | "Object" | "dict" | "Dict" | "map" | "Map" | "Record" => {
            Some("declare a shaped type at orbital level (`type Name = { field : type … }`) and use it, or use `any`")
        }
        _ => None,
    }
}

/// When the parse/lex error's line is itself a `//` comment, the real defect
/// is the comment marker, not whatever character the lexer tripped on inside
/// the comment prose. Line-grounded and deterministic: reads the exact line.
fn slash_comment_line_hint(source: &str, line: usize) -> Option<(String, String)> {
    let text = source.lines().nth(line.checked_sub(1)?)?;
    if text.trim_start().starts_with("//") {
        Some((
            "this line is a `//` comment — `.lolo` has no `//` comments; they start with `;;`"
                .to_string(),
            "replace the leading `//` with `;;` on every comment line (or delete the comment)"
                .to_string(),
        ))
    } else {
        None
    }
}

/// `uuid()` / `now()` — JS call syntax in value position. The discriminator is
/// textual and exact: the reported column points at the `(`, and an identifier
/// character immediately before it means a glued `name(...)` call (a legal
/// `.lolo` `(operator …)` always has whitespace, `{`, `[`, or `(` before its
/// opening paren).
fn js_call_syntax_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let text = source.lines().nth(line.checked_sub(1)?)?;
    let before = text.chars().nth(col.checked_sub(2)?)?;
    if before.is_ascii_alphanumeric() || before == '_' {
        Some((
            "`name(…)` is JS call syntax — `.lolo` operators are prefix s-expressions: `(operator arg …)`"
                .to_string(),
            "write `(time/now)` not `now()`; entity ids are auto-generated — omit `id:` from `persist create` payloads (idiom: `persist create Entity ?data { emit: { … } }`)"
                .to_string(),
        ))
    } else {
        None
    }
}

/// A colon inside an s-expression, where the error LINE is itself a plain
/// `key: value` block entry (config/object member): an earlier `(` is missing
/// its `)`, so the parser consumed past the effect into the next block — the
/// reported line is the victim, not the culprit. Gated on the line shape so a
/// genuine in-call colon (named-argument habit) never gets this hint.
fn swallowed_block_entry_hint(source: &str, line: usize) -> Option<(String, String)> {
    let text = source.lines().nth(line.checked_sub(1)?)?;
    let trimmed = text.trim_start();
    let ident_len = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .count();
    if ident_len > 0 && trimmed[ident_len..].trim_start().starts_with(':') {
        Some((
            "this `key: value` line is fine — the parser was still INSIDE an s-expression when it got here, because an earlier `(` above is missing its `)`".to_string(),
            "re-count parens in the effect ABOVE this line and add the missing `)` there".to_string(),
        ))
    } else {
        None
    }
}
