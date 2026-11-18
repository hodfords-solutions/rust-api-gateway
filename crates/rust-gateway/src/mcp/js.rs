//! The JavaScript semantics the reference's MCP code relies on: `String(value)`,
//! `JSON.stringify`, object key order, truthiness, property access and object spread, applied
//! to JSON values (the only values an MCP request or an OpenAPI document can carry).

use serde_json::{Map, Number, Value};

use crate::error::js_json;

/// Whether `key` is an array index (a canonical decimal integer below 2^32 - 1), which a
/// JavaScript object orders before every other key.
pub(crate) fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    let value: u64 = key.parse().ok()?;
    u32::try_from(value).ok().filter(|&v| v != u32::MAX)
}

/// An insertion-ordered string map with JavaScript object key order: array-index keys first
/// in ascending order, then the other keys in insertion order. Assigning to an existing key
/// keeps its position.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct JsMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> JsMap<V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn insert(&mut self, key: String, value: V) {
        if let Some(entry) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// The entries in JavaScript key order.
    pub(crate) fn into_entries(self) -> Vec<(String, V)> {
        let (mut indices, names): (Vec<_>, Vec<_>) = self
            .entries
            .into_iter()
            .partition(|(k, _)| array_index(k).is_some());
        indices.sort_by_key(|(k, _)| array_index(k));
        indices.extend(names);
        indices
    }
}

/// Reorders an object's keys into JavaScript key order (top level only).
pub(crate) fn js_ordered(map: Map<String, Value>) -> Map<String, Value> {
    let mut ordered = JsMap::new();
    for (k, v) in map {
        ordered.insert(k, v);
    }
    ordered.into_entries().into_iter().collect()
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Truthiness of a property that may be absent (`undefined`).
pub(crate) fn truthy_opt(value: Option<&Value>) -> bool {
    value.is_some_and(truthy)
}

/// `Number.prototype.toString()`: JSON numbers are doubles in JavaScript.
pub(crate) fn number_string(n: &Number) -> String {
    js_json(&Value::from(n.as_f64().unwrap_or(0.0)))
}

/// `String(value)`.
pub(crate) fn string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_string(n),
        Value::String(s) => s.clone(),
        Value::Array(items) => join(items),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `String(value)` of a property that may be absent: `undefined` for a missing one.
pub(crate) fn string_opt(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), string)
}

/// `Array.prototype.join(",")`: `null` elements become empty strings.
fn join(items: &[Value]) -> String {
    items
        .iter()
        .map(|item| match item {
            Value::Null => String::new(),
            other => string(other),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `JSON.stringify(value)`: numbers written as JavaScript writes them and object keys in
/// JavaScript order.
pub(crate) fn stringify(value: &Value) -> String {
    js_json(&normalize_for_json(value))
}

fn normalize_for_json(value: &Value) -> Value {
    match value {
        Value::Number(n) => Value::from(n.as_f64().unwrap_or(0.0)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_for_json).collect()),
        Value::Object(map) => Value::Object(js_ordered(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_for_json(v)))
                .collect(),
        )),
        other => other.clone(),
    }
}

/// `value[key]` in JavaScript for a JSON value: own properties of objects, indices and
/// `length` of arrays and strings. Strings are indexed by Unicode scalar value (JavaScript
/// indexes UTF-16 code units; the two agree outside the astral planes).
pub(crate) fn get<'a>(value: &'a Value, key: &str) -> Option<std::borrow::Cow<'a, Value>> {
    use std::borrow::Cow;
    match value {
        Value::Object(map) => map.get(key).map(Cow::Borrowed),
        Value::Array(items) => {
            if key == "length" {
                return Some(Cow::Owned(Value::from(items.len())));
            }
            let index = usize::try_from(array_index(key)?).ok()?;
            items.get(index).map(Cow::Borrowed)
        }
        Value::String(s) => {
            if key == "length" {
                return Some(Cow::Owned(Value::from(s.encode_utf16().count())));
            }
            let index = usize::try_from(array_index(key)?).ok()?;
            s.chars()
                .nth(index)
                .map(|c| Cow::Owned(Value::String(c.to_string())))
        }
        _ => None,
    }
}

/// `{ ...value }`: a shallow copy of an object; arrays and strings spread their indices;
/// every other value spreads nothing.
pub(crate) fn spread(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => js_ordered(map.clone()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        Value::String(s) => s
            .chars()
            .enumerate()
            .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
            .collect(),
        _ => Map::new(),
    }
}

/// The keys `for (const key in value)` visits, with their values, for a JSON value.
pub(crate) fn for_in(value: &Value) -> Vec<(String, Value)> {
    spread(value).into_iter().collect()
}

/// Whether `c` is matched by `\s` in a JavaScript regular expression.
pub(crate) fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_matches_javascript() {
        let cases = [
            (json!(null), "null"),
            (json!(true), "true"),
            (json!(12.0), "12"),
            (json!(1.5), "1.5"),
            (json!(1e21), "1e+21"),
            (json!(1e-7), "1e-7"),
            (json!(-0.0), "0"),
            (json!(12345678901234567890u64), "12345678901234567000"),
            (json!("x"), "x"),
            (
                json!([1, [2, 3], null, { "a": 1 }]),
                "1,2,3,,[object Object]",
            ),
            (json!([]), ""),
            (json!({}), "[object Object]"),
        ];
        for (value, expected) in cases {
            assert_eq!(string(&value), expected, "{value}");
        }
        assert_eq!(string_opt(None), "undefined");
    }

    #[test]
    fn stringify_orders_keys_and_numbers() {
        let value = json!({ "b": 1.0, "2": [1e21], "1": { "z": 1, "10": 2, "3": 3 } });
        assert_eq!(
            stringify(&value),
            r#"{"1":{"3":3,"10":2,"z":1},"2":[1e+21],"b":1}"#
        );
    }

    #[test]
    fn js_map_orders_indices_first_and_keeps_positions() {
        let mut map = JsMap::new();
        for (k, v) in [("b", 1), ("10", 2), ("a", 3), ("2", 4), ("b", 5), ("01", 6)] {
            map.insert(k.to_string(), v);
        }
        let keys: Vec<(String, i32)> = map.into_entries();
        assert_eq!(
            keys,
            [("2", 4), ("10", 2), ("b", 5), ("a", 3), ("01", 6)].map(|(k, v)| (k.to_string(), v))
        );
        assert_eq!(array_index("4294967294"), Some(4_294_967_294));
        assert_eq!(array_index("4294967295"), None);
    }

    #[test]
    fn spread_and_get_follow_javascript() {
        assert_eq!(
            Value::Object(spread(&json!("ab"))),
            json!({ "0": "a", "1": "b" })
        );
        assert_eq!(Value::Object(spread(&json!([1]))), json!({ "0": 1 }));
        assert_eq!(Value::Object(spread(&json!(5))), json!({}));
        assert_eq!(get(&json!([7, 8]), "1").as_deref(), Some(&json!(8)));
        assert_eq!(get(&json!([7, 8]), "length").as_deref(), Some(&json!(2)));
        assert_eq!(get(&json!("xyz"), "2").as_deref(), Some(&json!("z")));
        assert_eq!(get(&json!(null), "a"), None);
    }
}
