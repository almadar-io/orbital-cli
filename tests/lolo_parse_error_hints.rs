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
    assert!(err["fixGuidance"]
        .as_str()
        .unwrap()
        .contains("stray `;`"));
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
