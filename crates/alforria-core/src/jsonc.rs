//! JSONC parsing: `parse_jsonc` mirrors TS `ConfigParse.jsonc`
//! (`packages/opencode/src/config/parse.ts`).
//!
//! TS uses the npm `jsonc-parser` with `{ allowTrailingComma: true }`; the Rust
//! port uses the `jsonc-parser` crate (0.33, configured to match the npm
//! package's acceptance set: trailing commas and comments allowed; single
//! quotes, missing commas, loose property names, hex and unary-plus numbers
//! rejected — each of those makes the npm parser report an error, which makes
//! `ConfigParse.jsonc` throw). Accepted inputs produce a `serde_json::Value`;
//! syntax errors fail before any typed decode.
//!
//! Error blocks keep the exact TS shape:
//!
//! ```text
//! "\n--- JSONC Input ---\n{text}\n--- Errors ---\n{issues}\n--- End ---"
//! ```
//!
//! where each issue is `"{message} at line {line}, column {column}"` optionally
//! followed by the offending line and a caret, with line/column computed
//! exactly as `parse.ts:8-33` computes them (1-based; the column counts UTF-16
//! units, matching JS string indexing).
//!
//! Deviations from TS (crate limitations, recorded per the M3.1 chunk notes):
//!
//! 1. The Rust crate reports only the *first* parse error; the npm package
//!    collects all of them into the `errors` array. The block format is
//!    unchanged — it just never holds more than one issue here.
//! 2. The issue message is the Rust crate's prose (e.g. `"Unexpected close
//!    brace"`), not the npm package's `printParseErrorCode` name (e.g.
//!    `"ValueExpected"`). Line/column/caret placement is identical — verified
//!    against node-generated goldens in the tests below.
//! 3. The npm parser reports an error for empty, whitespace-only or
//!    comment-only input (no value to read); `parse_jsonc` mirrors that
//!    by synthesizing the same `ValueExpected` error the TS block would show.
//! 4. The npm scanner rejects raw control characters inside string literals;
//!    the Rust crate accepts them. `parse_jsonc` enforces the TS acceptance
//!    set with a post-parse validation of the raw string tokens.

use std::path::Path;

use jsonc_parser::{ast, parse_to_ast, CollectOptions, ParseOptions};
use serde_json::{Map, Number, Value};

use crate::CoreError;

/// Parse JSONC text into a `serde_json::Value`.
///
/// The npm package returns `undefined` for input with no JSON value; since the
/// TS reference *throws* for that input (the parser reports an error), this
/// function does too.
pub fn parse_jsonc(text: &str, path: &Path) -> Result<Value, CoreError> {
    let result =
        parse_to_ast(text, &CollectOptions::default(), &parse_options()).map_err(|err| {
            format_parse_error(text, path, err.range().start, &err.kind().to_string())
        })?;

    match result.value {
        Some(value) => convert_value(text, path, &value),
        None => {
            // npm jsonc-parser reports error code 4 (`ValueExpected`, offset 0)
            // for input that contains no JSON value at all.
            Err(format_parse_error(text, path, 0, "ValueExpected"))
        }
    }
}

/// Acceptance set verified against the npm `jsonc-parser` `parse()`:
/// single quotes, missing commas, loose property names, hex and unary-plus
/// numbers all make the npm parser report errors (so `ConfigParse.jsonc`
/// throws) — disable them here. Trailing commas match TS's
/// `{ allowTrailingComma: true }`.
fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_single_quoted_strings: false,
        allow_missing_commas: false,
        allow_loose_object_property_names: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Convert the JSONC AST into a `serde_json::Value`.
///
/// Object properties are inserted in source order, so a duplicate key keeps the
/// last value (the npm parser produces the same last-wins behavior).
fn convert_value(text: &str, path: &Path, value: &ast::Value<'_>) -> Result<Value, CoreError> {
    match value {
        ast::Value::Object(obj) => {
            let mut map = Map::new();
            for prop in &obj.properties {
                let value = convert_value(text, path, &prop.value)?;
                map.insert(prop.name.as_str().to_owned(), value);
            }
            Ok(Value::Object(map))
        }
        ast::Value::Array(arr) => {
            let mut elements = Vec::with_capacity(arr.elements.len());
            for element in &arr.elements {
                elements.push(convert_value(text, path, element)?);
            }
            Ok(Value::Array(elements))
        }
        ast::Value::StringLit(s) => {
            check_string_literal(text, path, s)?;
            Ok(Value::String(s.value.to_string()))
        }
        ast::Value::NumberLit(n) => Ok(decode_number(n.value)),
        ast::Value::BooleanLit(b) => Ok(Value::Bool(b.value)),
        ast::Value::NullKeyword(_) => Ok(Value::Null),
    }
}

/// The npm scanner rejects raw control characters inside string literals
/// (`InvalidCharacter`; `\n`/`\r` terminate the string with
/// `UnexpectedEndOfString`), while the Rust crate happily captures them.
/// Enforce the TS acceptance set here.
///
/// The check runs on the raw source slice (quotes included): the crate already
/// unescapes the value, so escape sequences like `\n` and raw control
/// characters are indistinguishable in `lit.value` — but in the source only
/// the latter contain bytes below `\u{20}`.
fn check_string_literal(
    text: &str,
    path: &Path,
    lit: &ast::StringLit<'_>,
) -> Result<(), CoreError> {
    let start = lit.range.start.min(text.len());
    let end = lit.range.end.min(text.len());
    let raw = &text[start..end];
    if let Some(c) = raw.chars().find(|c| *c < '\u{20}') {
        let message = if c == '\n' || c == '\r' {
            "UnexpectedEndOfString"
        } else {
            "InvalidCharacter"
        };
        return Err(format_parse_error(text, path, start, message));
    }
    Ok(())
}

fn decode_number(raw: &str) -> Value {
    if let Ok(i) = raw.parse::<i64>() {
        Value::Number(Number::from(i))
    } else {
        raw.parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Build the TS-shaped `ConfigJsonError` for a parse failure.
///
/// Line/column mirror `parse.ts:8-33`: split `text[..pos]` on `\n`, the line is
/// the number of segments, the column is the UTF-16 length of the last segment
/// plus one.
fn format_parse_error(text: &str, path: &Path, pos: usize, message: &str) -> CoreError {
    // `pos` is a byte offset into `text`; the scanner advances char by char,
    // so it always lands on a char boundary.
    let offset = pos.min(text.len());
    let before = &text[..offset];
    let before_lines: Vec<&str> = before.split('\n').collect();
    let line = before_lines.len();
    let column = before_lines
        .last()
        .map(|l| l.encode_utf16().count() + 1)
        .unwrap_or(1);

    let issue = format!("{message} at line {line}, column {column}");
    // TS: `if (!problemLine) return error` — an empty line is falsy and drops
    // the problem-line suffix too. Replicate that.
    let problem_line = text.split('\n').nth(line - 1);
    let issues = match problem_line {
        Some(p) if !p.is_empty() => {
            format!("{issue}\n   Line {line}: {p}\n{}^", " ".repeat(column + 9))
        }
        _ => issue,
    };

    let message = format!("\n--- JSONC Input ---\n{text}\n--- Errors ---\n{issues}\n--- End ---");
    CoreError::Jsonc {
        path: path.to_path_buf(),
        line: Some(line),
        column: Some(column),
        message,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::CoreError;

    #[test]
    fn parses_comments_and_trailing_commas() {
        let text = r#"{
  // line comment
  /* block
     comment */
  "a": [1, 2, 3,],
  "b": "http://not-a-comment",
  "c": { "nested": true, },
}"#;
        let value = parse_jsonc(text, Path::new("opencode.json")).unwrap();
        assert_eq!(value["a"], json!([1, 2, 3]));
        assert_eq!(value["b"], json!("http://not-a-comment"));
        assert_eq!(value["c"]["nested"], json!(true));
    }

    #[test]
    fn parses_string_containing_double_slash() {
        let value = parse_jsonc(
            r#"{ "url": "http://example.com" }"#,
            Path::new("opencode.json"),
        )
        .unwrap();
        assert_eq!(value["url"], json!("http://example.com"));
    }

    #[test]
    fn parses_escaped_strings() {
        let value = parse_jsonc(
            r#"{ "a": "line\nbreak \"quoted\" é", "b": "\u0041" }"#,
            Path::new("opencode.json"),
        )
        .unwrap();
        assert_eq!(value["a"], json!("line\nbreak \"quoted\" é"));
        assert_eq!(value["b"], json!("A"));
    }

    #[test]
    fn parses_numbers() {
        let value = parse_jsonc(
            r#"{ "i": 42, "neg": -1, "f": 3.5, "exp": 1e2, "exp2": 1e+2 }"#,
            Path::new("opencode.json"),
        )
        .unwrap();
        assert_eq!(value["i"], json!(42));
        assert_eq!(value["neg"], json!(-1));
        assert_eq!(value["f"], json!(3.5));
        assert_eq!(value["exp"], json!(100.0));
        assert_eq!(value["exp2"], json!(100.0));
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let value = parse_jsonc(r#"{ "a": 1, "a": 2 }"#, Path::new("opencode.json")).unwrap();
        assert_eq!(value["a"], json!(2));
    }

    #[test]
    fn empty_input_errors_like_ts() {
        for text in ["", "  \n", "// only a comment"] {
            let err = parse_jsonc(text, Path::new("opencode.json")).unwrap_err();
            match err {
                CoreError::Jsonc { message, .. } => {
                    assert!(
                        message.contains("ValueExpected at line 1, column 1"),
                        "{message}"
                    );
                }
                other => panic!("expected Jsonc error, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_ts_rejected_extensions() {
        // Each of these reports an error with the npm parser, so TS throws.
        for text in [
            "{'a': 1}",              // single-quoted strings
            "{ \"a\": 1 \"b\": 2 }", // missing comma
            "{ a: 1 }",              // loose property names
            "{ \"a\": 0xFF }",       // hex numbers
            "{ \"a\": +42 }",        // unary plus
            "{ \"a\": .5 }",         // leading dot
            "{ \"a\": 0123 }",       // leading zeros
        ] {
            assert!(
                parse_jsonc(text, Path::new("opencode.json")).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn rejects_trailing_garbage() {
        let err = parse_jsonc("{ \"a\": 1 } garbage", Path::new("opencode.json")).unwrap_err();
        assert!(matches!(err, CoreError::Jsonc { .. }));
    }

    // Golden: input, line and column were produced by running the TS reference
    // formatting logic (`parse.ts`) with the npm jsonc-parser (node 24,
    // jsonc-parser@latest). The issue message text differs between the two
    // parsers, but the reported positions match.
    #[test]
    fn error_positions_match_ts_golden() {
        let cases = [
            (
                // TS: ValueExpected at line 3, column 1
                "{\n  \"key\":\n}",
                3,
                1,
            ),
            (
                // TS: InvalidSymbol at line 2, column 10
                "{\n  \"key\": @\n}",
                2,
                10,
            ),
            (
                // TS: UnexpectedEndOfString at line 1, column 8
                "{ \"a\": \"abc }",
                1,
                8,
            ),
        ];
        for (text, line, column) in cases {
            let err = parse_jsonc(text, Path::new("opencode.json")).unwrap_err();
            match err {
                CoreError::Jsonc {
                    line: l, column: c, ..
                } => {
                    assert_eq!(l, Some(line), "line for {text:?}");
                    assert_eq!(c, Some(column), "column for {text:?}");
                }
                other => panic!("expected Jsonc error, got {other:?} for {text:?}"),
            }
        }
    }

    #[test]
    fn error_block_matches_ts_shape() {
        let err = parse_jsonc("{\n  \"key\":\n}", Path::new("opencode.json")).unwrap_err();
        match err {
            CoreError::Jsonc { message, .. } => {
                let expected = concat!(
                    "\n--- JSONC Input ---\n",
                    "{\n  \"key\":\n}\n",
                    "--- Errors ---\n",
                    "Unexpected close brace at line 3, column 1\n",
                    "   Line 3: }\n",
                    "          ^\n",
                    "--- End ---",
                );
                assert_eq!(message, expected);
            }
            other => panic!("expected Jsonc error, got {other:?}"),
        }
    }

    #[test]
    fn error_block_on_multiline_problem() {
        let err = parse_jsonc("{ \"a\": \"abc }", Path::new("opencode.json")).unwrap_err();
        match err {
            CoreError::Jsonc { message, .. } => {
                let expected = concat!(
                    "\n--- JSONC Input ---\n",
                    "{ \"a\": \"abc }\n",
                    "--- Errors ---\n",
                    "Unterminated string literal at line 1, column 8\n",
                    "   Line 1: { \"a\": \"abc }\n",
                    "                 ^\n",
                    "--- End ---",
                );
                assert_eq!(message, expected);
            }
            other => panic!("expected Jsonc error, got {other:?}"),
        }
    }

    // Differential test against the npm jsonc-parser (node 24,
    // `parse(text, errors, { allowTrailingComma: true })`): every input's
    // accept/reject outcome and accepted value recorded by hand from the npm
    // package. `ok` is what the TS reference sees (errors present ⇒
    // `ConfigParse.jsonc` throws).
    #[test]
    fn accept_set_matches_npm_jsonc_parser() {
        // (input, npm-accepted, npm-parsed value as JSON)
        let cases: &[(&str, bool, Option<&str>)] = &[
            (
                r#"{"a": {"b": [1, {"c": 2}, ] }}"#,
                true,
                Some(r#"{"a":{"b":[1,{"c":2}]}}"#),
            ),
            (r#"{"a": "√ π 漢"}"#, true, Some(r#"{"a":"√ π 漢"}"#)),
            // raw control characters in strings are rejected by npm
            ("{\"a\": \"x\tx\"}", false, None),
            ("{\"a\": \"x\nx\"}", false, None),
            (
                r#"{/* c */ "a" /* d */: /* e */ 1 }"#,
                true,
                Some(r#"{"a":1}"#),
            ),
            (r#"{"a": 1} // trailing"#, true, Some(r#"{"a":1}"#)),
            (
                "{\n/* multi\nline */\n\"a\": 1\n}",
                true,
                Some(r#"{"a":1}"#),
            ),
            (r#"{"a": 1e-2}"#, true, Some(r#"{"a":0.01}"#)),
            (r#"{"a": 1E-10}"#, true, Some(r#"{"a":1e-10}"#)),
            (
                r#"{"a": 12345678901234567890}"#,
                true,
                Some(r#"{"a":1.2345678901234568e19}"#),
            ),
            ("[01, 2]", false, None),
            ("{}", true, Some(r#"{}"#)),
            ("[]", true, Some(r#"[]"#)),
            (
                r#"{"t": true, "f": false, "n": null}"#,
                true,
                Some(r#"{"t":true,"f":false,"n":null}"#),
            ),
            (
                r#"{"a": "escaped \" quote"}"#,
                true,
                Some(r#"{"a":"escaped \" quote"}"#),
            ),
            (r#"{"a": "éA"}"#, true, Some(r#"{"a":"éA"}"#)),
            (r#"{"a": "\ud83d\ude00"}"#, true, Some(r#"{"a":"😀"}"#)),
            (
                r#"{"a": {"b": {"c": {"d": [1,2,{"e": 3}]}}}}"#,
                true,
                Some(r#"{"a":{"b":{"c":{"d":[1,2,{"e":3}]}}}}"#),
            ),
            (
                r#"{"a": "/* not comment */"}"#,
                true,
                Some(r#"{"a":"/* not comment */"}"#),
            ),
            (
                r#"{"a": "// not comment"}"#,
                true,
                Some(r#"{"a":"// not comment"}"#),
            ),
            ("{\"a\": [,]}", false, None),
            (r#"{"a": -}"#, false, None),
            (r#"{"a": 1+2}"#, false, None),
            (r#"{"a": NaN}"#, false, None),
            (r#"{"a": Infinity}"#, false, None),
            (r#"{"a": undefined}"#, false, None),
        ];

        for (text, npm_ok, npm_value) in cases {
            let parsed = parse_jsonc(text, Path::new("opencode.json"));
            if !npm_ok {
                assert!(parsed.is_err(), "expected rejection for {text}");
                continue;
            }
            let value = parsed.unwrap_or_else(|e| panic!("expected success for {text}: {e}"));
            let expected: Value =
                serde_json::from_str(npm_value.unwrap()).expect("test fixture JSON");
            assert_eq!(value, expected, "value mismatch for {text}");
        }
    }
}
