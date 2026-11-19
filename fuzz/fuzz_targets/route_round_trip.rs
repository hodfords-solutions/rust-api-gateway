//! A pattern built from literal segments, `:params` and a trailing `*wildcard` must match the
//! path obtained by substituting percent-encoded values, and give those values back decoded.
#![no_main]

use api_gateway::route::PathPattern;
use api_gateway::route::pattern::ParamValue;
use api_gateway_fuzz::encode_uri_component;
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Segment {
    /// A literal segment (letters, digits and `-` only), written in the path in upper case
    /// when the flag is set (matching is case-insensitive).
    Literal(String, bool),
    /// A `:param` and the value substituted for it.
    Param(String),
}

#[derive(Arbitrary, Debug)]
struct Input {
    segments: Vec<Segment>,
    wildcard: Option<Vec<String>>,
    trailing_slash: bool,
}

fuzz_target!(|input: Input| {
    if input.segments.len() > 16 {
        return;
    }
    let mut pattern = String::new();
    let mut path = String::new();
    let mut expected: Vec<(String, ParamValue)> = Vec::new();
    for (i, segment) in input.segments.iter().enumerate() {
        match segment {
            Segment::Literal(text, upper) => {
                let text: String = text
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect::<String>()
                    .to_ascii_lowercase();
                if text.is_empty() {
                    return;
                }
                pattern.push('/');
                pattern.push_str(&text);
                path.push('/');
                path.push_str(&if *upper {
                    text.to_ascii_uppercase()
                } else {
                    text
                });
            }
            Segment::Param(value) => {
                if value.is_empty() {
                    return;
                }
                let name = format!("p{i}");
                pattern.push_str(&format!("/:{name}"));
                path.push('/');
                path.push_str(&encode_uri_component(value));
                expected.push((name, ParamValue::Single(value.clone())));
            }
        }
    }
    if let Some(parts) = &input.wildcard {
        let joined: Vec<String> = parts.iter().map(|p| encode_uri_component(p)).collect();
        let joined = joined.join("/");
        if joined.is_empty() || parts.len() > 16 {
            return;
        }
        pattern.push_str("/*rest");
        path.push('/');
        path.push_str(&joined);
        expected.push(("rest".into(), ParamValue::Segments(parts.clone())));
    } else if input.trailing_slash {
        // `trailing: true`: one trailing slash is allowed.
        path.push('/');
    }
    if pattern.is_empty() {
        return;
    }
    let compiled = PathPattern::compile(&pattern)
        .unwrap_or_else(|e| panic!("generated pattern {pattern:?} rejected: {e}"));
    let found = compiled
        .matches(&path)
        .unwrap_or_else(|e| panic!("{pattern:?} on {path:?}: {e}"))
        .unwrap_or_else(|| panic!("{pattern:?} does not match {path:?}"));
    assert_eq!(found.params, expected, "{pattern:?} on {path:?}");
});
