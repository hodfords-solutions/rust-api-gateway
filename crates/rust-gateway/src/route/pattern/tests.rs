//! Golden-vector tests: every pattern and input in `testdata/path-to-regexp.json`, produced
//! by running the installed `path-to-regexp` under Node.

use std::collections::BTreeMap;

use serde_json::Value;

use super::*;

const VECTORS: &str = include_str!("../testdata/path-to-regexp.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("golden vectors are JSON")
}

fn flatten_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|item| flatten_paths(item, out)),
        other => panic!("unexpected pattern {other}"),
    }
}

fn compile(pattern: &Value) -> Result<PathPattern, PathError> {
    let mut paths = Vec::new();
    flatten_paths(pattern, &mut paths);
    PathPattern::compile_all(&paths)
}

fn params_json(params: &[(String, ParamValue)]) -> BTreeMap<String, Value> {
    params
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ParamValue::Single(s) => Value::String(s.clone()),
                ParamValue::Segments(parts) => {
                    Value::Array(parts.iter().cloned().map(Value::String).collect())
                }
            };
            (name.clone(), value)
        })
        .collect()
}

#[test]
fn golden_vectors_match_path_to_regexp() {
    let vectors = vectors();
    assert_eq!(vectors["pathToRegexpVersion"], "8.4.2");
    let mut failures = Vec::new();
    let mut checked = 0;
    for entry in vectors["patterns"].as_array().expect("patterns") {
        let pattern = &entry["pattern"];
        let compiled = compile(pattern);
        if let Some(expected) = entry.get("error").and_then(Value::as_str) {
            match compiled {
                Err(error) => {
                    let actual = format!("TypeError: {}", error.message());
                    if actual != expected {
                        failures.push(format!(
                            "{pattern}: error {actual:?}, expected {expected:?}"
                        ));
                    }
                }
                Ok(p) => failures.push(format!(
                    "{pattern}: compiled to {}, expected error {expected:?}",
                    p.regexp_source()
                )),
            }
            continue;
        }
        let compiled = match compiled {
            Ok(p) => p,
            Err(error) => {
                failures.push(format!("{pattern}: unexpected error {error}"));
                continue;
            }
        };
        if let Some(source) = entry["source"].as_str()
            && compiled.regexp_source() != source
        {
            failures.push(format!(
                "{pattern}: source {:?}, expected {source:?}",
                compiled.regexp_source()
            ));
        }
        let keys: Vec<Value> = compiled
            .keys()
            .iter()
            .map(|k| {
                serde_json::json!({
                    "name": k.name,
                    "type": match k.kind { ParamKind::Param => "param", ParamKind::Wildcard => "wildcard" },
                })
            })
            .collect();
        if Value::Array(keys.clone()) != entry["keys"] {
            failures.push(format!(
                "{pattern}: keys {keys:?}, expected {}",
                entry["keys"]
            ));
        }
        for case in entry["cases"].as_array().expect("cases") {
            checked += 1;
            let input = case["input"].as_str().expect("input");
            let expected = &case["result"];
            let actual = compiled.matches(input);
            let ok = match (&actual, expected) {
                (Ok(None), Value::Bool(false)) => true,
                (Ok(Some(m)), Value::Object(e)) if e.contains_key("path") => {
                    let expected_params: BTreeMap<String, Value> =
                        serde_json::from_value(e["params"].clone()).expect("params");
                    m.path == e["path"].as_str().unwrap_or_default()
                        && params_json(&m.params) == expected_params
                }
                (Err(_), Value::Object(e)) => e["error"] == "URIError: URI malformed",
                _ => false,
            };
            if !ok {
                failures.push(format!(
                    "{pattern} on {input:?}: {actual:?}, expected {expected}"
                ));
            }
            if compiled.is_match(input) != !matches!(expected, Value::Bool(false)) {
                failures.push(format!("{pattern} on {input:?}: is_match disagrees"));
            }
        }
    }
    assert!(checked > 3000, "only {checked} cases checked");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn identifier_tables_match_node() {
    let vectors = vectors();
    let ranges = |key: &str| -> Vec<(u32, u32)> {
        serde_json::from_value(vectors[key].clone()).expect("ranges")
    };
    assert_eq!(id_tables::ID_START, ranges("idStart").as_slice());
    assert_eq!(id_tables::ID_CONTINUE, ranges("idContinue").as_slice());
    assert!(is_id_start('a') && is_id_start('$') && is_id_start('_') && is_id_start('é'));
    assert!(!is_id_start('1') && !is_id_start('-') && !is_id_start('\u{200c}'));
    assert!(is_id_continue('1') && is_id_continue('\u{200c}') && is_id_continue('\u{200d}'));
    assert!(!is_id_continue('-') && !is_id_continue('.'));
}

#[test]
fn canonicalize_follows_ecmascript() {
    assert_eq!(canonicalize(u16::from(b'a')), u16::from(b'A'));
    // Non-ASCII never maps to ASCII: long s and the Kelvin sign stay themselves.
    assert_eq!(canonicalize(0x017F), 0x017F);
    assert_eq!(canonicalize(0x212A), 0x212A);
    // A multi-character upper case (ß -> SS) leaves the unit alone.
    assert_eq!(canonicalize(0x00DF), 0x00DF);
    assert_eq!(canonicalize(0x00E9), 0x00C9);
    assert_eq!(canonicalize(0xD83D), 0xD83D);
}

#[test]
fn oas_19_semantics() {
    let p = PathPattern::compile("/v1/users/:id").unwrap();
    assert!(p.is_match("/v1/users/42"));
    assert!(p.is_match("/V1/USERS/42"));
    assert!(p.is_match("/v1/users/42/"));
    assert!(!p.is_match("/v1/users/42//"));
    assert!(!p.is_match("/v1/users/"));
    assert!(!p.is_match("/v1/users/42/x"));
    let m = p.matches("/v1/users/a%2Fb").unwrap().unwrap();
    assert_eq!(
        m.params,
        vec![("id".into(), ParamValue::Single("a/b".into()))]
    );
    assert!(p.is_match("/v1/users/a;b"));
    assert!(p.matches("/v1/users/%zz").is_err());
}

#[test]
fn oas_20_router_path_syntax() {
    let files = PathPattern::compile("/v1/paths/files/*path").unwrap();
    assert!(files.is_match("/v1/paths/files/a/b/c"));
    assert!(!files.is_match("/v1/paths/files/"));
    assert!(!files.is_match("/v1/paths/files"));
    let optional = PathPattern::compile("/v1/paths/optional{/:id}").unwrap();
    assert!(optional.is_match("/v1/paths/optional"));
    assert!(optional.is_match("/v1/paths/optional/5"));
    assert!(!optional.is_match("/v1/paths/optional/5/6"));
    let escaped = PathPattern::compile("/v1/paths/escaped\\:literal").unwrap();
    assert!(escaped.is_match("/v1/paths/escaped:literal"));
    assert!(!escaped.is_match("/v1/paths/escaped%3Aliteral"));
    let quoted = PathPattern::compile("/:\"quoted name\"").unwrap();
    assert_eq!(quoted.keys()[0].name, "quoted name");
}
