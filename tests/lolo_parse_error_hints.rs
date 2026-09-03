//! Integration tests for teachable `LOLO_PARSE_ERROR` hints.
//!
//! Verifies that `orb validate --json` surfaces `suggestion`, `fixGuidance`,
//! `line`, and `column` for the common `.lolo` syntax mistakes the LLM makes.

use std::path::PathBuf;
use std::process::Command;

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn validate_lolo(lolo: &str, name: &str) -> serde_json::Value {
    let tmp = std::env::temp_dir().join(format!(
        "orb-lolo-hint-test-{}-{}-{}.lolo",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, lolo).expect("write temp lolo");

    let output = Command::new(orb_bin())
        .args(["validate", "--json", tmp.to_str().unwrap()])
        .output()
        .expect("failed to spawn orb binary");

    assert!(
        output.status.success(),
        "orb validate exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("validate --json output is not valid JSON");

    // Best-effort cleanup; ignore failures.
    let _ = std::fs::remove_file(&tmp);

    result
}

fn first_parse_error(result: &serde_json::Value) -> &serde_json::Value {
    result
        .get("errors")
        .and_then(|v| v.as_array())
        .expect("errors array missing")
        .first()
        .expect("errors array empty")
}

#[test]
fn semicolon_comment_gets_hint() {
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (set @entity.id "a") ; oops
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "semicolon");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"].as_str().unwrap().contains("`;;`"));
    assert!(err["fixGuidance"].as_str().unwrap().contains("stray `;`"));
    assert_eq!(err["line"], 7);
    assert!(err["column"].as_u64().unwrap() > 0);
}

#[test]
fn lex_error_inside_slash_comment_gets_comment_hint() {
    // The lexer dies on the apostrophe INSIDE the comment prose; the
    // single-quote hint ("strings are double-quoted") misdirects — the
    // line-grounded `//`-comment hint must win (C-COMMENT-SLASH lex shape).
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      // We'll assume the graph can handle it.
      INIT -> idle
        (set @entity.id "a")
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "slash-comment-lex");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"].as_str().unwrap().contains("`;;`"));
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("replace the leading `//` with `;;`"));
    assert_eq!(err["line"], 6);
}

#[test]
fn js_call_syntax_in_object_value_gets_prefix_operator_hint() {
    // `id: uuid(), timestamp: now()` — JS call syntax in value position. The
    // parser reports "object key expected, got LParen", whose default hint
    // (brace-lambda-body) misdirects; the identifier char glued to the `(`
    // disambiguates (C-CALL-SYNTAX).
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (persist create X { id: uuid(), ts: now() } {
          emit: { success: Saved }
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "js-call-syntax");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("prefix s-expressions"));
    assert!(err["fixGuidance"].as_str().unwrap().contains("(time/now)"));
}

#[test]
fn colon_after_unclosed_paren_gets_swallowed_block_hint() {
    // The object value's s-expression is missing its `)`, so the parser
    // consumes the next `key: value` line and dies on its colon — the victim
    // line, not the culprit (Run 19). (With a `}` between, UnclosedDelimiter
    // fires instead — that shape has its own hint.)
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (render-ui main {
          type: stack
          gap: (array/len @entity.id
          title: "Edit Topic"
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "swallowed-block");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("missing its `)`"));
    assert!(err["fixGuidance"].as_str().unwrap().contains("ABOVE"));
}

#[test]
fn closer_collision_in_array_names_the_unclosed_opener() {
    // `(if …` in an array element missing its `)` — the parser consumes both
    // branch objects legally and then collides with the array's `]`:
    // "unexpected token in s-expression: RBracket". Run 20: ReviewScheduler
    // burned 4+ validates on this shape with NO guidance; the delimiter-stack
    // replay must name the culprit `(` line, not the collision line.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
    ok : boolean
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (render-ui main {
          type: stack
          children: [
            (if @entity.ok { type: badge label: "a" } { type: badge label: "b" }
          ]
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "closer-collision");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("`(` opened at line 13"));
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("fix the balance THERE"));
}

#[test]
fn closer_collision_via_rbrace_names_unclosed_array_opener() {
    // Run 21 (ReviewScheduler, RParen variant verified live): a `children: [`
    // missing its `]` makes the next closer collide inside the s-expression.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (render-ui main {
          type: stack
          children: [
            { type: typography content: "a" }
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "rbrace-collision");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("`[` opened at line 11"));
}

#[test]
fn effect_stranded_at_trait_level_names_surplus_brace() {
    // Run 21 (TopicManager, 8+ identical validates): a surplus `}` ejects the
    // effect from its state block; the old catch-all hint told the model to
    // rewrite the state body instead of deleting the `}`.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        }
        (render-ui main { type: stack children: [] })
    }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "lparen-trait-level");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("closed its `state { … }` block too early"));
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("delete the surplus `}`"));
}

#[test]
fn parenless_effect_in_children_gets_call_syntax_hint() {
    // Run 21 (StreakTracker, 2 sessions): `render-ui side { … }` written as a
    // bare line inside a `children:` array — effects are parenthesized calls
    // and can never nest in children at all.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (render-ui main {
          type: stack
          children: [{
            render-ui side { type: box }
          }]
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "parenless-effect");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("`render-ui` is an EFFECT"));
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("cannot sit inside a `children:` array"));
}

#[test]
fn comma_in_children_gets_hint() {
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
    ok : boolean
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (set @entity.ok false)
        (render-ui main {
          type: stack
          children: [
            { type: typography content: "a" }
            , (if @entity.ok { type: typography content: "b" } { type: typography content: "c" })
          ]
        })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "comma");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"].as_str().unwrap().contains("comma"));
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("remove the `,`"));
    assert!(err["line"].as_u64().unwrap() > 0);
}

#[test]
fn array_type_bang_gets_hint() {
    let lolo = r#"type Q = { options : [string!]! }
orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      INIT -> idle
        (render-ui main { type: stack children: [] })
    }
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "bang");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"]
        .as_str()
        .unwrap()
        .contains("[ElementType]"));
    assert!(err["fixGuidance"].as_str().unwrap().contains("[string]"));
    assert_eq!(err["line"], 1);
}

#[test]
fn when_guard_gets_hint() {
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
    n : number
  }
  trait T -> X [interaction] {
    initial: a
    state a {
      INIT -> a
        (set @entity.n 0)
      NEXT -> b
        when (> @entity.n 0)
    }
    state b {}
  }
  page "/" as P -> T
}"#;

    let result = validate_lolo(lolo, "when");
    let err = first_parse_error(&result);
    assert_eq!(err["code"], "LOLO_PARSE_ERROR");
    assert!(err["suggestion"].as_str().unwrap().contains("`when`"));
    assert!(err["fixGuidance"].as_str().unwrap().contains("guard-expr"));
    assert!(err["line"].as_u64().unwrap() > 0);
}

#[test]
fn payload_keyword_in_emits_gets_hint() {
    // run 33 (ActivityHeatmap): `payload: { … }` under an emits entry — the
    // old silent loop-break surfaced a trait-level keyword error instead.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (emit DAY_SELECTED { date: @now })
    }
    emits {
      DAY_SELECTED
        @description "day picked"
        payload: {
          date : datetime
        }
    }
  }
}
"#;
    let result = validate_lolo(lolo, "emits-payload-keyword");
    let err = first_parse_error(&result);
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("inside `emits") && msg.contains("payload"),
        "message must name the emits block and the payload token: {msg}"
    );
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("no `payload:` keyword"),
        "suggestion must teach the brace-block form: {sugg}"
    );
    assert!(err["line"].as_u64().is_some(), "line must be present");
}

#[test]
fn tier_word_as_emit_route_gets_hint() {
    // run 33 (TopicManager): `ADD_TOPIC -> presentation` — @tier vocabulary
    // written as a route; the bare token list never converted the model.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
    emits {
      ADD_TOPIC -> presentation
    }
  }
}
"#;
    let result = validate_lolo(lolo, "emit-route-tier");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("TIER annotations"),
        "suggestion must name the tier/route conflation: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("@tier \"presentation\""),
        "fixGuidance must show the concrete @tier rewrite: {fix}"
    );
}

#[test]
fn operator_head_colon_gets_hint() {
    // run 36 (TopicManager): `(emit: REVIEW_COMPLETED { … })` — YAML-style
    // colon on the s-expression head.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (emit: DONE_EVENT {topic: @entity.id})
    }
    emits {
      DONE_EVENT
    }
  }
}
"#;
    let result = validate_lolo(lolo, "operator-head-colon");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("head takes NO colon"),
        "suggestion must name the head-colon mistake: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("delete the `:` after `emit`"),
        "fixGuidance must name the concrete head: {fix}"
    );
}

#[test]
fn uses_inline_config_gets_hint() {
    // run 36 (ReviewScheduler): `uses X from "path" { dateField: … }` — the
    // model gave up on this shape across three repair respawns.
    let lolo = r#"orbital X {
  uses ReminderScheduler from "std/behaviors/std-reminder-scheduler" { dateField: "nextReviewDate" }
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
  }
}
"#;
    let result = validate_lolo(lolo, "uses-inline-config");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("ONLY the import"),
        "suggestion must name the uses-line rule: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("reference site"),
        "fixGuidance must point at the reference-site override: {fix}"
    );
}

#[test]
fn generic_type_syntax_gets_hint() {
    // run 38 (ReviewHeatmap): `reviewCounts : map[date, number]`.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
    reviewCounts : map[date, number]
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
  }
}
"#;
    let result = validate_lolo(lolo, "generic-type-syntax");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("`map[…]` is not `.lolo` type syntax")
            && sugg.contains("Map KeyType ValueType"),
        "suggestion must teach both array and Map forms: {sugg}"
    );
}

#[test]
fn keyless_object_entry_gets_hint() {
    // run 38 (StudyStreak): `[{ "Completed review today" }]` — the quoted
    // string parses as a KEY; the generic quoting hint misdirected.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string!
    recentActivity : [any] = []
  }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (set @entity.recentActivity (array/concat [{ "Completed review today" }] @entity.recentActivity))
    }
  }
}
"#;
    let result = validate_lolo(lolo, "keyless-object-entry");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("bare quoted string"),
        "suggestion must name the keyless-entry mistake: {sugg}"
    );
}

#[test]
fn flat_app_file_without_orbital_gets_hint() {
    // run 39 (StudyStreak, granite): `app` header + top-level `entity` —
    // the surplus-brace hint misdirected (no braces were unbalanced).
    let lolo = r#"app StudyStreak "1.0.0"

entity StudyStreak [runtime] {
  streakCount : number = 0
}
"#;
    let result = validate_lolo(lolo, "flat-app-no-orbital");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("NO `orbital <Name> { … }` block"),
        "suggestion must name the missing orbital wrapper: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("orbital <Name> {"),
        "fixGuidance must show the wrapper: {fix}"
    );
}

#[test]
fn stranded_declaration_with_orbital_above_keeps_brace_hint() {
    // The surplus-brace teaching must survive when an orbital header DOES
    // exist above (the original C-EARLY-ORBITAL-CLOSE shape).
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
}
entity Y [runtime] { id : string! }
"#;
    let result = validate_lolo(lolo, "stranded-decl-orbital-above");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("closed the orbital early"),
        "brace teaching must stay when an orbital exists above: {sugg}"
    );
}

#[test]
fn event_name_with_space_gets_hint() {
    // run 39 (KnowledgeTracer): `REVIEW_ COMPLETED -> updateMastery`.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      REVIEW_ COMPLETED -> idle
    }
  }
}
"#;
    let result = validate_lolo(lolo, "event-name-space");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("ONE identifier"),
        "suggestion must name the space-in-event-name mistake: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("REVIEW_COMPLETED"),
        "fixGuidance must show the joined name: {fix}"
    );
}

#[test]
fn payload_field_default_gets_hint() {
    // run 39 (KnowledgeTracer): `masteryLevel : string = "Mastery Reached"`
    // inside an emits payload block.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
    emits {
      DONE -> external {
        masteryLevel : string = "Mastery Reached"
      }
    }
  }
}
"#;
    let result = validate_lolo(lolo, "payload-field-default");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("default value") && sugg.contains("payload"),
        "suggestion must name the illegal payload default: {sugg}"
    );
}

#[test]
fn invented_private_block_gets_hint() {
    // run 39 (KnowledgeTracer): a JS-class `private { … }` methods block.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
    private {
      updateBeta(alpha, beta) {
        alpha
      }
    }
  }
}
"#;
    let result = validate_lolo(lolo, "invented-private-block");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("NO function/method definitions"),
        "suggestion must say lolo has no method definitions: {sugg}"
    );
}

#[test]
fn inline_dash_prose_gets_hint() {
    // run 39 (CalendarHeatmap): `date : date! – day of the review activity`.
    let lolo = "orbital X {\n  entity X [runtime] {\n    date : date!   \u{2013} day of the review\n  }\n}\n";
    let result = validate_lolo(lolo, "inline-dash-prose");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains(";;"),
        "suggestion must route prose to a ;; comment: {sugg}"
    );
}

#[test]
fn unicode_escape_gets_hint() {
    // run 39 (StudyStreak): `\u` escape in a string.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! = "fire \u1f525" }
}
"#;
    let result = validate_lolo(lolo, "unicode-escape");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("not a valid string escape"),
        "suggestion must name the escape rule: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("UTF-8"),
        "fixGuidance must say to write the character directly: {fix}"
    );
}

#[test]
fn js_call_as_field_default_gets_hint() {
    // run 39d (ReviewReminders): `id : string! = uuid()` — JS call syntax
    // as an entity-field default.
    let lolo = r#"orbital X {
  entity X [runtime] {
    id : string! = uuid()
  }
}
"#;
    let result = validate_lolo(lolo, "js-call-field-default");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("JS call syntax"),
        "suggestion must name the call-syntax mistake: {sugg}"
    );
}

#[test]
fn markdown_divider_gets_hint() {
    // run 39d (ReviewScheduler): a `---` document divider in the file.
    let lolo = r#"app ReviewScheduler "1.0.0"

---

orbital ReviewScheduler {
  entity X [runtime] { id : string! }
}
"#;
    let result = validate_lolo(lolo, "markdown-divider");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("markdown `---` divider"),
        "suggestion must name the divider: {sugg}"
    );
}

#[test]
fn orbital_header_version_gets_hint() {
    // run 39e (ActivityHeatmap/TopicManager): `orbital Name "1.0.0"`.
    let lolo = r#"orbital ActivityHeatmap "1.0.0"

entity X [runtime] { id : string! }
"#;
    let result = validate_lolo(lolo, "orbital-header-version");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("NO version string"),
        "suggestion must reject the version string: {sugg}"
    );
}

#[test]
fn bare_docstring_in_orbital_gets_hint() {
    // run 39e (TopicManager): a bare prose string under the orbital header.
    let lolo = r#"orbital TopicManager {
"Manage study topics across subjects"
  entity X [runtime] { id : string! }
}
"#;
    let result = validate_lolo(lolo, "bare-docstring");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("bare prose string"),
        "suggestion must name the docstring mistake: {sugg}"
    );
}

#[test]
fn let_with_equals_gets_hint() {
    // run 39e (StreakTracker): `(let day = @payload.day)`.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (let day = @payload.day)
    }
  }
}
"#;
    let result = validate_lolo(lolo, "let-equals");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("positional"),
        "suggestion must teach positional let: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("(let name value"),
        "fixGuidance must show the form: {fix}"
    );
}

#[test]
fn infix_in_object_value_gets_hint() {
    // run 40 (StreakTracker): `streakCount: @entity.currentStreak + 1`.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (render-ui main { type: badge, label: @entity.id + 1 })
    }
  }
}
"#;
    let result = validate_lolo(lolo, "infix-object-value");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("NO infix syntax"),
        "suggestion must name the infix mistake: {sugg}"
    );
}

#[test]
fn jsx_attr_colon_gets_hint() {
    // run 40 (ReviewScheduler): a JSX tag whose props switch mid-tag from
    // `gap={sm}` JSX style to `renderItem: (fn …)` object style.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (render-ui main
          <UiStack
            gap={sm}
            renderItem: (fn item @item.title)
          >
          </UiStack>)
    }
  }
}
"#;
    let result = validate_lolo(lolo, "jsx-attr-colon");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("OBJECT syntax inside a JSX tag"),
        "suggestion must name the attr-colon mistake: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("renderItem={"),
        "fixGuidance must show the braced form for the named prop: {fix}"
    );
}

#[test]
fn jsx_markup_in_lambda_body_gets_hoist_hint() {
    // run 40 (ReviewScheduler residual): `(fn item {` with a JSX body.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
        (render-ui main {
          type: stack
          children: (array/map @entity.items (fn item {
            <UiStack direction="vertical" gap="xs">
            </UiStack>
          }))
        })
    }
  }
}
"#;
    let result = validate_lolo(lolo, "jsx-markup-in-lambda");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("JSX hoists out of the lambda") || sugg.contains("raw object form"),
        "suggestion must route away from JSX markup: {sugg}"
    );
}

#[test]
fn effect_under_routed_listens_gets_listens_hint() {
    // run 40b (StudyDashboard): a routed listens entry carrying effects.
    let lolo = r#"orbital X {
  entity X [runtime] { id : string! }
  trait T -> X [interaction] {
    initial: idle
    state idle {
      CLICK -> idle
    }
    listens {
      Other.SAVED -> idle
        (set @entity.id "x")
    }
  }
}
"#;
    let result = validate_lolo(lolo, "effect-in-listens");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("only NAMES the event"),
        "suggestion must teach the listens contract: {sugg}"
    );
}

#[test]
fn function_type_field_gets_hint() {
    // run 40b (StudySchedule): a TS method signature on an entity field.
    let lolo = r#"orbital X {
  expects entity KnowledgeTracker {
    recordObservation: (skillId: string, outcome: boolean) -> KnowledgeState
  }
  entity X [runtime] { id : string! }
}
"#;
    let result = validate_lolo(lolo, "function-type-field");
    let err = first_parse_error(&result);
    let sugg = err["suggestion"].as_str().unwrap();
    assert!(
        sugg.contains("FUNCTION type"),
        "suggestion must name the function-type mistake: {sugg}"
    );
    let fix = err["fixGuidance"].as_str().unwrap();
    assert!(
        fix.contains("trait"),
        "fixGuidance must route behavior to traits: {fix}"
    );
}
