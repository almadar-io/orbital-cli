//! Syntax detection and schema parsing (LOLO vs `.orb` JSON), plus the typed
//! parse-error mapping consumed by `orbital validate --json`.

use crate::utils::i18n::translate_schema_content;
use crate::{SourcePosition, ValidationJsonError};

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
        InputSyntax::Lolo => orbital_lolo::parse_and_lower_str_at(content, source_path)
            .map_err(TypedParseError::Lolo),
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
            position: None,
            needs_install: None,
        }],
        TypedParseError::Lolo(LoloError::Io { path, message }) => vec![ValidationJsonError {
            code: "LOLO_IO_ERROR".to_string(),
            path: path.clone(),
            message: message.clone(),
            suggestion: None,
            fix_guidance: None,
            expected_shape: None,
            valid_values: None,
            position: None,
            needs_install: None,
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
                        // Second gate (run 39d: `id : string! = uuid()` — an
                        // entity-field default written as a JS call; the `(`
                        // lands after the bare-identifier default parsed).
                        if msg.contains("object key expected, got LParen")
                            || msg.contains("expected newline or }, got LParen")
                        {
                            js_call_syntax_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // Second shape (run 36: `(fn day { if (== …) … })`) —
                        // a body starting with a bare word reads as an object
                        // KEY first, so the failure lands on the `:` expect.
                        if msg.contains("object key expected, got LParen")
                            || msg.contains("expected :, got LParen")
                        {
                            braced_lambda_body_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("object key expected, got Sigil") {
                            value_continued_past_first_term_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("too many expressions in") {
                            infix_in_call_line_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `type Name { … }` without the `=` (run 30: StudyStreaks,
                        // 3 identical validates on "expected =, got LBrace").
                        if msg.contains("expected =, got LBrace") {
                            Some((
                                "a `type` declaration needs `=` between the name and the body".to_string(),
                                "write `type Name = { field : type … }` — insert the missing `=` before the `{`".to_string(),
                            ))
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // The infix habit surfaces as `got Identifier("+")` (or
                        // another operator token) in ANY parser context — run 29:
                        // TopicManager hit it as "expected }, got Identifier(\"+\")".
                        // One teaching fits all sites.
                        if ["+", "-", "*", ">", "<", ">=", "<=", "==", "!="]
                            .iter()
                            .any(|op| msg.contains(&format!("got Identifier(\"{op}\")")))
                        {
                            Some((
                                "an infix operator appears mid-expression — `.lolo` has NO infix syntax; a value is ONE expression and operators are prefix calls".to_string(),
                                "rewrite as a prefix call: `(str/concat a b)` to append text, `(+ a b)` / `(> a b)` for arithmetic/comparison — never `a + b`".to_string(),
                            ))
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `uses X from "path" { dateField: … }` — bare knobs on
                        // the import line. The parser opens the `{ … }` body
                        // (orbital-import Stage A) and then wants `config` or
                        // `}`, so the token error lands on the first knob.
                        if msg.contains("expected }, got Identifier") {
                            uses_inline_config_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `field : map[k, v]` / `array[t]` — generic-style
                        // type syntax (run 38: ReviewHeatmap).
                        if msg.contains("got LBracket") {
                            generic_type_syntax_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `{ "some text" }` — a keyless object entry: the
                        // quoted string parses as the KEY, then `:` is
                        // expected (run 38: StudyStreak — the generic
                        // quoting hint misdirected because the text WAS
                        // quoted).
                        if msg.contains("expected :, got") {
                            keyless_object_entry_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `{ streakCount: @entity.currentStreak + 1 }` — the
                        // infix habit inside an OBJECT VALUE: the `+` parses
                        // as a would-be key, so the failure lands on the `:`
                        // expect and the Identifier("+") infix gate never
                        // sees it (run 40: ministral StreakTracker).
                        if msg.contains("expected :, got") {
                            infix_in_object_value_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `<UiStack …>` where an object entry/expression is
                        // expected — JSX markup in expression position (run
                        // 40: ministral, all three residual stalls were
                        // renderItem lambdas with JSX bodies).
                        if msg.contains("expected :, got") {
                            jsx_markup_in_object_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("unexpected token in s-expression: Colon") {
                            operator_head_colon_hint(
                                s,
                                span.line as usize,
                                span.col as usize,
                            )
                            .or_else(|| swallowed_block_entry_hint(s, span.line as usize))
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // RParen included: run 21 showed `)` colliding with an
                        // unclosed `[` (children array) the same way `]`/`}`
                        // collide with an unclosed `(` — same replay, same fix.
                        if msg.contains("unexpected token in s-expression: RBracket")
                            || msg.contains("unexpected token in s-expression: RBrace")
                            || msg.contains("unexpected token in s-expression: RParen")
                        {
                            unbalanced_closer_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("expected :, got") {
                            parenless_effect_in_tree_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("expected operator, got Newline") {
                            paren_should_be_brace_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                        {
                            orbital_keyword_inside_trait_hint(
                                s,
                                &msg,
                                span.line as usize,
                                span.col as usize,
                            )
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                            && (msg.contains("got RParen") || msg.contains("got RBracket"))
                        {
                            surplus_closer_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // An effect under a ROUTED `listens` entry (run 40b:
                        // `StudySchedule.ScheduleSaved -> idle` + effects) —
                        // the "state closed too early" variant hint misdirects
                        // when the enclosing block is `listens { … }`.
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                            && msg.contains("got LParen")
                        {
                            effect_in_listens_hint(s, span.line as usize, span.col as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                        {
                            parenless_effect_at_trait_level_hint(&msg)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // A transition arm (`error -> loading`) at trait level —
                        // the run-19 arm hint gates on event-identifier CASE and
                        // missed lowercase names (run 30: TopicSchedule spun 3×
                        // under the state-body catch-all). Line shape decides:
                        // anything followed by `->` is arm-shaped.
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                        {
                            let line_text =
                                s.lines().nth((span.line as usize).saturating_sub(1)).unwrap_or("");
                            let mut parts = line_text.trim_start().split_whitespace();
                            if parts.next().is_some() && parts.next() == Some("->") {
                                // Same arm shape, two different mistakes: inside
                                // a `listens { … }` block it is invented routing
                                // syntax (run 31: `Orbital.event -> idle { … }`),
                                // elsewhere it is an arm ejected by a surplus `}`.
                                let in_listens = innermost_open_delimiter(
                                    s,
                                    span.line as usize,
                                    span.col as usize,
                                )
                                .and_then(|(_, l, _)| s.lines().nth(l.saturating_sub(1)))
                                .is_some_and(|opener| opener.trim_start().starts_with("listens"));
                                if in_listens {
                                    Some((
                                        "a `listens` entry cannot carry `-> target` routing or a body block — it only NAMES the event (optionally with `{ with/guard }` refinements)".to_string(),
                                        "write the entry as just `EVENT_NAME`, then route it with a transition arm `EVENT_NAME -> target` inside a `state { … }` block (effects go under that arm)".to_string(),
                                    ))
                                } else {
                                    Some((
                                        "this transition arm is sitting OUTSIDE any `state { … }` block — a `}` above this line closed the state block too early".to_string(),
                                        "delete the surplus `}` between the previous effect and this arm (or move the arm back inside its `state <name> { … }` block)".to_string(),
                                    ))
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // A markdown `---` divider in the file (run 39d:
                        // ReviewScheduler) — sibling of the ``` fence class.
                        if msg.contains("got DashDash") {
                            markdown_divider_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `orbital Name "1.0.0"` / bare docstring (run 39e:
                        // granite ×14 — the app-header form leaking onto the
                        // orbital header).
                        if msg.contains("expected uses, expects, type, entity, trait, page")
                            && msg.contains("got String")
                        {
                            orbital_header_extra_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `(let day = @payload.day)` (run 39e: StreakTracker).
                        if msg.contains("unexpected token in s-expression: Equals") {
                            sexpr_equals_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // Object-style `key: value` props inside a JSX tag
                        // (run 40: ministral ReviewScheduler — `renderItem:`
                        // among `gap={sm}`-style attributes), with the
                        // JSX-in-lambda hoist teaching as the fallback.
                        if msg.contains("expected a JSX attribute name") {
                            if msg.contains("got Colon") {
                                jsx_attr_colon_hint(s, span.line as usize)
                            } else {
                                jsx_in_lambda_hint(s, span.line as usize, span.col as usize)
                            }
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // FLAT app file with no `orbital { … }` wrapper (run
                        // 39: granite ×2) — the surplus-brace variant hint
                        // misdirects when no orbital block exists at all.
                        if msg.contains("expected type, uses, or orbital") {
                            no_orbital_block_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `REVIEW_ COMPLETED -> …` — a space inside an event
                        // name (run 39: KnowledgeTracer, 5 identical validates).
                        if msg.contains("expected -> or") {
                            event_name_space_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `field : type = default` where defaults are illegal
                        // (emits payload — run 39). Blocks that allow defaults
                        // parse the `=`, so this error only fires where it is
                        // not allowed.
                        if msg.contains("expected identifier") && msg.contains("got Equals") {
                            field_default_not_allowed_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `recordObservation: (skillId: string) -> State` — a
                        // TypeScript function-type signature on an entity
                        // field (run 40b: StudySchedule).
                        if msg.contains("expected identifier") && msg.contains("got LParen") {
                            function_type_field_hint(s, span.line as usize)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // `private { fn(…) { … } }` — an invented methods
                        // block at trait level (run 39: KnowledgeTracer).
                        if msg.contains("expected state, initial, emits, listens, ticks, or config")
                        {
                            invented_trait_block_hint(s, &msg, span.line as usize)
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
                position: Some(SourcePosition::of(span, true)),
                needs_install: None,
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
                            "no type named {name:?} is declared — declare it at orbital level (`type {name} = {{ field : type … }}`), or use a primitive (string/number/boolean/scalar) or a declared type/entity name"
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
                TypeError::GenericTypeForbidden { name, .. } => (
                    "ORB_T_GENERIC_TYPE_DEPRECATED",
                    Some(format!(
                        "replace `{name}`: renderable content → `node`; structured data → an inline shape, a named `type` alias, or `Map K V`; a single primitive of unknown kind → `scalar`"
                    )),
                ),
                TypeError::PayloadSiblingUnresolved { sibling, .. } => (
                    "ELOLO_T_PAYLOAD_SIBLING",
                    Some(format!(
                        "declare a sibling field named {sibling:?} in the same struct, typed `event`"
                    )),
                ),
                TypeError::EventRefPosition { .. } => (
                    "ELOLO_T_EVENT_REF_POSITION",
                    Some(
                        "declare that position `event`, or write a plain string value there"
                            .to_string(),
                    ),
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
                position: Some(SourcePosition::of(span, true)),
                needs_install: None,
            }]
        }
        TypedParseError::Lolo(LoloError::Lower { inner, .. })
            if matches!(inner.innermost(), LowerError::LanguageData(_)) =>
        {
            let LowerError::LanguageData(e) = inner.innermost() else {
                unreachable!("guard matched LanguageData")
            };
            vec![crate::commands::validate::language_data_json_error(e)]
        }
        TypedParseError::Lolo(LoloError::Lower { path, inner })
            if matches!(
                inner.innermost(),
                LowerError::BehaviorPackageNotInstalled { .. }
            ) =>
        {
            vec![ValidationJsonError {
                code: "ORB_BEHAVIOR_PACKAGE_NOT_INSTALLED".to_string(),
                path: path.clone(),
                message: format!("{inner}"),
                suggestion: Some(
                    "Run `orb add <package>` for the package that declares this \
                 behavior, then retry — the schema was NOT evaluated."
                        .to_string(),
                ),
                fix_guidance: None,
                expected_shape: None,
                valid_values: None,
                position: None,
                needs_install: Some(true),
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
            position: inner
                .span()
                .map(|span| SourcePosition::of(span, inner.span_is_exact())),
            needs_install: None,
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

/// A lambda body wrapped in JS-style braces: `(fn item { (== …) })` — the
/// parser reads `{` as an object literal and dies on the `(` where a key
/// belongs (run 23: ReviewScheduler, 3 identical validates while the generic
/// object-literal hint suggested `(do …)`). A lambda body is ONE expression —
/// the fix is deleting the braces, not wrapping in `do`. Gated on `(fn ` /
/// `(lambda ` appearing on the error line before the error column.
fn braced_lambda_body_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    // The innermost open `{` at the error position is the brace that was
    // read as an object literal; the hint applies when ITS line opens a
    // lambda — covering both the same-line form and the multi-line
    // `(fn dayIdx {` + chained `(let …)` block form (run 23 both live).
    let (open, open_line, _) = innermost_open_delimiter(source, line, col)?;
    if open != '{' {
        return None;
    }
    let opener_text = source.lines().nth(open_line.checked_sub(1)?)?;
    if !opener_text.contains("(fn ") && !opener_text.contains("(lambda ") {
        return None;
    }
    Some((
        format!(
            "the `{{` on line {open_line} wraps the lambda body in braces — a lambda body is ONE bare expression, not a block"
        ),
        "delete the `{` and `}` around the body: `(fn item (== (object/get @item nextReviewDate) \"today\"))`. Intermediate values nest as `(let a … (let b … expr))`; a conditional body is the `(if cond thenExpr elseExpr)` expression directly; `(do …)` is only for effect sequences, never predicate/mapper bodies".to_string(),
    ))
}

/// `uses X from "path" { dateField: … }` — knobs written directly in the
/// import's `{ … }` body. The body takes ONE `config { … }` block (the
/// imported behavior's organism-wide knobs, orbital-import Stage A);
/// per-trait overrides still live on the trait REFERENCE.
fn uses_inline_config_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    if !line_text.trim_start().starts_with("uses ") {
        return None;
    }
    Some((
        "a `uses` line's `{ … }` body holds ONLY a `config { … }` block — knobs cannot sit directly inside it".to_string(),
        "wrap the knobs: `uses Name from \"std/behaviors/<name>\" { config { dateField: \"nextReviewDate\" } }` (organism-wide knobs), or override one trait at its reference site: `trait My = Name.traits.<TraitName> -> Entity { config { key: value … } }`".to_string(),
    ))
}

/// `orbital Name "1.0.0"` (a version string borrowed from the `app` header)
/// or a bare docstring line inside the orbital body (run 39e: granite ×14).
fn orbital_header_extra_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let t = line_text.trim();
    if t.starts_with("orbital ") && t.contains('"') {
        return Some((
            "`orbital` takes NO version string — the header is `orbital <Name> {` (only the `app` header carries a version)".to_string(),
            "delete the quoted string and open the body: `orbital <Name> {`".to_string(),
        ));
    }
    if t.starts_with('"') {
        return Some((
            "a bare prose string is not a declaration — description text is not part of the orbital body".to_string(),
            "delete the line or turn it into a `;;` comment".to_string(),
        ));
    }
    None
}

/// `(let day = @payload.day)` — an `=` inside an s-expression (run 39e:
/// StreakTracker). Bindings are positional; assignment is `(set …)`.
fn sexpr_equals_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    if line_text.contains("(let ") {
        return Some((
            "`(let name = value)` — a `let` binding is positional, there is no `=` inside s-expressions".to_string(),
            "write `(let name value bodyExpr)` — the binding is the first two arguments, the body is the rest".to_string(),
        ));
    }
    Some((
        "an `=` inside an s-expression — `.lolo` has no assignment operator; arguments are positional".to_string(),
        "delete the `=`; to mutate state use `(set target value)`, to bind a name use `(let name value body)`".to_string(),
    ))
}

/// A markdown `---` divider written into the file (run 39d: ReviewScheduler
/// laid the draft out like a document). Sibling of the ``` fence class.
fn markdown_divider_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let t = line_text.trim();
    if t.is_empty() || !t.chars().all(|c| c == '-') {
        return None;
    }
    Some((
        "this line is a markdown `---` divider — the file must contain raw `.lolo` source only, no document formatting".to_string(),
        "delete the `---` line (and any surrounding prose); commentary belongs in `;;` comments, and declarations live inside `orbital <Name> { … }`".to_string(),
    ))
}

/// `app Name "1.0"` followed by top-level `entity`/`trait` — a FLAT file
/// with no `orbital { … }` wrapper (run 39: granite ×2; the surplus-brace
/// hint sent the model re-counting braces that were never unbalanced).
/// Gated on NO orbital header anywhere above the error line.
fn no_orbital_block_hint(source: &str, line: usize) -> Option<(String, String)> {
    let has_orbital_above = source
        .lines()
        .take(line.saturating_sub(1))
        .any(|l| l.trim_start().starts_with("orbital "));
    if has_orbital_above {
        return None;
    }
    Some((
        "this file has NO `orbital <Name> { … }` block — `entity`, `trait`, and `page` declarations live INSIDE an orbital; only `type` and `orbital` declarations are allowed at file level".to_string(),
        "wrap the declarations in one block: add `orbital <Name> {` before the first `entity`/`trait` and a closing `}` at end of file (an `app` header line may stay above it)".to_string(),
    ))
}

/// `REVIEW_ COMPLETED -> …` — a space INSIDE an event name; the parser takes
/// the first word as the event and then expects `->`. Gated on the arm shape:
/// two or more ALL-CAPS words before a `->` on the error line.
fn event_name_space_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let arrow = line_text.find("->")?;
    let before = line_text[..arrow].trim();
    let words: Vec<&str> = before.split_whitespace().collect();
    let is_event_word = |w: &str| {
        w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    };
    if words.len() < 2 || !words.iter().all(|w| is_event_word(w)) {
        return None;
    }
    let joined = words.join("");
    Some((
        format!(
            "`{before}` — an event name is ONE identifier; the space splits it, so the parser expects `->` right after the first word"
        ),
        format!("join the words: `{joined} -> …` (and use the same joined name in `emits`/`listens`)"),
    ))
}

/// `masteryLevel : string = "…"` where a default is illegal (emits payload
/// blocks — run 39: KnowledgeTracer ×2). Entity fields and config knobs DO
/// take defaults and parse the `=`, so this error only fires where they are
/// not allowed. Gated on the `name : type = …` line shape.
fn field_default_not_allowed_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let t = line_text.trim();
    let colon = t.find(" : ")?;
    if !t[colon..].contains('=') {
        return None;
    }
    Some((
        "a default value (`= …`) is not allowed in this block — an event payload field is `name : type` only (defaults belong on entity fields and config knobs)".to_string(),
        "delete the `= …` after the type; the concrete value is supplied by the effect that emits the event".to_string(),
    ))
}

/// `private { updateBeta(a, b) { … } }` — an invented named block at trait
/// level (run 39: KnowledgeTracer wrote a JS-class methods block). Gated on
/// the error line being `<word> {` where `<word>` is the rejected token.
fn invented_trait_block_hint(source: &str, msg: &str, line: usize) -> Option<(String, String)> {
    let word = msg.split("got Identifier(\"").nth(1)?.split('"').next()?;
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    if line_text.trim() != format!("{word} {{") {
        return None;
    }
    Some((
        format!(
            "`{word} {{ … }}` is not a trait block — a trait body allows only `state`, `initial`, `emits`, `listens`, `ticks`, and `config`; `.lolo` has NO function/method definitions"
        ),
        "delete the whole block and write each computation inline in the transition's effects as prefix s-expressions (e.g. `(set @entity.x (+ @entity.x 1))`)".to_string(),
    ))
}

/// `{ "Completed review today" }` — an object whose entry is a bare quoted
/// string: the string parses as the KEY, then `:` is expected. Gated on the
/// text before the error column ending with a closing quote.
/// An effect `(…)` under a routed `listens` entry — the enclosing block is
/// `listens { … }`, so the "state closed too early" teaching misdirects.
/// Listens entries only NAME events; routing + effects live in state arms.
fn effect_in_listens_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let in_listens = innermost_open_delimiter(source, line, col)
        .and_then(|(_, l, _)| source.lines().nth(l.saturating_sub(1)))
        .is_some_and(|opener| opener.trim_start().starts_with("listens"));
    if !in_listens {
        return None;
    }
    Some((
        "effects under a `listens` entry — a `listens` entry only NAMES the event (optionally with `{ with/guard }` refinements); it takes no `-> target` routing and no effect body".to_string(),
        "keep the entry bare (`Orbital.EVENT`), then handle it in a state: `EVENT -> target` inside `state <name> { … }`, with the effects under that arm".to_string(),
    ))
}

/// `recordObservation: (skillId: string, …) -> KnowledgeState` — a TypeScript
/// function-type signature on an entity field. Entities hold DATA; behavior
/// lives in traits. Gated on the `name : (` / `name: (` line shape.
fn function_type_field_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let t = line_text.trim();
    let after_colon = t.split_once(':').map(|(_, rest)| rest.trim_start())?;
    if !after_colon.starts_with('(') {
        return None;
    }
    Some((
        "this field declares a FUNCTION type — entity fields hold data (`name : string`, `count : number`, `[Type]`, …); `.lolo` has no function/method types".to_string(),
        "delete the signature; the behavior belongs in a trait — model the call as an event with a payload (`emits`/`listens`) and its logic as transition effects".to_string(),
    ))
}

/// `<UiStack …>` where an object entry/expression is expected — JSX markup
/// in expression position. Inside a lambda the hoist teaching applies;
/// elsewhere the raw object form is the fix. Gated on the text before the
/// error column ending with `<`.
fn jsx_markup_in_object_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let before: String = line_text.chars().take(col.checked_sub(1)?).collect();
    if !before.trim_end().ends_with('<') {
        return None;
    }
    jsx_in_lambda_hint(source, line, col).or(Some((
        "`<Tag …>` markup where an object entry is expected — this render position takes the raw object form, not JSX".to_string(),
        "rewrite the markup as an object: `{ type: stack, direction: vertical, children: [ … ] }`".to_string(),
    )))
}

/// Object-style `key: value` props inside a JSX tag — attributes switch
/// syntax mid-tag (`gap={sm}` … then `renderItem: (fn …)`); JSX props are
/// `name={expr}` / `name="str"`, never `name: value` (run 40: ministral).
fn jsx_attr_colon_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let name = line_text.trim_start().split(':').next()?.trim().to_string();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((
        format!("`{name}: …` is OBJECT syntax inside a JSX tag — JSX props are written `{name}={{value}}` (or `{name}=\"text\"`)"),
        format!("wrap the value in braces: `{name}={{…}}` — or rewrite the whole component in the raw object form (`{{ type: …, {name}: … }}`), which is also the only correct shape for lambdas that render per-item content"),
    ))
}

/// JSX inside a lambda body (`renderItem: (fn item { <UiStack> … })`) — JSX
/// hoists out of the lambda and strands `@item`, and a lambda body is one
/// bare expression anyway. The raw `type:` object form is the correct shape
/// inside map/filter/render lambdas. Gated on the innermost open delimiter
/// at the error position belonging to a lambda.
fn jsx_in_lambda_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let (_, open_line, _) = innermost_open_delimiter(source, line, col)?;
    let opener_text = source.lines().nth(open_line.checked_sub(1)?)?;
    if !opener_text.contains("(fn ") && !opener_text.contains("(lambda ") {
        return None;
    }
    Some((
        "JSX inside a lambda body — JSX hoists out of the lambda and strands `@item`/the lambda parameter; inside map/filter/render lambdas the raw object form is the correct shape".to_string(),
        "replace the `<Tag …>` markup with the object form: `(fn item { type: stack, children: [{ type: typography, content: @item.title }] })` — no `<`/`>` tags inside lambdas".to_string(),
    ))
}

/// `key: value + 1` — an infix operator continuing an object value; the
/// operator token parses as the NEXT key, so the error lands on its `:`
/// expect. Gated on the text before the error column ending with an
/// operator character.
fn infix_in_object_value_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let before: String = line_text.chars().take(col.checked_sub(1)?).collect();
    let last = before.trim_end().chars().last()?;
    if !matches!(last, '+' | '-' | '*' | '%') {
        return None;
    }
    Some((
        format!("the `{last}` continues the previous value as an infix expression — `.lolo` has NO infix syntax; a value is ONE expression and operators are prefix calls"),
        "wrap the whole value in a prefix call: `streakCount: (+ @entity.currentStreak 1)` — never `a + 1`".to_string(),
    ))
}

fn keyless_object_entry_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let before: String = line_text.chars().take(col.checked_sub(1)?).collect();
    if !before.trim_end().ends_with('"') {
        return None;
    }
    Some((
        "this object entry is a bare quoted string — object entries are `key: value`, so the string was read as a KEY with no value".to_string(),
        "give it a key (`{ text: \"…\" }`), or if you meant a plain list of strings drop the braces: `[ \"…\" ]`".to_string(),
    ))
}

/// `field : map[date, number]` / `subjects : array[string]` — generic-style
/// type syntax. Arrays are `[ElementType]`; key-value maps are `Map K V`.
fn generic_type_syntax_hint(source: &str, line: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let after_colon = line_text.split(" : ").nth(1)?;
    let head: String = after_colon
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .collect();
    if head.is_empty() || !after_colon[head.len()..].starts_with('[') {
        return None;
    }
    Some((
        format!(
            "`{head}[…]` is not `.lolo` type syntax — arrays are written `[ElementType]` (brackets AROUND the element) and key-value maps are `Map KeyType ValueType`"
        ),
        "rewrite the annotation, e.g. `subjects : [string]` or `reviewCounts : Map date number`".to_string(),
    ))
}

/// An operator head written YAML-style with a colon: `(emit: EVENT { … })`
/// (run 36: TopicManager — "unexpected token in s-expression: Colon" named
/// neither the head nor the fix). Gated on the error column pointing at a
/// `:` that directly follows `(<head>`.
fn operator_head_colon_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let line_text = source.lines().nth(line.checked_sub(1)?)?;
    let before: String = line_text.chars().take(col.checked_sub(1)?).collect();
    let head: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '/' || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if head.is_empty() {
        return None;
    }
    let before_head = &before[..before.len() - head.len()];
    if !before_head.trim_end().ends_with('(') {
        return None;
    }
    Some((
        format!("`({head}: …)` — an s-expression head takes NO colon; the head is the operator, the rest are its arguments"),
        format!("delete the `:` after `{head}`: `({head} arg1 arg2 …)`"),
    ))
}

/// A `(` used to open an OBJECT body: `(fn item (` followed by `key: value`
/// lines (run 22: CalendarHeatmap, 3 identical validates — the generic "bare
/// `( … )` is not a grouping block / write `(do …)`" hint misdirects when the
/// body is a component object). Gated on the error line ending with `(` and
/// the next non-blank line being a `key: value` member.
fn paren_should_be_brace_hint(source: &str, line: usize) -> Option<(String, String)> {
    let mut lines = source.lines().skip(line.checked_sub(1)?);
    let err_line = lines.next()?;
    if !err_line.trim_end().ends_with('(') {
        return None;
    }
    let next = lines.find(|l| !l.trim().is_empty())?;
    let trimmed = next.trim_start();
    let ident_len = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .count();
    if ident_len > 0 && trimmed[ident_len..].trim_start().starts_with(':') {
        return Some((
            "the trailing `(` on this line opens an s-expression, but what follows is `key: value` members — an OBJECT body, which is written with `{ … }`".to_string(),
            "replace that `(` with `{` (and its matching closer with `}`): e.g. `(fn item { type: badge … })`, never `(fn item ( type: badge … ))`".to_string(),
        ));
    }
    // `(fn group (` followed by effect calls — a paren-wrapped body block
    // (run 24: TopicManager nested `(do (render-ui …))` inside a map lambda).
    if (err_line.contains("(fn ") || err_line.contains("(lambda ")) && trimmed.starts_with('(') {
        return Some((
            "the trailing `(` opens a body BLOCK — a lambda takes ONE bare expression, no wrapper parens: `(fn group <expr>)`".to_string(),
            "inside a render tree, the mapper's expression is a component OBJECT like `{ type: stack, children: [ … ] }` — effects (`render-ui`, `set`, `emit`, …) can NEVER appear inside a tree; delete the wrapper `(` and return one object".to_string(),
        ));
    }
    None
}

/// An ORBITAL-level keyword (`page`, `entity`, `trait`, …) reached while the
/// parser still expects TRAIT members: a `}` is MISSING above, so the trait
/// (or a state inside it) swallowed the rest of the file (run 22: Dashboard,
/// 3 identical validates while the catch-all "inside a state body…" hint told
/// the wrong story). The delimiter replay names the block left open.
fn orbital_keyword_inside_trait_hint(
    source: &str,
    msg: &str,
    line: usize,
    col: usize,
) -> Option<(String, String)> {
    const ORBITAL_KEYWORDS: [&str; 7] =
        ["page", "entity", "trait", "theme", "service", "type", "use"];
    let keyword = ORBITAL_KEYWORDS
        .iter()
        .find(|k| msg.contains(&format!("got Identifier(\"{k}\")")))?;
    let (open, open_line, open_col) = innermost_open_delimiter(source, line, col)?;
    Some((
        format!(
            "`{keyword}` is an ORBITAL-level declaration, but the parser is still INSIDE the block opened at line {open_line}:{open_col} (`{open}` never closed) — a `}}` is missing above this line"
        ),
        format!(
            "add the missing `}}` for the `{open}` opened at line {open_line} so the trait closes before the `{keyword}` declaration"
        ),
    ))
}

/// A bare effect keyword where an object KEY was expected — the model wrote
/// an effect without parens inside a UI tree (`render-ui main { … }` as a
/// child of a `children:` array; run 21: StreakTracker burned 2 sessions on
/// it). Two mistakes stack: effects are `(effect …)` calls, AND an effect can
/// never appear inside a `children:` array at all — children take component
/// objects. Gated on the error line starting with a known effect head.
/// A second `@…` binding where the next object key belongs — the prop's
/// value ended at its first expression and the model kept writing (run 26:
/// ReviewScheduler wrote `content: @item.interval + " days"`, infix
/// concatenation; 3 identical validates). When the line carries an infix
/// `+`, teach the prefix `str/concat` form specifically.
fn value_continued_past_first_term_hint(source: &str, line: usize) -> Option<(String, String)> {
    let text = source.lines().nth(line.checked_sub(1)?)?;
    if text.contains(" + ") {
        Some((
            "a prop value is ONE expression — the `+` after the binding is infix syntax, which does not exist in `.lolo`".to_string(),
            "combine values with a prefix call instead: `content: (str/concat @item.interval \" days\")` — never `@item.interval + \" days\"`".to_string(),
        ))
    } else {
        Some((
            "a second `@…` binding appears where the next `key:` belongs — the previous prop's value already ended at its first expression".to_string(),
            "each prop takes exactly ONE expression; to combine values use a prefix call like `(str/concat a b)`, and separate props as `key: value` pairs".to_string(),
        ))
    }
}

const EFFECT_HEADS: [&str; 11] = [
    "render-ui",
    "set",
    "emit",
    "persist",
    "fetch",
    "fetch-stream",
    "navigate",
    "notify",
    "log",
    "call-service",
    "send-server",
];

/// A bare effect keyword where TRAIT members are expected — `render-ui main {`
/// standing alone at trait/state level with no parens (run 25: TopicLibrary,
/// 3 identical validates under the generic state-body catch-all). Message-
/// gated: the offending Identifier IS a known effect head.
fn parenless_effect_at_trait_level_hint(msg: &str) -> Option<(String, String)> {
    let head = EFFECT_HEADS
        .iter()
        .find(|h| msg.contains(&format!("got Identifier(\"{h}\")")))?;
    Some((
        format!(
            "`{head}` is an EFFECT — it is written as a parenthesized call `({head} …)` and can only appear under a transition arm, never standing alone"
        ),
        format!(
            "move this `{head}` under the transition that should run it, as `EVENT -> target` followed by `({head} …)` — with the parens"
        ),
    ))
}

fn parenless_effect_in_tree_hint(source: &str, line: usize) -> Option<(String, String)> {
    let text = source.lines().nth(line.checked_sub(1)?)?;
    let trimmed = text.trim_start();
    let head = EFFECT_HEADS.iter().find(|h| {
        trimmed
            .strip_prefix(*h)
            .is_some_and(|rest| rest.starts_with([' ', '\t']))
    })?;
    Some((
        format!(
            "`{head}` is an EFFECT — it is written as a parenthesized call `({head} …)`, never as a bare `{head} …` line"
        ),
        format!(
            "effects also cannot sit inside a `children:` array — children take component objects like `{{ type: … }}`. Move the `{head}` effect up to a transition arm (as `({head} …)`), and put a component object here instead"
        ),
    ))
}

/// A `]`/`}` arrived where an s-expression element was expected — the closer
/// pile-up shape (`}]` / `}])`) the model cannot untangle by eye (Run 20:
/// ReviewScheduler burned 4+ validates on one such line with no guidance).
/// Deterministic aid: replay the delimiter stack up to the error position
/// (skipping strings and `;;` comments) and NAME the innermost still-open
/// delimiter — the culprit the offending closer is colliding with.
fn unbalanced_closer_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let (open, open_line, open_col) = innermost_open_delimiter(source, line, col)?;
    let closer = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        _ => return None,
    };
    Some((
        format!(
            "the innermost unclosed delimiter at this point is `{open}` opened at line {open_line}:{open_col} — it is still missing its `{closer}`, so the closer here collides with it"
        ),
        format!(
            "add the missing `{closer}` for the `{open}` opened at line {open_line} (or delete a surplus closer between line {open_line} and here) — fix the balance THERE, not on this line"
        ),
    ))
}

/// An infix comparison written inside a call's argument list, caught at the
/// PARSER's arity check — `(if @entity.currentStreak > 0 "a" "b")` is 5
/// arguments (run 27: StreakTracker, 3 identical validates while the generic
/// `(do …)` guidance misdirected). Rebuild the prefix form from the line's
/// own pieces so the fix is copy-pasteable.
fn infix_in_call_line_hint(source: &str, line: usize) -> Option<(String, String)> {
    const INFIX_TOKENS: [&str; 8] = [">=", "<=", "==", "!=", ">", "<", "+", "-"];
    let text = source.lines().nth(line.checked_sub(1)?)?;
    let open = text.find('(')?;
    let inner = &text[open + 1..];
    let mut parts = inner.split_whitespace();
    let head = parts.next()?;
    let lhs = parts.next()?;
    let op = parts.next()?;
    let rhs = parts.next()?;
    if !INFIX_TOKENS.contains(&op) || lhs.starts_with('(') || rhs.starts_with('(') {
        return None;
    }
    Some((
        format!(
            "an infix `{op}` appears inside `({head} …)`'s argument list — comparisons/arithmetic are PREFIX calls, so `{lhs} {op} {rhs}` reads as three separate arguments"
        ),
        format!(
            "wrap it: `({head} ({op} {lhs} {rhs}) …)` — never `({head} {lhs} {op} {rhs} …)`"
        ),
    ))
}

/// A stray `)`/`]` falling through to trait level: the model faces a closer
/// pile-up (run 24: StreakTracker's `))))))` on one line, 3 identical
/// validates with only "re-count parens" guidance). The replay names the
/// FIRST closer that had nothing to close — the exact one to delete. Scans
/// one column PAST the error so the reported token itself is checked too.
fn surplus_closer_hint(source: &str, line: usize, col: usize) -> Option<(String, String)> {
    let (_, mismatch) = replay_delimiters(source, line, col + 1);
    let (c, m_line, m_col) = mismatch?;
    Some((
        format!(
            "the first surplus closer is `{c}` at line {m_line}:{m_col} — at that point nothing is open for it to close"
        ),
        format!(
            "delete the `{c}` at line {m_line}:{m_col} (every closer before it already matched its opener). If an effect there LOOKS unfinished, the missing piece is an opener above, not more closers"
        ),
    ))
}

/// Replay `(`/`[`/`{` nesting from the top of `source` to just before
/// (line, col), skipping string literals and `;;` comments. Returns the
/// delimiter stack at that point plus the FIRST closer that did not match
/// the stack top (a surplus or wrong-kind closer — the first structural
/// anomaly in the file). Mismatched closers do NOT pop (only a matching
/// closer does), so the stale opener stays on top — which is exactly the
/// culprit to report.
fn replay_delimiters(
    source: &str,
    err_line: usize,
    err_col: usize,
) -> (Vec<(char, usize, usize)>, Option<(char, usize, usize)>) {
    let mut stack: Vec<(char, usize, usize)> = Vec::new();
    let mut first_mismatch: Option<(char, usize, usize)> = None;
    let record_mismatch = |c: char, l: usize, co: usize, fm: &mut Option<(char, usize, usize)>| {
        if fm.is_none() {
            *fm = Some((c, l, co));
        }
    };
    for (li, text) in source.lines().enumerate() {
        let lineno = li + 1;
        let mut in_str = false;
        let mut escaped = false;
        let mut prev_semi = false;
        for (ci, c) in text.chars().enumerate() {
            let col = ci + 1;
            if lineno == err_line && col >= err_col {
                return (stack, first_mismatch);
            }
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_str = false;
                }
                prev_semi = false;
                continue;
            }
            if c == ';' && prev_semi {
                break; // `;;` comment — rest of line is prose
            }
            prev_semi = c == ';';
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => stack.push((c, lineno, col)),
                ')' if matches!(stack.last(), Some(('(', _, _))) => {
                    stack.pop();
                }
                ']' if matches!(stack.last(), Some(('[', _, _))) => {
                    stack.pop();
                }
                '}' if matches!(stack.last(), Some(('{', _, _))) => {
                    stack.pop();
                }
                ')' | ']' | '}' => record_mismatch(c, lineno, col, &mut first_mismatch),
                _ => {}
            }
        }
        if lineno >= err_line {
            return (stack, first_mismatch);
        }
    }
    (stack, first_mismatch)
}

fn innermost_open_delimiter(
    source: &str,
    err_line: usize,
    err_col: usize,
) -> Option<(char, usize, usize)> {
    replay_delimiters(source, err_line, err_col)
        .0
        .last()
        .copied()
}
