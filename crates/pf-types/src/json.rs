//! Minimal, bounded JSON writer and parser.
//!
//! PulseForge emits machine-readable reports and stores schema-0 session data
//! as JSON/JSONL. To keep the workspace dependency-free and hostile-input-safe
//! this module implements exactly the JSON grammar it needs:
//!
//! * Writer: ordered objects (`Vec<(String, JsonValue)>`), exact integers
//!   (`i64`/`u64`), finite floats only, full string escaping.
//! * Parser: recursive descent with a hard [`MAX_DEPTH`] limit, a caller-owned
//!   input length expectation (callers apply their own byte caps before
//!   parsing), rejects trailing garbage, and returns typed errors instead of
//!   panicking on malformed input.
//!
//! These are used for reports and provisional schema-0 session files only; the
//! durable binary chunk format (roadmap PF-501) will replace JSONL for
//! high-frequency data.

// `from_utf8_unchecked` below relies on a provable invariant of `parse`:
// the input is a `&str` (already valid UTF-8) and string/number segments
// are delimited by ASCII bytes, which never occur inside a multi-byte
// sequence — so every segment is codepoint-aligned valid UTF-8 and the
// per-segment re-validation is redundant hot-path work.
#![allow(unsafe_code)]

use core::fmt;

/// Maximum nesting depth accepted by [`parse`]. Bounds stack usage against
/// hostile deeply-nested input (decompression-bomb style attacks).
pub const MAX_DEPTH: usize = 64;

/// A JSON value. Objects preserve insertion order so reports are stable.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonValue::UInt(v) => Some(*v),
            JsonValue::Int(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(v) => Some(*v),
            JsonValue::UInt(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Float(v) => Some(*v),
            JsonValue::Int(v) => Some(*v as f64),
            JsonValue::UInt(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object_pairs(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    pub fn as_object_pairs_mut(&mut self) -> Option<&mut Vec<(String, JsonValue)>> {
        match self {
            JsonValue::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Serialize to a compact JSON string. Non-finite floats are a programming
    /// error and become `null` rather than invalid JSON.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(true) => out.push_str("true"),
            JsonValue::Bool(false) => out.push_str("false"),
            JsonValue::Int(v) => {
                let _ = fmt::Write::write_fmt(out, format_args!("{v}"));
            }
            JsonValue::UInt(v) => {
                let _ = fmt::Write::write_fmt(out, format_args!("{v}"));
            }
            JsonValue::Float(v) => {
                if v.is_finite() {
                    // Rust float Display produces the shortest representation
                    // that round-trips; force a decimal point so the value
                    // re-parses as a number, not an int-looking token.
                    let s = format!("{v}");
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        out.push_str(&s);
                    } else {
                        out.push_str(&s);
                        out.push_str(".0");
                    }
                } else {
                    out.push_str("null");
                }
            }
            JsonValue::Str(s) => write_escaped(out, s),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            JsonValue::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escape `s` as a JSON string literal (including quotes) into `out`.
pub fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = fmt::Write::write_fmt(out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse error with byte offset, safe to display to callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JSON parse error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse one complete JSON document. Trailing non-whitespace is an error.
pub fn parse(input: &str) -> Result<JsonValue, ParseError> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

// All `pos` advances happen only after confirming a byte exists at `pos`, so
// the cursor stays <= input length; usize overflow is unreachable.
#[allow(clippy::arithmetic_side_effects)]
impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> ParseError {
        ParseError {
            offset: self.pos,
            message: message.to_string(),
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.bytes.get(self.pos) {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn value(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting depth limit exceeded"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(JsonValue::Str(self.string()?)),
            Some(b't') => self.literal("true", JsonValue::Bool(true)),
            Some(b'f') => self.literal("false", JsonValue::Bool(false)),
            Some(b'n') => self.literal("null", JsonValue::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            Some(c) => Err(self.err(&format!("unexpected character {:?}", c as char))),
        }
    }

    fn literal(&mut self, word: &str, value: JsonValue) -> Result<JsonValue, ParseError> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.err(&format!("invalid literal, expected `{word}`")))
        }
    }

    fn object(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        self.pos += 1; // '{'
        // Most objects are small (report/stacks frames ≈ 5–7 pairs); one
        // right-sized allocation beats growing 4 → 8.
        let mut pairs = Vec::with_capacity(8);
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key string"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(pairs));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        self.pos += 1; // '['
        let mut items = Vec::with_capacity(8);
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            self.skip_ws();
            let value = self.value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            // Fast path over plain UTF-8 segments.
            while let Some(b) = self.bytes.get(self.pos) {
                if *b == b'"' || *b == b'\\' || *b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > start {
                let chunk = &self.bytes[start..self.pos];
                // SAFETY: `parse` takes a `&str`, so the whole buffer is
                // valid UTF-8. UTF-8 is self-synchronizing and the segment
                // boundaries (`"`, `\`, bytes < 0x20) are ASCII — bytes
                // that never occur inside a multi-byte sequence — so this
                // segment is codepoint-aligned valid UTF-8. Re-validating
                // per segment is redundant hot-path work (stacks.jsonl is
                // mostly strings); the fuzz suites feed `&str` inputs only,
                // so the invariant is guaranteed by the type.
                let s = unsafe { std::str::from_utf8_unchecked(chunk) };
                out.push_str(s);
            }
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => {
                            out.push('"');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            out.push('/');
                            self.pos += 1;
                        }
                        Some(b'b') => {
                            out.push('\u{08}');
                            self.pos += 1;
                        }
                        Some(b'f') => {
                            out.push('\u{0C}');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // High surrogate: require a following \uXXXX
                                // low surrogate.
                                if self.bytes.get(self.pos) != Some(&b'\\')
                                    || self.bytes.get(self.pos + 1) != Some(&b'u')
                                {
                                    return Err(self.err("unpaired high surrogate"));
                                }
                                self.pos += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.err("invalid low surrogate"));
                                }
                                let combined = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                match char::from_u32(combined) {
                                    Some(c) => out.push(c),
                                    None => return Err(self.err("invalid surrogate pair")),
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(self.err("unpaired low surrogate"));
                            } else {
                                match char::from_u32(cp) {
                                    Some(c) => out.push(c),
                                    None => return Err(self.err("invalid code point")),
                                }
                            }
                        }
                        _ => return Err(self.err("invalid escape sequence")),
                    }
                }
                Some(b) if b < 0x20 => {
                    return Err(self.err("unescaped control character in string"));
                }
                Some(_) => unreachable!("fast path consumed plain bytes"),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(b @ b'0'..=b'9') => u32::from(b - b'0'),
                Some(b @ b'a'..=b'f') => u32::from(b - b'a') + 10,
                Some(b @ b'A'..=b'F') => u32::from(b - b'A') + 10,
                _ => return Err(self.err("invalid \\u escape")),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<JsonValue, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: expected digits after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: expected exponent digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Number characters (`-0-9.eE+`) are all ASCII, so the slice is
        // valid UTF-8 by the same &str-input invariant as string segments.
        let text = unsafe { std::str::from_utf8_unchecked(&self.bytes[start..self.pos]) };
        if !is_float {
            // Canonical integer decoding (serde_json convention): a value
            // that fits i64 decodes to Int, larger values to UInt. This
            // keeps writer→parser round-trips stable for canonical values
            // (fuzz-test-enforced): non-negative integers stored as Int
            // re-parse as Int.
            if let Ok(v) = text.parse::<i64>() {
                return Ok(JsonValue::Int(v));
            }
            if let Ok(v) = text.parse::<u64>() {
                return Ok(JsonValue::UInt(v));
            }
        }
        text.parse::<f64>()
            .map(JsonValue::Float)
            .map_err(|_| self.err("number out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_escapes_strings() {
        let v = JsonValue::Str("a\"b\\c\nd\u{1}".into());
        assert_eq!(v.to_json_string(), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn writer_keeps_integers_exact() {
        let v = JsonValue::UInt(u64::from(u32::MAX) * 3 + 7);
        let s = v.to_json_string();
        assert_eq!(s.parse::<u64>().unwrap(), u64::from(u32::MAX) * 3 + 7);
    }

    #[test]
    fn writer_floats_have_decimal_point() {
        assert_eq!(JsonValue::Float(2.0).to_json_string(), "2.0");
        assert_eq!(JsonValue::Float(0.1).to_json_string(), "0.1");
    }

    #[test]
    fn writer_nonfinite_float_is_null_not_invalid_json() {
        assert_eq!(JsonValue::Float(f64::NAN).to_json_string(), "null");
    }

    #[test]
    fn writer_nested_structures() {
        let v = JsonValue::Object(vec![
            ("a".into(), JsonValue::Int(1)),
            (
                "b".into(),
                JsonValue::Array(vec![JsonValue::Bool(true), JsonValue::Null]),
            ),
        ]);
        assert_eq!(v.to_json_string(), "{\"a\":1,\"b\":[true,null]}");
    }

    #[test]
    fn parse_roundtrip() {
        let docs = [
            "{\"a\":1,\"b\":[true,null,\"x\\ny\"],\"c\":2.5,\"d\":-3}",
            "[[[[]]]]",
            "\"\\u0041\\u00e9\\ud834\\udd1e\"",
            "18446744073709551615",
        ];
        for doc in docs {
            let parsed = parse(doc).expect(doc);
            let re = parse(&parsed.to_json_string()).expect("reparse");
            assert_eq!(parsed, re, "roundtrip stable for {doc}");
        }
    }

    #[test]
    fn parse_surrogate_pair() {
        let v = parse("\"\\ud834\\udd1e\"").unwrap();
        assert_eq!(v, JsonValue::Str("\u{1D11E}".into()));
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "{",
            "}",
            "[1,]",
            "{\"a\":}",
            "tru",
            "\"unterminated",
            "01",
            "1.",
            "1e",
            "{}extra",
            "\"\\x12\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "[1 2]",
            "{\"a\" 1}",
        ] {
            assert!(parse(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn parse_rejects_deep_nesting() {
        let deep = "[".repeat(MAX_DEPTH + 8) + &"]".repeat(MAX_DEPTH + 8);
        assert!(parse(&deep).is_err());
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn parse_control_characters_rejected() {
        assert!(parse("\"a\nb\"").is_err());
        assert!(parse("\"a\tb\"").is_err());
        assert!(parse("\"a\\tb\"").is_ok());
    }

    #[test]
    fn parse_multibyte_codepoints_across_segments() {
        // The unchecked-UTF-8 fast path relies on segments between ASCII
        // structural bytes being codepoint-aligned valid UTF-8: multi-byte
        // sequences must survive intact, including around escapes.
        let v = parse(r#"{"path":"/usr/日本語/libß.so","esc":"é\nü","sym":"∑→∑"}"#).unwrap();
        assert_eq!(
            v.get("path").and_then(|x| x.as_str()),
            Some("/usr/日本語/libß.so")
        );
        assert_eq!(v.get("esc").and_then(|x| x.as_str()), Some("é\nü"));
        assert_eq!(v.get("sym").and_then(|x| x.as_str()), Some("∑→∑"));
        // Same for arrays of multi-byte strings.
        let arr = parse(r#"["日本","∑∑∑","plain"]"#).unwrap();
        let a = arr.as_array().unwrap();
        assert_eq!(a[1].as_str(), Some("∑∑∑"));
    }

    #[test]
    fn accessors() {
        let v = parse(r#"{"name":"pf","n":42,"neg":-7,"f":1.5,"on":true,"list":[1,2]}"#).unwrap();
        assert_eq!(v.get("name").and_then(|x| x.as_str()), Some("pf"));
        assert_eq!(v.get("n").and_then(|x| x.as_u64()), Some(42));
        assert_eq!(v.get("neg").and_then(|x| x.as_i64()), Some(-7));
        assert_eq!(v.get("f").and_then(|x| x.as_f64()), Some(1.5));
        assert_eq!(v.get("on").and_then(|x| x.as_bool()), Some(true));
        assert_eq!(
            v.get("list").and_then(|x| x.as_array()).map(|a| a.len()),
            Some(2)
        );
        assert!(v.get("missing").is_none());
    }
}
