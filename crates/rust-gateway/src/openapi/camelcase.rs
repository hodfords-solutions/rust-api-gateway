//! A port of `camelcase-keys` 10.0.3 as the Reference Gateway calls it on each operation
//! object: shallow, default options, which runs `camelcase` 9.0.0 with `locale: false` on
//! every key that is not numeric.
//!
//! JavaScript objects enumerate integer-like keys first, in ascending order, then the other
//! keys in insertion order; the port reproduces that ordering, so `extra` has the keys, the
//! values (for colliding keys) and the order the reference would produce.
//!
//! Golden vectors: `testdata/camelcase-keys.json` (generator:
//! `testdata/generate-camelcase-keys.mjs`).

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Map, Value};

static UPPERCASE: LazyLock<Regex> = LazyLock::new(|| compile(r"^\p{Lu}$"));
static LOWERCASE: LazyLock<Regex> = LazyLock::new(|| compile(r"^\p{Ll}$"));
static LEADING_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| compile(r"^[_.\- ]+"));
static NUMBERS_AND_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"[0-9]+([\p{Alphabetic}\p{N}_]|$)"));
static SEPARATORS_AND_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"[_.\- ]+([\p{Alphabetic}\p{N}_]|$)"));

fn compile(pattern: &str) -> Regex {
    // The patterns are constants; failing to compile one is a programming error caught by
    // every test that converts a key.
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("invalid built-in pattern {pattern}: {error}"))
}

/// Shallow `camelcaseKeys(object)`: every non-numeric key camel-cased, values untouched.
///
/// When two keys convert to the same name, the later value wins at the earlier key's
/// position. A key that converts to `__proto__` is dropped, as `map-obj` does.
pub fn camelcase_keys(object: &Map<String, Value>) -> Map<String, Value> {
    let mut converted: Vec<(String, Value)> = Vec::with_capacity(object.len());
    for (key, value) in js_key_order(object.iter()) {
        let new_key = if is_numeric_key(key) {
            key.clone()
        } else {
            camel_case(key)
        };
        if new_key == "__proto__" {
            continue;
        }
        match converted.iter_mut().find(|(k, _)| *k == new_key) {
            Some(slot) => slot.1 = value.clone(),
            None => converted.push((new_key, value.clone())),
        }
    }
    let ordered: Vec<(String, Value)> = js_key_order(converted.iter().map(|(k, v)| (k, v)))
        .into_iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    ordered.into_iter().collect()
}

/// Orders entries as JavaScript enumerates an object's own string keys: array-index keys
/// (canonical integers below 2^32 - 1) ascending, then the rest in insertion order.
pub(crate) fn js_key_order<'a, I>(entries: I) -> Vec<(&'a String, &'a Value)>
where
    I: Iterator<Item = (&'a String, &'a Value)>,
{
    let (mut indices, others): (Vec<_>, Vec<_>) =
        entries.partition(|(key, _)| array_index(key).is_some());
    indices.sort_by_key(|(key, _)| array_index(key));
    indices.extend(others);
    indices
}

/// The value of `key` when it is an array index: `"0"`, or digits without a leading zero
/// below 4294967295.
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    key.parse::<u32>().ok().filter(|n| *n < u32::MAX)
}

/// `isNumericKey` in `camelcase-keys`: `key.trim() !== '' && !Number.isNaN(Number(key))`.
fn is_numeric_key(key: &str) -> bool {
    let trimmed = js_trim(key);
    !trimmed.is_empty() && is_js_number(trimmed)
}

/// Whether `Number(s)` is not `NaN` for an already trimmed, non-empty `s`
/// (ECMAScript `StringNumericLiteral`).
fn is_js_number(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let digits = &bytes[2..];
        let radix_ok = match bytes[1] {
            b'x' | b'X' => digits.iter().all(u8::is_ascii_hexdigit),
            b'o' | b'O' => digits.iter().all(|b| (b'0'..=b'7').contains(b)),
            b'b' | b'B' => digits.iter().all(|b| *b == b'0' || *b == b'1'),
            _ => false,
        };
        if radix_ok {
            return true;
        }
    }
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned == "Infinity" {
        return true;
    }
    let bytes = unsigned.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let mut mantissa = digits(&mut i);
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        mantissa += digits(&mut i);
    }
    if mantissa == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == bytes.len()
}

/// ECMAScript `WhiteSpace` and `LineTerminator`, which `String.prototype.trim` removes.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{A}' | '\u{B}' | '\u{C}' | '\u{D}' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn is_separator(c: char) -> bool {
    matches!(c, '_' | '.' | '-' | ' ')
}

/// `camelCase(input, { locale: false })` from `camelcase` 9.0.0 with its other defaults
/// (`pascalCase: false`, `preserveConsecutiveUppercase: false`,
/// `capitalizeAfterNumber: true`).
pub fn camel_case(input: &str) -> String {
    let input = js_trim(input);
    if input.is_empty() {
        return String::new();
    }
    // Preserve leading `_` and `$`.
    let prefix_len = input.len() - input.trim_start_matches(['_', '$']).len();
    let (prefix, input) = input.split_at(prefix_len);
    if input.is_empty() {
        return prefix.to_string();
    }
    if input.encode_utf16().count() == 1 {
        if input.chars().all(is_separator) {
            return prefix.to_string();
        }
        return format!("{prefix}{}", input.to_lowercase());
    }

    let mut input = input.to_string();
    if input != input.to_lowercase() {
        input = preserve_camel_case(&input);
    }
    let input = LEADING_SEPARATORS.replace(&input, "").to_lowercase();
    format!("{prefix}{}", post_process(&input))
}

/// `preserveCamelCase`: inserts `-` at lower-to-upper and upper-run-to-word transitions.
/// It walks UTF-16 code units, so characters outside the BMP count as caseless.
fn preserve_camel_case(input: &str) -> String {
    let mut units: Vec<u16> = input.encode_utf16().collect();
    let dash = u16::from(b'-');
    let mut is_last_char_lower = false;
    let mut is_last_char_upper = false;
    let mut is_last_last_char_upper = false;
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        let is_last_last_char_preserved = if index > 2 {
            units[index - 3] == dash
        } else {
            true
        };
        if is_last_char_lower && unit_is(&UPPERCASE, unit) {
            // FooBar -> Foo-Bar
            units.insert(index, dash);
            is_last_char_lower = false;
            is_last_last_char_upper = is_last_char_upper;
            is_last_char_upper = true;
            index += 1;
        } else if is_last_char_upper
            && is_last_last_char_upper
            && unit_is(&LOWERCASE, unit)
            && !is_last_last_char_preserved
        {
            // FOOBar -> FOO-Bar
            units.insert(index - 1, dash);
            is_last_last_char_upper = is_last_char_upper;
            is_last_char_upper = false;
            is_last_char_lower = true;
        } else {
            let (lower, upper) = unit_cases(unit);
            is_last_char_lower = lower && !upper;
            is_last_last_char_upper = is_last_char_upper;
            is_last_char_upper = upper && !lower;
        }
        index += 1;
    }
    String::from_utf16_lossy(&units)
}

/// Whether a single code unit matches a one-character Unicode-mode pattern. Lone
/// surrogates match no property class.
fn unit_is(class: &Regex, unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(|c| class.is_match(c.encode_utf8(&mut [0; 4])))
}

/// For one code unit: (`toLowerCase(c) === c`, `toUpperCase(c) === c`).
fn unit_cases(unit: u16) -> (bool, bool) {
    match char::from_u32(u32::from(unit)) {
        Some(c) => {
            let s = c.to_string();
            (s.to_lowercase() == s, s.to_uppercase() == s)
        }
        None => (true, true),
    }
}

/// `postProcess` with `capitalizeAfterNumber: true`.
fn post_process(input: &str) -> String {
    let after_numbers = NUMBERS_AND_IDENTIFIER.replace_all(input, |caps: &Captures<'_>| {
        let (Some(whole), Some(identifier)) = (caps.get(0), caps.get(1)) else {
            return String::new();
        };
        // A numeric run followed by a separator continues the same word.
        if input[whole.end()..]
            .chars()
            .next()
            .is_some_and(is_separator)
        {
            return whole.as_str().to_string();
        }
        if identifier.as_str().is_empty() {
            return whole.as_str().to_string();
        }
        format!(
            "{}{}",
            &input[whole.start()..identifier.start()],
            identifier.as_str().to_uppercase()
        )
    });
    SEPARATORS_AND_IDENTIFIER
        .replace_all(&after_numbers, |caps: &Captures<'_>| {
            caps.get(1)
                .map(|identifier| identifier.as_str().to_uppercase())
                .unwrap_or_default()
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("testdata/camelcase-keys.json");

    #[test]
    fn golden_keys_match_camelcase_keys() {
        let vectors: Value = serde_json::from_str(VECTORS).expect("golden vectors are JSON");
        assert_eq!(vectors["camelcaseKeysVersion"], "10.0.3");
        let mut failures = Vec::new();
        for case in vectors["keys"].as_array().expect("keys") {
            let input = case["input"].as_str().expect("input");
            let mut object = Map::new();
            object.insert(input.to_string(), Value::Bool(true));
            let converted = camelcase_keys(&object);
            let output: Vec<&String> = converted.keys().collect();
            let actual = (output.len() == 1).then(|| output[0].as_str());
            if actual != case["output"].as_str() {
                failures.push(format!(
                    "{input:?}: {actual:?}, expected {}",
                    case["output"]
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn golden_objects_match_camelcase_keys() {
        let vectors: Value = serde_json::from_str(VECTORS).expect("golden vectors are JSON");
        for case in vectors["objects"].as_array().expect("objects") {
            let input: Map<String, Value> =
                serde_json::from_str(case["input"].as_str().expect("input")).expect("object");
            let actual: Vec<Value> = camelcase_keys(&input)
                .into_iter()
                .map(|(k, v)| Value::Array(vec![Value::String(k), v]))
                .collect();
            assert_eq!(Value::Array(actual), case["output"], "{}", case["input"]);
        }
    }

    #[test]
    fn oas_09_reads_extension_keys_camel_cased() {
        assert_eq!(camel_case("x-rate-limits"), "xRateLimits");
        assert_eq!(camel_case("x-router-path"), "xRouterPath");
        assert_eq!(camel_case("x-api-mcp"), "xApiMcp");
        assert_eq!(camel_case("operationId"), "operationId");
    }
}
