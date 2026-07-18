//! A minimal hand-written JSON parser — just enough to let the REPL (and
//! anyone else) express documents and queries as JSON text like
//! `{"name": "Alice", "age": {"$gt": 25}}` and have it become our
//! Document/Value types, plus the reverse for printing results.
//!
//! This is a recursive-descent parser: `parse_value_from` looks at the
//! next character to decide what kind of value follows, and objects and
//! arrays recursively call back into it for their contents — the same
//! recursive pattern we've used since Phase 1 for nested documents.

use crate::document::{Document, Value};
use std::iter::Peekable;
use std::str::Chars;

/// Parse `input` as a JSON object and return it as a `Document`. This is
/// the entry point the REPL uses, since every command that takes a
/// document or query expects an object at the top level.
pub fn parse_document(input: &str) -> Result<Document, String> {
    match parse_value(input)? {
        Value::Document(doc) => Ok(doc),
        _ => Err("expected a JSON object, e.g. {\"field\": value}".to_string()),
    }
}

pub fn parse_value(input: &str) -> Result<Value, String> {
    let mut chars = input.chars().peekable();
    let value = parse_value_from(&mut chars)?;
    skip_whitespace(&mut chars);
    if chars.peek().is_some() {
        return Err("unexpected trailing characters after JSON value".to_string());
    }
    Ok(value)
}

fn parse_value_from(chars: &mut Peekable<Chars<'_>>) -> Result<Value, String> {
    skip_whitespace(chars);
    match chars.peek() {
        Some('{') => parse_object(chars).map(Value::Document),
        Some('[') => parse_array(chars),
        Some('"') => parse_string(chars).map(Value::Str),
        Some('t') | Some('f') => parse_bool(chars),
        Some('n') => parse_null(chars),
        Some(c) if c.is_ascii_digit() || *c == '-' => parse_number(chars),
        Some(c) => Err(format!("unexpected character '{c}'")),
        None => Err("unexpected end of input".to_string()),
    }
}

fn skip_whitespace(chars: &mut Peekable<Chars<'_>>) {
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
}

fn expect_char(chars: &mut Peekable<Chars<'_>>, expected: char) -> Result<(), String> {
    match chars.next() {
        Some(c) if c == expected => Ok(()),
        Some(c) => Err(format!("expected '{expected}', found '{c}'")),
        None => Err(format!("expected '{expected}', found end of input")),
    }
}

fn parse_object(chars: &mut Peekable<Chars<'_>>) -> Result<Document, String> {
    expect_char(chars, '{')?;
    let mut doc = Document::new();

    skip_whitespace(chars);
    if chars.peek() == Some(&'}') {
        chars.next();
        return Ok(doc);
    }

    loop {
        skip_whitespace(chars);
        let key = parse_string(chars)?;
        skip_whitespace(chars);
        expect_char(chars, ':')?;
        let value = parse_value_from(chars)?;
        doc.insert(key, value);

        skip_whitespace(chars);
        match chars.next() {
            Some(',') => continue,
            Some('}') => break,
            Some(c) => return Err(format!("expected ',' or '}}', found '{c}'")),
            None => return Err("unexpected end of input inside object".to_string()),
        }
    }
    Ok(doc)
}

fn parse_array(chars: &mut Peekable<Chars<'_>>) -> Result<Value, String> {
    expect_char(chars, '[')?;
    let mut items = Vec::new();

    skip_whitespace(chars);
    if chars.peek() == Some(&']') {
        chars.next();
        return Ok(Value::Array(items));
    }

    loop {
        items.push(parse_value_from(chars)?);
        skip_whitespace(chars);
        match chars.next() {
            Some(',') => continue,
            Some(']') => break,
            Some(c) => return Err(format!("expected ',' or ']', found '{c}'")),
            None => return Err("unexpected end of input inside array".to_string()),
        }
    }
    Ok(Value::Array(items))
}

fn parse_string(chars: &mut Peekable<Chars<'_>>) -> Result<String, String> {
    expect_char(chars, '"')?;
    let mut s = String::new();
    loop {
        match chars.next() {
            Some('"') => break,
            Some('\\') => match chars.next() {
                Some('"') => s.push('"'),
                Some('\\') => s.push('\\'),
                Some('/') => s.push('/'),
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some(other) => return Err(format!("unsupported escape sequence '\\{other}'")),
                None => return Err("unexpected end of input in string escape".to_string()),
            },
            Some(c) => s.push(c),
            None => return Err("unterminated string".to_string()),
        }
    }
    Ok(s)
}

fn parse_bool(chars: &mut Peekable<Chars<'_>>) -> Result<Value, String> {
    if consume_literal(chars, "true") {
        Ok(Value::Bool(true))
    } else if consume_literal(chars, "false") {
        Ok(Value::Bool(false))
    } else {
        Err("expected 'true' or 'false'".to_string())
    }
}

fn parse_null(chars: &mut Peekable<Chars<'_>>) -> Result<Value, String> {
    if consume_literal(chars, "null") {
        Ok(Value::Null)
    } else {
        Err("expected 'null'".to_string())
    }
}

/// Try to consume an exact literal (like "true" or "null"). We check on a
/// cloned iterator first and only commit (`*chars = clone`) if the whole
/// literal matched — this is how a hand-written parser gets "lookahead"
/// without a more elaborate backtracking mechanism.
fn consume_literal(chars: &mut Peekable<Chars<'_>>, literal: &str) -> bool {
    let mut clone = chars.clone();
    for expected in literal.chars() {
        match clone.next() {
            Some(c) if c == expected => continue,
            _ => return false,
        }
    }
    *chars = clone;
    true
}

fn parse_number(chars: &mut Peekable<Chars<'_>>) -> Result<Value, String> {
    let mut text = String::new();
    if chars.peek() == Some(&'-') {
        text.push(chars.next().unwrap());
    }

    let mut is_float = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            text.push(c);
            chars.next();
        } else if c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-' {
            is_float = true;
            text.push(c);
            chars.next();
        } else {
            break;
        }
    }

    if is_float {
        text.parse::<f64>().map(Value::Float).map_err(|_| format!("invalid number '{text}'"))
    } else {
        text.parse::<i64>().map(Value::Int).map_err(|_| format!("invalid number '{text}'"))
    }
}

// --- Rendering Document/Value back to a JSON string, for printing results ---

pub fn to_json_string(doc: &Document) -> String {
    value_to_json(&Value::Document(doc.clone()))
}

fn value_to_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        // Debug-formatting a &str gives us a quoted, escaped string for
        // free — Rust's string Debug impl already does the JSON-style
        // escaping we want (\n, \t, \", \\, etc.).
        Value::Str(s) => format!("{s:?}"),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(value_to_json).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Document(doc) => {
            let parts: Vec<String> = doc.iter().map(|(k, v)| format!("{k:?}: {}", value_to_json(v))).collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_object() {
        let doc = parse_document(r#"{"name": "Alice", "age": 30, "active": true}"#).unwrap();
        assert_eq!(doc.get("name"), Some(&Value::Str("Alice".to_string())));
        assert_eq!(doc.get("age"), Some(&Value::Int(30)));
        assert_eq!(doc.get("active"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parses_nested_object_and_array() {
        let doc = parse_document(r#"{"tags": ["a", "b"], "address": {"city": "Bengaluru"}}"#).unwrap();
        assert_eq!(
            doc.get("tags"),
            Some(&Value::Array(vec![Value::Str("a".to_string()), Value::Str("b".to_string())]))
        );
        match doc.get("address") {
            Some(Value::Document(inner)) => {
                assert_eq!(inner.get("city"), Some(&Value::Str("Bengaluru".to_string())))
            }
            _ => panic!("expected nested document"),
        }
    }

    #[test]
    fn parses_negative_and_float_numbers() {
        let doc = parse_document(r#"{"a": -5, "b": 3.14}"#).unwrap();
        assert_eq!(doc.get("a"), Some(&Value::Int(-5)));
        assert_eq!(doc.get("b"), Some(&Value::Float(3.14)));
    }

    #[test]
    fn parses_null_and_escaped_strings() {
        let doc = parse_document(r#"{"a": null, "b": "line1\nline2"}"#).unwrap();
        assert_eq!(doc.get("a"), Some(&Value::Null));
        assert_eq!(doc.get("b"), Some(&Value::Str("line1\nline2".to_string())));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_document(r#"{"name": "Alice""#).is_err()); // missing closing brace
        assert!(parse_document(r#"not json at all"#).is_err());
        assert!(parse_document(r#"{"a": 1,}"#).is_err()); // trailing comma
    }

    #[test]
    fn round_trips_through_to_json_string() {
        let mut doc = Document::new();
        doc.insert("name", "Alice");
        doc.insert("age", 30);

        let json_text = to_json_string(&doc);
        let reparsed = parse_document(&json_text).unwrap();
        assert_eq!(reparsed, doc);
    }
}
