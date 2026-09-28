//! `orb validate --json` places every `.lolo` diagnostic on the exact source
//! span an editor underlines: 1-based `line` / `column`, and `endColumn` just
//! past the last character (`endLine` for the line it ends on). Each fixture
//! marks the span it expects with `⟦…⟧`; the markers are stripped before
//! validating. A JSX prop's position is its whole `name={value}` attribute
//! (one IR pointer covers both). A position borrowed from an enclosing construct (the source
//! map's ancestor fallback) says so with `approximate: true`.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

struct Marked {
    source: String,
    line: u64,
    column: u64,
    end_column: u64,
}

/// Strip one `⟦…⟧` pair; its position is the expected span (1-based, end exclusive).
fn marked(src: &str) -> Marked {
    let open = src.find('⟦').expect("fixture has ⟦");
    let close = src.find('⟧').expect("fixture has ⟧");
    let before = &src[..open];
    let inner = &src[open + '⟦'.len_utf8()..close];
    let line = before.matches('\n').count() as u64 + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = before[line_start..].chars().count() as u64 + 1;
    Marked {
        source: format!("{before}{inner}{}", &src[close + '⟧'.len_utf8()..]),
        line,
        column,
        end_column: column + inner.chars().count() as u64,
    }
}

fn validate(name: &str, source: &str) -> serde_json::Value {
    let path = std::env::temp_dir().join(format!(
        "orb-diag-span-{}-{name}-{}.lolo",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, source).expect("write fixture");
    let out = Command::new(orb_bin())
        .args(["validate", "--json", path.to_str().unwrap()])
        .output()
        .expect("spawn orb");
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "validate --json is not JSON ({e}); stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn errors(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("errors")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The note block, with `render` as INIT's render tree and `rename` as RENAME's effects.
fn note(render: &str, rename: &str) -> String {
    format!(
        r#"app note "1.0.0"
orbital NoteOrbital {{
  entity Note [runtime] {{
    id : string!
    name : string
  }}

  trait NoteView -> Note [interaction, instance] {{
    initial: idle
    state idle {{
      INIT -> idle
        (render-ui main {render})
      RENAME -> idle
        {rename}
    }}
    listens {{
      RENAME {{ name: string }}
    }}
  }}
  page "/note" -> NoteView
}}
"#
    )
}

fn assert_span(case: &str, fixture: &str, code: &str) {
    let m = marked(fixture);
    let v = validate(case, &m.source);
    let all = errors(&v);
    let e = all
        .iter()
        .find(|e| e["code"] == code)
        .unwrap_or_else(|| panic!("{case}: expected {code}, got {all:#?}"));
    let got = (
        e["line"].as_u64(),
        e["column"].as_u64(),
        e["endLine"].as_u64(),
        e["endColumn"].as_u64(),
    );
    assert_eq!(
        got,
        (
            Some(m.line),
            Some(m.column),
            Some(m.line),
            Some(m.end_column)
        ),
        "{case}: {code} must sit exactly on the marked span; error = {e:#}"
    );
    assert!(
        e.get("approximate").is_none_or(|a| a == false),
        "{case}: an exact span must not be flagged approximate; error = {e:#}"
    );
}

#[test]
fn control_a_clean_block_has_no_errors() {
    let v = validate(
        "clean",
        &note(
            "<Typography content={@entity.name} />",
            "(set @entity.name ?name)",
        ),
    );
    assert_eq!(errors(&v), Vec::<serde_json::Value>::new());
}

#[test]
fn a_parse_error_sits_on_the_unexpected_token() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name)",
    )
    .replace("    name : string\n", "    name : string\n    ⟦oops⟧\n");
    assert_span("parse", &src, "LOLO_PARSE_ERROR");
}

#[test]
fn an_unknown_entity_field_sits_on_the_whole_binding_sigil_included() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name) (set ⟦@entity.title⟧ ?name)",
    );
    assert_span("field", &src, "ORB_BINDING_ENTITY_FIELD_NOT_FOUND");
}

#[test]
fn an_unknown_pattern_sits_on_its_tag_name_not_the_render_effect() {
    let src = note(
        "<⟦Typograhpy⟧ content={@entity.name} />",
        "(set @entity.name ?name)",
    );
    assert_span("pattern", &src, "ORB_RUI_INVALID_PATTERN");
}

#[test]
fn an_unknown_prop_sits_on_the_whole_attribute() {
    let src = note(
        "<Typography ⟦contnt={@entity.name}⟧ />",
        "(set @entity.name ?name)",
    );
    assert_span("prop", &src, "ORB_RUI_INVALID_PROP");
}

#[test]
fn a_nested_child_error_sits_inside_the_child() {
    let src = note(
        "<Stack direction={vertical}>\n          <⟦Typograhpy⟧ content={@entity.name} />\n        </Stack>",
        "(set @entity.name ?name)",
    );
    assert_span("child", &src, "ORB_RUI_INVALID_PATTERN");
}

#[test]
fn a_read_never_set_sits_on_the_attribute_that_first_reads_it() {
    let src = note(
        "<Typography ⟦content={@entity.nick}⟧ />",
        "(set @entity.name ?name)",
    );
    assert_span("never-set", &src, "LOLO_LOWER_ERROR");
}

#[test]
fn edge_an_unknown_effect_sits_on_its_operator() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name) (⟦sett⟧ @entity.name ?name)",
    );
    assert_span("effect", &src, "ORB_EFF_UNKNOWN_TYPE");
}

#[test]
fn edge_a_second_line_error_keeps_its_own_line_and_a_multibyte_line_counts_chars() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name \"— é\") (set ⟦@entity.nope⟧ ?name)",
    );
    assert_span("multibyte", &src, "ORB_BINDING_ENTITY_FIELD_NOT_FOUND");
}

#[test]
fn edge_an_error_with_no_token_of_its_own_borrows_the_enclosing_transition_and_says_so() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name)",
    )
    .replace("RENAME -> idle", "RENAME -> nowhere");
    let v = validate("approx", &src);
    let all = errors(&v);
    let e = all
        .iter()
        .find(|e| e["code"] == "ORB_T_INVALID_TRANSITION")
        .unwrap_or_else(|| panic!("expected ORB_T_INVALID_TRANSITION, got {all:#?}"));
    let transition_line = src
        .lines()
        .position(|l| l.contains("RENAME -> nowhere"))
        .unwrap() as u64
        + 1;
    assert_eq!(e["line"].as_u64(), Some(transition_line), "{e:#}");
    assert_eq!(e["approximate"], true, "{e:#}");
}

#[test]
fn a_trait_bound_to_a_missing_entity_sits_on_the_entity_name() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name)",
    )
    .replace("trait NoteView -> Note [", "trait NoteView -> ⟦Nope⟧ [");
    assert_span("linked-entity", &src, "ORB_E_INVALID_RELATION");
}

#[test]
fn edge_a_rebindable_binding_sits_on_the_entity_name_not_the_marker() {
    let src = note(
        "<Typography content={@entity.name} />",
        "(set @entity.name ?name)",
    )
    .replace(
        "trait NoteView -> Note [",
        "trait NoteView -> @rebindable ⟦Nope⟧ [",
    );
    assert_span("linked-entity-rebindable", &src, "ORB_E_INVALID_RELATION");
}
