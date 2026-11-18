//! Query re-serialisation (FWD-04, Deviation D9): a port of `node:querystring`.
//!
//! The Reference Gateway's Express 5 parses the normalised query with
//! `querystring.parse(query)` (default options: `&`, `=`, at most 1000 pairs) and undici
//! sends `querystring.stringify(parsed)`. The parsed value is a plain object, so keys come
//! out in JavaScript property order: array-index keys (`0`, `42`) first in numeric order,
//! then the others in first-appearance order, with repeated keys grouped at their first
//! position.

use super::js_object::JsObject;
use super::url::{hex_value, percent_decode_strict};

/// The query string the upstream receives for a normalised query (without the `?`).
/// An empty result means the upstream request has no query at all.
pub fn forwarded_query(query: &str) -> String {
    stringify(&parse(query))
}

/// `querystring.parse(query)`: keys and their values in JavaScript object order. A key that
/// appears once has one value.
pub fn parse(query: &str) -> Vec<(String, Vec<String>)> {
    let mut object: JsObject<Vec<String>> = JsObject::new();
    if query.is_empty() {
        return Vec::new();
    }
    let bytes = query.as_bytes();
    let mut pairs: i32 = 1000;
    let mut last_pos = 0usize;
    let mut sep_idx = 0usize;
    let mut eq_idx = 0usize;
    let mut key = String::new();
    let mut value = String::new();
    let mut key_encoded = false;
    let mut val_encoded = false;
    let mut encode_check = 0u8;
    let mut i = 0usize;
    while i < bytes.len() {
        let code = bytes[i];
        if code == b'&' {
            sep_idx += 1;
            let end = i + 1 - sep_idx;
            if eq_idx < 1 {
                if last_pos < end {
                    key.push_str(&query[last_pos..end]);
                } else if key.is_empty() {
                    pairs -= 1;
                    if pairs == 0 {
                        return object.into_entries();
                    }
                    last_pos = i + 1;
                    sep_idx = 0;
                    eq_idx = 0;
                    i += 1;
                    continue;
                }
            } else if last_pos < end {
                value.push_str(&query[last_pos..end]);
            }
            add_key_value(&mut object, &key, &value, key_encoded, val_encoded);
            pairs -= 1;
            if pairs == 0 {
                return object.into_entries();
            }
            key_encoded = false;
            val_encoded = false;
            key.clear();
            value.clear();
            encode_check = 0;
            last_pos = i + 1;
            sep_idx = 0;
            eq_idx = 0;
        } else {
            sep_idx = 0;
            if eq_idx < 1 {
                if code == b'=' {
                    eq_idx += 1;
                    let end = i + 1 - eq_idx;
                    if last_pos < end {
                        key.push_str(&query[last_pos..end]);
                    }
                    encode_check = 0;
                    last_pos = i + 1;
                    i += 1;
                    continue;
                }
                eq_idx = 0;
                if !key_encoded {
                    if code == b'%' {
                        encode_check = 1;
                        i += 1;
                        continue;
                    } else if encode_check > 0 {
                        if code.is_ascii_hexdigit() {
                            encode_check += 1;
                            if encode_check == 3 {
                                key_encoded = true;
                            }
                            i += 1;
                            continue;
                        }
                        encode_check = 0;
                    }
                }
                if code == b'+' {
                    if last_pos < i {
                        key.push_str(&query[last_pos..i]);
                    }
                    key.push(' ');
                    last_pos = i + 1;
                    i += 1;
                    continue;
                }
            }
            if code == b'+' {
                if last_pos < i {
                    value.push_str(&query[last_pos..i]);
                }
                value.push(' ');
                last_pos = i + 1;
            } else if !val_encoded {
                if code == b'%' {
                    encode_check = 1;
                } else if encode_check > 0 {
                    if code.is_ascii_hexdigit() {
                        encode_check += 1;
                        if encode_check == 3 {
                            val_encoded = true;
                        }
                    } else {
                        encode_check = 0;
                    }
                }
            }
        }
        i += 1;
    }
    if last_pos < bytes.len() {
        if eq_idx < 1 {
            key.push_str(&query[last_pos..]);
        } else if sep_idx < 1 {
            value.push_str(&query[last_pos..]);
        }
    } else if eq_idx == 0 && key.is_empty() {
        return object.into_entries();
    }
    add_key_value(&mut object, &key, &value, key_encoded, val_encoded);
    object.into_entries()
}

fn add_key_value(
    object: &mut JsObject<Vec<String>>,
    key: &str,
    value: &str,
    key_encoded: bool,
    val_encoded: bool,
) {
    let key = if !key.is_empty() && key_encoded {
        unescape(key)
    } else {
        key.to_string()
    };
    let value = if !value.is_empty() && val_encoded {
        unescape(value)
    } else {
        value.to_string()
    };
    match object.get_mut(&key) {
        Some(values) => values.push(value),
        None => object.insert(key, vec![value]),
    }
}

/// `querystring.stringify(object)` for the shape [`parse`] returns.
pub fn stringify(entries: &[(String, Vec<String>)]) -> String {
    let mut fields = String::new();
    for (key, values) in entries {
        if values.is_empty() {
            continue;
        }
        let ks = format!("{}=", escape(key));
        for value in values {
            if !fields.is_empty() {
                fields.push('&');
            }
            fields.push_str(&ks);
            fields.push_str(&escape(value));
        }
    }
    fields
}

/// `querystring.escape`: percent-encodes everything except ASCII letters, digits and
/// `-._~!'()*`, as UTF-8 with upper-case hex.
pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut buf = [0u8; 4];
    for c in input.chars() {
        if c.is_ascii_alphanumeric()
            || matches!(c, '-' | '.' | '_' | '~' | '!' | '\'' | '(' | ')' | '*')
        {
            out.push(c);
        } else {
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

/// `querystring.unescape`: `decodeURIComponent`, or where that throws, a byte-wise decode of
/// the valid `%XX` escapes read back as UTF-8 with replacement characters.
pub fn unescape(input: &str) -> String {
    if let Some(decoded) = percent_decode_strict(input) {
        return decoded;
    }
    // `unescapeBuffer`: works on UTF-16 code units, each truncated to a byte.
    let units: Vec<u16> = input.encode_utf16().collect();
    let max_length = units.len().saturating_sub(2);
    let mut out = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let mut current = units[index];
        if current == u16::from(b'%') && index < max_length {
            index += 1;
            let high = hex_unit(units[index]);
            match high {
                None => {
                    out.push(b'%');
                    continue;
                }
                Some(high) => {
                    current = units[index];
                    index += 1;
                    match hex_unit(units[index]) {
                        None => {
                            out.push(b'%');
                            index -= 1;
                        }
                        Some(low) => current = u16::from(high * 16 + low),
                    }
                }
            }
        }
        out.push(current.to_le_bytes()[0]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_unit(unit: u16) -> Option<u8> {
    u8::try_from(unit).ok().and_then(hex_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::golden;

    fn pairs(entries: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        entries
            .iter()
            .map(|(k, vs)| (k.to_string(), vs.iter().map(|v| v.to_string()).collect()))
            .collect()
    }

    #[test]
    fn parse_groups_repeated_keys_and_orders_like_an_object() {
        assert_eq!(
            parse("a=1&b=2&a=3"),
            pairs(&[("a", &["1", "3"]), ("b", &["2"])])
        );
        assert_eq!(
            parse("b=1&2=x&1=y"),
            pairs(&[("1", &["y"]), ("2", &["x"]), ("b", &["1"])])
        );
        assert_eq!(parse("a&b="), pairs(&[("a", &[""]), ("b", &[""])]));
        assert_eq!(parse("&&a=1&&"), pairs(&[("a", &["1"])]));
        assert_eq!(parse("=v"), pairs(&[("", &["v"])]));
        assert!(parse("").is_empty());
    }

    #[test]
    fn values_are_decoded_only_when_they_hold_a_valid_escape() {
        assert_eq!(parse("q=%zz"), pairs(&[("q", &["%zz"])]));
        assert_eq!(parse("q=%41%zz"), pairs(&[("q", &["A%zz"])]));
        assert_eq!(parse("q=a+b&q=%2B"), pairs(&[("q", &["a b", "+"])]));
        assert_eq!(parse("q=%e9"), pairs(&[("q", &["\u{fffd}"])]));
    }

    // The table of forwarding::fwd_04_query_strings, after URL normalisation.
    #[test]
    fn matches_the_conformance_table() {
        for (query, expected) in [
            ("a=1&b=2&a=3", "a=1&a=3&b=2"),
            ("a", "a="),
            ("q=a+b", "q=a%20b"),
            ("q=%41", "q=A"),
            ("a[b]=1", "a%5Bb%5D=1"),
            ("a=1=2", "a=1%3D2"),
            ("a=1;b=2", "a=1%3Bb%3D2"),
            ("q=it%27s", "q=it's"),
            ("q=~!*()", "q=~!*()"),
            ("q=%22x%22", "q=%22x%22"),
            ("q={x}|`y`^", "q=%7Bx%7D%7C%60y%60%5E"),
            ("__proto__=x", "__proto__=x"),
            ("&&", ""),
        ] {
            assert_eq!(forwarded_query(query), expected, "{query:?}");
        }
    }

    #[test]
    fn stops_after_1000_pairs() {
        let query: Vec<String> = (0..1005).map(|i| format!("k{i}={i}")).collect();
        assert_eq!(parse(&query.join("&")).len(), 1000);
    }

    #[test]
    fn matches_node_golden_vectors() {
        let mut failures = Vec::new();
        for case in golden::cases(golden::QUERY) {
            let raw = case[0].as_str().unwrap();
            let (Some(normalised), Some(expected)) = (case[1].as_str(), case[2].as_str()) else {
                continue;
            };
            let url = crate::request::normalize_request_url(&format!("/p?{raw}")).ok();
            let rust_normalised = url
                .as_deref()
                .map(|u| u.split_once('?').map(|(_, q)| q).unwrap_or(""));
            if rust_normalised != Some(normalised) {
                failures.push(format!(
                    "{raw:?}: node normalises to {normalised:?}, rust {rust_normalised:?}"
                ));
                continue;
            }
            let actual = forwarded_query(normalised);
            if actual != expected {
                failures.push(format!(
                    "{normalised:?}: node {expected:?}, rust {actual:?}"
                ));
            }
        }
        golden::assert_no_failures("query.json", &failures);
    }
}
