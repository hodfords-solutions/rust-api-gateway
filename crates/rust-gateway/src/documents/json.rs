//! JSON with JavaScript semantics: `JSON.parse` into a value tree and `JSON.stringify` out
//! of it.
//!
//! The Reference Gateway parses each fetched document with `JSON.parse` (through axios) and
//! sends it back with `JSON.stringify` (Express `res.json`), so what a client receives is
//! not the upstream's bytes but their JavaScript round trip:
//!
//! - object keys that are array indices (canonical integers from 0 to 2^32 - 2) come first,
//!   in ascending numeric order, then every other key in insertion order;
//! - a duplicate key keeps the position of its first occurrence and the last value;
//! - numbers are IEEE doubles, written by `Number.prototype.toString` (`1.0` as `1`, `1e21`
//!   as `1e+21`, integers beyond 2^53 rounded, non-finite values as `null`);
//! - strings are UTF-16: lone surrogates survive and are written as `\udXXX`, `\/` is
//!   written as `/`, and only `"`, `\` and control characters are escaped.
//!
//! `serde_json` differs on each point, so this module keeps strings as UTF-16 code units,
//! numbers as `f64`, and orders keys when writing.

use std::fmt::Write as _;

use indexmap::IndexMap;

use crate::error::js_json;

/// Nesting deeper than this is refused (it would only risk the stack). `serde_json`, which
/// the document loader uses, already refuses documents nested deeper than 128.
pub(crate) const MAX_DEPTH: usize = 512;

/// A JavaScript string: UTF-16 code units, possibly with lone surrogates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct JsString(Vec<u16>);

impl JsString {
    /// The string's UTF-16 encoding.
    pub(crate) fn from_str(text: &str) -> Self {
        Self(text.encode_utf16().collect())
    }

    /// Whether this is exactly `text`.
    pub(crate) fn eq_str(&self, text: &str) -> bool {
        self.0.iter().copied().eq(text.encode_utf16())
    }

    /// `startsWith(prefix)`.
    pub(crate) fn starts_with_str(&self, prefix: &str) -> bool {
        let mut units = self.0.iter().copied();
        prefix.encode_utf16().all(|unit| units.next() == Some(unit))
    }

    /// The string as Rust text, with lone surrogates replaced by U+FFFD.
    pub(crate) fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    /// The array index this key denotes, if it is one.
    pub(crate) fn array_index(&self) -> Option<u32> {
        let units = &self.0;
        if units.is_empty() || units.len() > 10 {
            return None;
        }
        if units.len() > 1 && units[0] == u16::from(b'0') {
            return None;
        }
        let mut value: u64 = 0;
        for &unit in units {
            let digit = u8::try_from(unit).ok().filter(u8::is_ascii_digit)?;
            value = value * 10 + u64::from(digit - b'0');
        }
        // 2^32 - 1 is not an array index.
        u32::try_from(value).ok().filter(|&index| index != u32::MAX)
    }
}

/// An object: keys in insertion order (array-index keys are ordered when written).
pub(crate) type JsObject = IndexMap<JsString, JsValue>;

/// A value produced by `JSON.parse`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JsValue {
    Null,
    Bool(bool),
    Number(f64),
    String(JsString),
    Array(Vec<JsValue>),
    Object(JsObject),
}

impl JsValue {
    /// A string value.
    pub(crate) fn string(text: &str) -> Self {
        JsValue::String(JsString::from_str(text))
    }

    /// An object value from `(key, value)` pairs.
    pub(crate) fn object<'a>(entries: impl IntoIterator<Item = (&'a str, JsValue)>) -> Self {
        JsValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (JsString::from_str(key), value))
                .collect(),
        )
    }

    /// JavaScript truthiness.
    pub(crate) fn is_truthy(&self) -> bool {
        match self {
            JsValue::Null => false,
            JsValue::Bool(value) => *value,
            JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
            JsValue::String(value) => !value.0.is_empty(),
            JsValue::Array(_) | JsValue::Object(_) => true,
        }
    }

    /// The value as a `serde_json` value would be read by JavaScript (numbers as doubles).
    pub(crate) fn from_serde(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => JsValue::Null,
            serde_json::Value::Bool(value) => JsValue::Bool(*value),
            serde_json::Value::Number(value) => JsValue::Number(value.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(value) => JsValue::string(value),
            serde_json::Value::Array(items) => {
                JsValue::Array(items.iter().map(JsValue::from_serde).collect())
            }
            serde_json::Value::Object(map) => JsValue::Object(
                map.iter()
                    .map(|(key, value)| (JsString::from_str(key), JsValue::from_serde(value)))
                    .collect(),
            ),
        }
    }
}

/// Where and why `JSON.parse` would throw.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {position}: {reason}")]
pub(crate) struct ParseError {
    position: usize,
    reason: &'static str,
}

/// `JSON.parse(text)`.
pub(crate) fn parse(text: &str) -> Result<JsValue, ParseError> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("unexpected content after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> ParseError {
        ParseError {
            position: self.pos,
            reason,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, literal: &str, value: JsValue) -> Result<JsValue, ParseError> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("unexpected token"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<JsValue, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => Ok(JsValue::String(self.string()?)),
            Some(b't') => self.expect_literal("true", JsValue::Bool(true)),
            Some(b'f') => self.expect_literal("false", JsValue::Bool(false)),
            Some(b'n') => self.expect_literal("null", JsValue::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected token")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<JsValue, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.pos += 1;
        let mut object = JsObject::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsValue::Object(object));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a property name"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value(depth)?;
            // An existing key keeps its position and takes the new value, as in JavaScript.
            object.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsValue::Object(object));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JsValue, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsValue::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsValue::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<JsValue, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("invalid number"));
            }
        }
        // Rust's float parsing is correctly rounded, like JavaScript's; out-of-range values
        // become infinities or zeros in both.
        self.text[start..self.pos]
            .parse::<f64>()
            .map(JsValue::Number)
            .map_err(|_| self.error("invalid number"))
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("invalid unicode escape"))?;
        let mut value: u16 = 0;
        for &digit in digits {
            let nibble = char::from(digit)
                .to_digit(16)
                .ok_or_else(|| self.error("invalid unicode escape"))?;
            value = value * 16 + u16::try_from(nibble).unwrap_or(0);
        }
        self.pos += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<JsString, ParseError> {
        self.pos += 1;
        let mut units = Vec::new();
        loop {
            // Copy the run of plain characters up to the next quote, backslash or control.
            let start = self.pos;
            while matches!(self.peek(), Some(byte) if byte != b'"' && byte != b'\\' && byte >= 0x20)
            {
                self.pos += 1;
            }
            units.extend(self.text[start..self.pos].encode_utf16());
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(JsString(units));
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("unterminated string"))?;
                    self.pos += 1;
                    let unit = match escape {
                        b'"' => u16::from(b'"'),
                        b'\\' => u16::from(b'\\'),
                        b'/' => u16::from(b'/'),
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => u16::from(b'\n'),
                        b'r' => u16::from(b'\r'),
                        b't' => u16::from(b'\t'),
                        b'u' => self.hex4()?,
                        _ => return Err(self.error("invalid escape")),
                    };
                    units.push(unit);
                }
                Some(_) => return Err(self.error("control character in string")),
                None => return Err(self.error("unterminated string")),
            }
        }
    }
}

/// `JSON.stringify(value)`.
pub(crate) fn stringify(value: &JsValue) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &JsValue) {
    match value {
        JsValue::Null => out.push_str("null"),
        JsValue::Bool(true) => out.push_str("true"),
        JsValue::Bool(false) => out.push_str("false"),
        JsValue::Number(number) => match serde_json::Number::from_f64(*number) {
            Some(number) => out.push_str(&js_json(&serde_json::Value::Number(number))),
            // Infinity and NaN are written as `null`.
            None => out.push_str("null"),
        },
        JsValue::String(text) => write_string(out, text),
        JsValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        JsValue::Object(object) => {
            let mut indices: Vec<(u32, &JsString, &JsValue)> = object
                .iter()
                .filter_map(|(key, value)| key.array_index().map(|index| (index, key, value)))
                .collect();
            indices.sort_unstable_by_key(|&(index, _, _)| index);
            let names = object.iter().filter(|(key, _)| key.array_index().is_none());
            out.push('{');
            let entries = indices
                .into_iter()
                .map(|(_, key, value)| (key, value))
                .chain(names);
            for (i, (key, value)) in entries.enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, value);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &JsString) {
    out.push('"');
    for decoded in char::decode_utf16(text.0.iter().copied()) {
        match decoded {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            Ok(c) => out.push(c),
            Err(lone) => {
                let _ = write!(out, "\\u{:04x}", lone.unpaired_surrogate());
            }
        }
    }
    out.push('"');
}
