//! Unit tests for area 07. Spec ports name the case of
//! `restful/services/open-api.service.spec.ts` they come from; `D5`/`D6` tests are the
//! Rust-only tests of those Deviations.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use http::{Method, StatusCode, header};
use serde_json::{Value, json};

use super::json::{self, JsValue};
use super::page::script_string_literal;
use super::*;
use crate::openapi::{FetchError, FetchFuture, FetchedDocument, SecurityKeys};
use crate::upstream::UpstreamService;

/// The reference's page template (`views/document.hbs`, 12.4.2), copied verbatim.
const REFERENCE_TEMPLATE: &str = include_str!("testdata/document.hbs");

const BEARER: &str = r#"{"scheme":"bearer","bearerFormat":"JWT","type":"http"}"#;

fn options() -> DocumentOptions {
    DocumentOptions {
        exclude_headers: vec!["auth-user-id".into(), "permission-in-any-guard".into()],
        open_api_security_keys: vec!["auth-user-id".into()],
        hide_document_ids: Vec::new(),
    }
}

fn transform(prefix: &str, document: &str, options: &DocumentOptions) -> String {
    transform_document(prefix, document.as_bytes(), options).expect("a document")
}

// ---------------------------------------------------------------------------------------
// Spec ports: "OpenApiService document mutation helpers" and "getDocumentDetailsForUI".

// Spec: "removeSecuritySchemes deletes only auth-* schemes" (DOC-07).
#[test]
fn doc_07_removes_only_auth_schemes() {
    let doc = transform(
        "svc",
        r#"{"components":{"securitySchemes":{"auth-jwt":{},"bearer":{},"api-key":{}}}}"#,
        &options(),
    );
    assert_eq!(
        doc,
        format!(
            r#"{{"components":{{"securitySchemes":{{"bearer":{BEARER},"api-key":{{}}}}}},"servers":[{{"url":"/svc"}}]}}"#
        )
    );
}

// Spec: "changeSecurityOfPath swaps a scheme name in place" (DOC-09).
#[test]
fn doc_09_renames_security_requirements_to_bearer() {
    let options = DocumentOptions {
        open_api_security_keys: vec!["auth-jwt".into()],
        ..options()
    };
    let doc = transform(
        "svc",
        r#"{"paths":{"/a":{"get":{"security":[{"auth-jwt":["scope"]},{"other":[]}]}}},"components":{}}"#,
        &options,
    );
    let doc: Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(
        doc["paths"]["/a"]["get"]["security"],
        json!([{ "bearer": [] }, { "other": [] }])
    );
}

// Spec: "removeHeader filters out excluded header parameters" (DOC-08).
#[test]
fn doc_08_removes_excluded_header_parameters() {
    let options = DocumentOptions {
        exclude_headers: vec!["auth-user-id".into()],
        ..options()
    };
    let doc = transform(
        "svc",
        r#"{"paths":{"/users":{"get":{"parameters":[{"in":"header","name":"Auth-User-Id"},{"in":"header","name":"x-keep"},{"in":"query","name":"auth-user-id"}]}}},"components":{}}"#,
        &options,
    );
    let doc: Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(
        doc["paths"]["/users"]["get"]["parameters"],
        json!([
            { "in": "header", "name": "x-keep" },
            { "in": "query", "name": "auth-user-id" }
        ])
    );
}

/// The embedded JSON values of a page: the source list, the options and the default.
fn page_values(html: &str) -> (Value, Value, String) {
    let literal = |marker: &str| -> String {
        let start = html.find(marker).expect(marker) + marker.len();
        let mut de = serde_json::Deserializer::from_str(&html[start..]);
        serde::Deserialize::deserialize(&mut de).expect("a JSON string literal")
    };
    (
        serde_json::from_str(&literal("const apiDocs = JSON.parse(")).unwrap(),
        serde_json::from_str(&literal("const scalarOptions = JSON.parse(")).unwrap(),
        literal("let type = urlParams.get('type') || "),
    )
}

// Spec: "lists every loaded document and the last one as default" (DOC-02).
#[test]
fn doc_02_page_lists_every_loaded_document_and_the_last_as_default() {
    let html = render_page(&["users".into(), "orders".into()], None);
    let (sources, options, default) = page_values(&html);
    assert_eq!(
        sources,
        json!([
            { "title": "users", "slug": "users", "url": "document-json?type=users" },
            { "title": "orders", "slug": "orders", "url": "document-json?type=orders" }
        ])
    );
    assert_eq!(default, "orders");
    assert_eq!(options, json!({}));
}

// ---------------------------------------------------------------------------------------
// /document-json transformation.

// DOC-06, DOC-12: the bearer scheme replaces an existing one in place, is added last
// otherwise, and survives the `auth-` removal that runs after it.
#[test]
fn doc_06_doc_12_bearer_scheme_is_set_before_auth_schemes_are_removed() {
    let doc = transform(
        "svc",
        r#"{"components":{"securitySchemes":{"auth-x":{},"bearer":{"type":"apiKey"},"z":{}}}}"#,
        &options(),
    );
    assert!(doc.contains(&format!(
        r#""securitySchemes":{{"bearer":{BEARER},"z":{{}}}}"#
    )));
    let doc = transform("svc", r#"{"components":{"schemas":{}},"x":1}"#, &options());
    assert_eq!(
        doc,
        format!(
            r#"{{"components":{{"schemas":{{}},"securitySchemes":{{"bearer":{BEARER}}}}},"x":1,"servers":[{{"url":"/svc"}}]}}"#
        )
    );
    for falsy in ["null", "false", "0", "\"\""] {
        let doc = transform(
            "svc",
            &format!(r#"{{"components":{{"securitySchemes":{falsy},"y":1}}}}"#),
            &options(),
        );
        assert!(
            doc.starts_with(&format!(
                r#"{{"components":{{"securitySchemes":{{"bearer":{BEARER}}},"y":1}}"#
            )),
            "{falsy}: {doc}"
        );
    }
}

// DOC-08: entries are compared as configured, so an upper-case entry never matches.
#[test]
fn doc_08_exclude_headers_entries_are_not_lower_cased() {
    let options = DocumentOptions {
        exclude_headers: vec!["X-Upper".into(), "x-lower".into()],
        ..options()
    };
    let doc = transform(
        "svc",
        r#"{"paths":{"/a":{"post":{"parameters":[{"in":"header","name":"X-Upper"},{"in":"header","name":"X-LOWER"},{"in":"cookie","name":"x-lower"}]}}},"components":{}}"#,
        &options,
    );
    let doc: Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(
        doc["paths"]["/a"]["post"]["parameters"],
        json!([
            { "in": "header", "name": "X-Upper" },
            { "in": "cookie", "name": "x-lower" }
        ])
    );
}

// DOC-09: the renamed requirement moves last; an existing `bearer` keeps its place; every
// configured key is renamed, in order.
#[test]
fn doc_09_renamed_requirements_follow_javascript_key_order() {
    let options = DocumentOptions {
        open_api_security_keys: vec!["a".into(), "b".into(), "bearer".into()],
        ..options()
    };
    let doc = transform(
        "svc",
        r#"{"paths":{"/p":{"get":{"security":[{"a":[],"x":[]},{"bearer":["s"],"b":[]},{"x":[],"bearer":["s"],"y":[]},{},null,"a"]}}},"components":{}}"#,
        &options,
    );
    assert!(
        doc.contains(r#""security":[{"x":[],"bearer":[]},{"bearer":[]},{"x":[],"y":[],"bearer":[]},{},null,"a"]"#),
        "{doc}"
    );
}

// DOC-10: a hidden operation removes its path item, whichever method it is (D5: the
// reference throws when it is not the last method).
#[test]
fn doc_10_d5_hidden_operation_removes_its_path_item() {
    let options = DocumentOptions {
        hide_document_ids: vec!["hide".into()],
        ..options()
    };
    let doc = transform(
        "svc",
        r#"{"paths":{"/a":{"get":{"operationId":"hide"},"post":{"operationId":"keep","security":[{"auth-user-id":[]}]}},"/b":{"get":{"operationId":"keep2"},"delete":{"operationId":"hide"}},"/c":{"get":{"operationId":"other"}}},"components":{}}"#,
        &options,
    );
    let doc: Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(
        doc["paths"],
        json!({ "/c": { "get": { "operationId": "other" } } })
    );
}

// DOC-11: `servers` is replaced in place or added last, except for the Default Service.
#[test]
fn doc_11_servers_is_the_prefix_except_for_the_default_service() {
    let document = r#"{"servers":[{"url":"https://x"}],"paths":{},"components":{}}"#;
    assert!(transform("svc", document, &options()).starts_with(r#"{"servers":[{"url":"/svc"}],"#));
    assert!(
        transform("default", document, &options())
            .starts_with(r#"{"servers":[{"url":"https://x"}],"#)
    );
    let document = r#"{"paths":{},"components":{}}"#;
    assert!(transform("svc", document, &options()).ends_with(r#","servers":[{"url":"/svc"}]}"#));
    assert!(!transform("default", document, &options()).contains("servers"));
}

// DOC-15 (D5): shapes the reference throws on are served, the odd parts left alone.
#[test]
fn doc_15_d5_unexpected_shapes_do_not_fail() {
    let options = DocumentOptions {
        hide_document_ids: vec!["hide".into()],
        ..options()
    };
    let cases = [
        // An operation without `parameters`.
        (
            r#"{"paths":{"/x":{"get":{"responses":{}}}},"components":{}}"#,
            format!(
                r#"{{"paths":{{"/x":{{"get":{{"responses":{{}}}}}}}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
        // A document without `components`, or with `components: null`.
        (
            r#"{"paths":{}}"#,
            format!(
                r#"{{"paths":{{}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
        (
            r#"{"components":null,"paths":{}}"#,
            format!(
                r#"{{"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"paths":{{}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
        // Path-item keys that are not operations.
        (
            r##"{"paths":{"/x":{"summary":"s","$ref":"#/r","parameters":[{"in":"header","name":"auth-user-id"}],"servers":[],"get":{"parameters":[{"in":"header","name":"auth-user-id"}]},"put":null}},"components":{}}"##,
            format!(
                r##"{{"paths":{{"/x":{{"summary":"s","$ref":"#/r","parameters":[{{"in":"header","name":"auth-user-id"}}],"servers":[],"get":{{"parameters":[]}},"put":null}}}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"##
            ),
        ),
        // Non-object `components` and `securitySchemes`, which the reference throws on
        // (primitives) or silently ignores (arrays): left alone.
        (
            r#"{"components":"x","paths":{}}"#,
            r#"{"components":"x","paths":{},"servers":[{"url":"/svc"}]}"#.to_string(),
        ),
        (
            r#"{"components":[],"paths":{}}"#,
            r#"{"components":[],"paths":{},"servers":[{"url":"/svc"}]}"#.to_string(),
        ),
        (
            r#"{"components":{"securitySchemes":["auth-x"]}}"#,
            r#"{"components":{"securitySchemes":["auth-x"]},"servers":[{"url":"/svc"}]}"#
                .to_string(),
        ),
        // Odd parameters, security and paths.
        (
            r#"{"paths":{"/x":{"get":{"parameters":[null,1,{"in":"header"},{"in":"header","name":7},{"in":"header","name":"auth-user-id"}],"security":{"auth-user-id":[]},"operationId":["hide"]},"post":{"parameters":{"a":1},"security":"auth-user-id"}}},"components":{}}"#,
            format!(
                r#"{{"paths":{{"/x":{{"get":{{"parameters":[null,1,{{"in":"header"}},{{"in":"header","name":7}}],"security":{{"auth-user-id":[]}},"operationId":["hide"]}},"post":{{"parameters":{{"a":1}},"security":"auth-user-id"}}}}}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
        (
            r#"{"paths":"x","components":{}}"#,
            format!(
                r#"{{"paths":"x","components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
        (
            r#"{"paths":{"/x":null,"/y":[{"operationId":"hide"}]},"components":{}}"#,
            format!(
                r#"{{"paths":{{"/x":null,"/y":[{{"operationId":"hide"}}]}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
            ),
        ),
    ];
    for (document, expected) in cases {
        assert_eq!(transform("svc", document, &options), expected, "{document}");
    }
}

// DOC-13: a body that is not a JSON object is not a document.
#[test]
fn doc_13_bodies_that_are_not_json_objects_are_refused() {
    for body in [
        "",
        " ",
        "not json",
        "[1]",
        "\"s\"",
        "1",
        "null",
        "true",
        "{",
        "{}x",
        r#"{"a":1,}"#,
    ] {
        assert_eq!(
            transform_document("svc", body.as_bytes(), &options()),
            Err(NotADocument),
            "{body:?}"
        );
    }
}

// DOC-05: the document is re-serialised exactly as `JSON.stringify` would (the same case
// as the conformance scenario `documents::doc_05_serialisation`).
#[test]
fn doc_05_serialisation_follows_javascript() {
    let document = r#"{"openapi":"3.0.0","b":1,"10":2,"2":3,"01":4,"-1":5,"4294967294":6,"4294967295":7,"n":[1.0,1e21,1E5,12345678901234567890,9007199254740993,-0,0.1,1e-7,123456789012345678901234567890,1e400,-1e-400,5e-324,0.000001],"s":"a\/b\u2028\u2029\ud800x\udc00\u001f\u007f<>&é\ud83d\ude00\b\f\n\r\t\"\\","dup":1,"dup":2,"__proto__":{"x":1},"paths":{"/x":{"get":{"parameters":[],"responses":{"201":{},"200":{},"default":{}}}}},"components":{}}"#;
    let expected = "{\"2\":3,\"10\":2,\"4294967294\":6,\"openapi\":\"3.0.0\",\"b\":1,\"01\":4,\"-1\":5,\"4294967295\":7,\"n\":[1,1e+21,100000,12345678901234567000,9007199254740992,0,0.1,1e-7,1.2345678901234568e+29,null,0,5e-324,0.000001],\"s\":\"a/b\u{2028}\u{2029}\\ud800x\\udc00\\u001f\u{7f}<>&é😀\\b\\f\\n\\r\\t\\\"\\\\\",\"dup\":2,\"__proto__\":{\"x\":1},\"paths\":{\"/x\":{\"get\":{\"parameters\":[],\"responses\":{\"200\":{},\"201\":{},\"default\":{}}}}},\"components\":{\"securitySchemes\":{\"bearer\":{\"scheme\":\"bearer\",\"bearerFormat\":\"JWT\",\"type\":\"http\"}}},\"servers\":[{\"url\":\"/svc\"}]}";
    assert_eq!(transform("svc", document, &options()), expected);

    let mut with_bom = "\u{feff}".as_bytes().to_vec();
    with_bom.extend_from_slice(b" {\"paths\":{},\"components\":{},\"bad\":\"\xff\xfe\"}\r\n");
    assert_eq!(
        transform_document("svc", &with_bom, &options()).unwrap(),
        format!(
            "{{\"paths\":{{}},\"components\":{{\"securitySchemes\":{{\"bearer\":{BEARER}}}}},\"bad\":\"\u{fffd}\u{fffd}\",\"servers\":[{{\"url\":\"/svc\"}}]}}"
        )
    );
}

#[test]
fn json_parse_follows_the_json_grammar() {
    for invalid in [
        "01",
        "1.",
        ".5",
        "+1",
        "-",
        "1e",
        "1e+",
        "'a'",
        "NaN",
        "Infinity",
        "[1,]",
        "{\"a\"}",
        "{\"a\":}",
        "{a:1}",
        "\"\\x\"",
        "\"\\u12\"",
        "\"\\u12g4\"",
        "\"a\nb\"",
        "\"a",
        "tru",
        "nul",
        "[",
        "\u{a0}1",
    ] {
        assert!(json::parse(invalid).is_err(), "{invalid:?} parsed");
    }
    for (valid, written) in [
        (" \t\r\n1 ", "1"),
        ("-0.0e-0", "0"),
        ("1E+2", "100"),
        ("\"\\u00e9\\u00E9\"", "\"éé\""),
        ("[]", "[]"),
        ("{}", "{}"),
        ("[true,false,null]", "[true,false,null]"),
        ("{\"\":1,\"a\":{\"b\":[{}]}}", "{\"\":1,\"a\":{\"b\":[{}]}}"),
    ] {
        assert_eq!(
            json::stringify(&json::parse(valid).unwrap()),
            written,
            "{valid:?}"
        );
    }
}

#[test]
fn deep_nesting_is_refused_beyond_the_limit() {
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let within = format!("{{\"a\":{}}}", nested(json::MAX_DEPTH - 1));
    assert!(json::parse(&within).is_ok());
    let beyond = format!("{{\"a\":{}}}", nested(json::MAX_DEPTH));
    assert!(json::parse(&beyond).is_err());
}

// ---------------------------------------------------------------------------------------
// /documents page.

/// Values that break the reference's page: they end the template literal, interpolate,
/// change escapes, end the script, or end the string (U+2028 in older engines).
const HOSTILE: &[&str] = &[
    "</script><script>alert(1)</script>",
    "</SCRIPT >",
    "`",
    "${alert(1)}",
    "\\",
    "\\u0041",
    "\"'",
    "<!--",
    "a\u{2028}b\u{2029}c",
    "\n\r\t\u{0}\u{7f}",
    "&amp;",
];

/// `script` with the contents of every double-quoted literal removed, and the removed
/// contents.
fn blank_string_literals(script: &str) -> (String, String) {
    let (mut code, mut contents) = (String::new(), String::new());
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        code.push(c);
        if c != '"' {
            continue;
        }
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    code.push('"');
                    break;
                }
                '\\' => {
                    contents.push(c);
                    contents.extend(chars.next());
                }
                c => contents.push(c),
            }
        }
    }
    (code, contents)
}

/// The script element's content.
fn inline_script(html: &str) -> &str {
    let start = html.find("<script>\n").expect("inline script") + "<script>\n".len();
    let end = html[start..].find("</script>").expect("script end") + start;
    &html[start..end]
}

// DOC-04 (D6): hostile service prefixes and options cannot break out of the script, and
// round-trip exactly through `JSON.parse`.
#[test]
fn doc_04_d6_embedded_values_cannot_break_out() {
    let services: Vec<String> = HOSTILE.iter().map(|s| s.to_string()).collect();
    let mut options = serde_json::Map::new();
    for (i, value) in HOSTILE.iter().enumerate() {
        options.insert(format!("k{i}{value}"), json!(value));
    }
    let options = Value::Object(options);
    let html = render_page(&services, Some(&options));

    // At the HTML level, nothing in the script element can end it or open a comment.
    let script = inline_script(&html);
    let lower = script.to_ascii_lowercase();
    assert!(!lower.contains("</script"), "{script}");
    assert!(!lower.contains("<!--"), "{script}");
    assert_eq!(
        html.matches("</script>").count(),
        2,
        "only the two closing tags"
    );
    // At the JavaScript level, the values stay inside double-quoted literals: with their
    // contents blanked, the script is the one rendered for harmless values. Line
    // terminators that would end a literal never appear raw.
    let (code, contents) = blank_string_literals(script);
    for forbidden in ['\n', '\r', '\u{2028}', '\u{2029}'] {
        assert!(
            !contents.contains(forbidden),
            "{forbidden:?} inside a literal"
        );
    }
    let harmless = render_page(&["a".into()], Some(&json!({ "k": "v" })));
    assert_eq!(code, blank_string_literals(inline_script(&harmless)).0);

    let (sources, embedded_options, default) = page_values(&html);
    let expected: Vec<Value> = HOSTILE
        .iter()
        .map(|p| json!({ "title": p, "slug": p, "url": format!("document-json?type={p}") }))
        .collect();
    assert_eq!(sources, Value::Array(expected));
    assert_eq!(
        serde_json::to_string(&embedded_options).unwrap(),
        serde_json::to_string(&options).unwrap()
    );
    assert_eq!(default, *HOSTILE.last().unwrap());
}

#[test]
fn script_string_literals_are_json_string_literals() {
    for value in HOSTILE.iter().copied().chain(["", "plain", "é😀"]) {
        let literal = script_string_literal(value);
        let decoded: String = serde_json::from_str(&literal).unwrap();
        assert_eq!(decoded, value);
        assert!(!literal[1..literal.len() - 1].contains('"') || literal.contains("\\\""));
    }
}

// DOC-04 (D6): `?type=` is compared with the sources' `slug`, the chosen source is passed
// with `default: true`, and the fallback is the last loaded service.
#[test]
fn doc_04_d6_type_selects_the_source_by_slug() {
    let html = render_page(&["a".into(), "b".into()], None);
    assert!(html.contains("    let type = urlParams.get('type') || \"b\";\n"));
    assert!(html.contains(
        "    if (!apiDocs.some((doc) => doc.slug === type)){\n        type = \"b\"\n    }\n"
    ));
    assert!(html.contains(
        "        sources: apiDocs.map((doc) => (doc.slug === type ? { ...doc, default: true } : doc)),\n"
    ));
    assert!(!html.contains("doc.name"));
}

// DOC-01, DOC-03, DOC-04: apart from the lines D6 changes, the page is the reference
// template byte for byte, including the option block and the missing final newline.
#[test]
fn doc_01_doc_03_page_is_the_reference_template_outside_the_d6_lines() {
    let html = render_page(&["svc".into()], Some(&json!({ "showExtensions": true })));
    let changed = [
        "    const apiDocs = JSON.parse(",
        "    const scalarOptions = JSON.parse(",
        "    let type = urlParams.get('type') || ",
        "    if (!apiDocs.some((doc) => doc.",
        "        type = ",
        "        sources: apiDocs",
    ];
    let ours: Vec<&str> = html.split('\n').collect();
    let reference: Vec<&str> = REFERENCE_TEMPLATE.split('\n').collect();
    assert_eq!(ours.len(), reference.len());
    for (line, (mine, theirs)) in ours.iter().zip(&reference).enumerate() {
        if changed.iter().any(|prefix| theirs.starts_with(prefix)) {
            let prefix = changed
                .iter()
                .find(|prefix| theirs.starts_with(*prefix))
                .unwrap();
            assert!(mine.starts_with(prefix), "line {}: {mine}", line + 1);
        } else {
            assert_eq!(mine, theirs, "line {}", line + 1);
        }
    }
    assert!(html.ends_with("</html>"));
}

// DOC-03: `scalarOptions || {}` is embedded with JavaScript key order and numbers.
#[test]
fn doc_03_scalar_options_are_embedded_like_json_stringify() {
    let literal = |options: Option<Value>| {
        let html = render_page(&[], options.as_ref());
        page_values(&html).1
    };
    assert_eq!(literal(None), json!({}));
    for falsy in [json!(null), json!(false), json!(0), json!("")] {
        assert_eq!(literal(Some(falsy)), json!({}));
    }
    let html = render_page(&[], Some(&json!({ "b": 1.0, "3": [2.50], "a": "x" })));
    assert!(
        html.contains(r#"const scalarOptions = JSON.parse("{\"3\":[2.5],\"b\":1,\"a\":\"x\"}");"#)
    );
    let html = render_page(&[], None);
    assert!(html.contains(r#"const apiDocs = JSON.parse("[]");"#));
    assert!(html.contains("let type = urlParams.get('type') || \"\";"));
}

// DOC-02: the reference lists services in plain-object key order.
#[test]
fn doc_02_page_services_follow_javascript_key_order() {
    let loaded: Vec<String> = ["svc", "7", "b", "0", "10", "01", "4294967295"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        page_service_order(&loaded),
        ["0", "7", "10", "svc", "b", "01", "4294967295"]
    );
}

// ---------------------------------------------------------------------------------------
// Endpoints.

#[test]
fn doc_14_type_is_read_like_express() {
    assert_eq!(document_type(None), None);
    assert_eq!(document_type(Some("")), None);
    assert_eq!(document_type(Some("x=1")), None);
    assert_eq!(document_type(Some("TYPE=svc")), None);
    assert_eq!(document_type(Some("type=svc")).as_deref(), Some("svc"));
    assert_eq!(document_type(Some("type=%73vc")).as_deref(), Some("svc"));
    assert_eq!(document_type(Some("type=a+b")).as_deref(), Some("a b"));
    assert_eq!(document_type(Some("type=")).as_deref(), Some(""));
    assert_eq!(document_type(Some("type=a&type=b")).as_deref(), Some("a,b"));
    assert_eq!(document_type(Some("type[]=svc")), None);
}

#[test]
fn doc_01_endpoints_match_like_express_routes() {
    use DocumentEndpoint::{Json, Page};
    for (method, path, expected) in [
        (Method::GET, "/documents", Some(Page)),
        (Method::GET, "/documents/", Some(Page)),
        (Method::GET, "/DOCUMENTS", Some(Page)),
        (Method::HEAD, "/Documents/", Some(Page)),
        (Method::GET, "/document-json", Some(Json)),
        (Method::GET, "/document-json/", Some(Json)),
        (Method::HEAD, "/DOCUMENT-JSON", Some(Json)),
        (Method::GET, "/document%73", None),
        (Method::GET, "/documents//", None),
        (Method::GET, "/documents/x", None),
        (Method::GET, "//documents", None),
        (Method::GET, "documents", None),
        (Method::POST, "/documents", None),
        (Method::PUT, "/document-json", None),
        (Method::OPTIONS, "/documents", None),
    ] {
        assert_eq!(
            DocumentEndpoint::match_request(&method, path),
            expected,
            "{method} {path}"
        );
    }
}

enum Answer {
    Document(u16, &'static str),
    Fail,
}

#[derive(Default)]
struct FakeFetcher {
    answers: Mutex<HashMap<String, Answer>>,
    calls: Mutex<Vec<String>>,
}

impl FakeFetcher {
    fn set(&self, url: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(url.to_string(), answer);
    }
}

impl DocumentFetcher for FakeFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        self.calls.lock().unwrap().push(url.to_string());
        let result = match self.answers.lock().unwrap().get(url) {
            Some(Answer::Document(status, body)) => Ok(FetchedDocument {
                status: *status,
                body: body.as_bytes().to_vec(),
            }),
            Some(Answer::Fail) | None => Err(FetchError::new("connection refused")),
        };
        Box::pin(async move { result })
    }
}

const LOADABLE: &str = r#"{"openapi":"3.0.0","info":{"title":"t","version":"1"},"paths":{"/x":{"get":{"parameters":[]}}},"components":{}}"#;

/// Endpoints with services `svc` and `7` loaded (in that order) and `idle` configured but
/// never loaded.
fn endpoints(scalar_options: Option<Value>) -> (DocumentEndpoints, Arc<FakeFetcher>) {
    let services = vec![
        UpstreamService::new("svc", "http://svc/doc", "http://svc"),
        UpstreamService::new("7", "http://seven/doc", "http://seven"),
        UpstreamService::new("idle", "http://idle/doc", "http://idle"),
    ];
    let store = Arc::new(DocumentStore::new(services, SecurityKeys::default()));
    for (prefix, url) in [("svc", "http://svc/doc"), ("7", "http://seven/doc")] {
        store
            .apply(prefix, url, serde_json::from_str(LOADABLE).unwrap())
            .unwrap();
    }
    let fetcher = Arc::new(FakeFetcher::default());
    let endpoints = DocumentEndpoints::new(
        store,
        fetcher.clone(),
        DocumentOptions {
            hide_document_ids: vec!["hide".into()],
            ..options()
        },
        scalar_options,
    );
    (endpoints, fetcher)
}

fn body(response: &http::Response<Vec<u8>>) -> &str {
    std::str::from_utf8(response.body()).unwrap()
}

#[test]
fn doc_02_page_endpoint_lists_loaded_services_in_reference_order() {
    let (endpoints, _) = endpoints(Some(json!({ "theme": "purple" })));
    let response = endpoints.page();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], HTML_UTF8);
    let (sources, options, default) = page_values(body(&response));
    assert_eq!(
        sources,
        json!([
            { "title": "7", "slug": "7", "url": "document-json?type=7" },
            { "title": "svc", "slug": "svc", "url": "document-json?type=svc" }
        ])
    );
    assert_eq!(options, json!({ "theme": "purple" }));
    assert_eq!(default, "svc");
}

#[tokio::test]
async fn doc_05_document_json_fetches_and_transforms_on_every_call() {
    let (endpoints, fetcher) = endpoints(None);
    fetcher.set(
        "http://svc/doc",
        Answer::Document(
            200,
            r#"{"paths":{"/a":{"get":{"operationId":"hide"},"post":{}}}}"#,
        ),
    );
    let response = endpoints.document_json(Some("type=svc")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    assert_eq!(
        body(&response),
        format!(
            r#"{{"paths":{{}},"components":{{"securitySchemes":{{"bearer":{BEARER}}}}},"servers":[{{"url":"/svc"}}]}}"#
        )
    );
    fetcher.set("http://svc/doc", Answer::Document(204, r#"{"paths":{}}"#));
    let response = endpoints.document_json(Some("type=svc")).await.unwrap();
    assert!(body(&response).starts_with(r#"{"paths":{},"components""#));
    assert_eq!(
        *fetcher.calls.lock().unwrap(),
        ["http://svc/doc", "http://svc/doc"]
    );
}

// DOC-14 (D5): a missing `type`, or one naming no service with a loaded document, is 404.
#[tokio::test]
async fn doc_14_d5_unknown_type_is_not_found() {
    let (endpoints, fetcher) = endpoints(None);
    for query in [
        None,
        Some(""),
        Some("x=svc"),
        Some("type="),
        Some("type=nope"),
        Some("type=idle"),
        Some("type=svc&type=svc"),
        Some("type=__proto__"),
        Some("type=constructor"),
    ] {
        let error = endpoints.document_json(query).await.unwrap_err();
        assert!(matches!(error, GatewayError::DocumentNotFound), "{query:?}");
        let response = error.to_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
        assert_eq!(
            body(&response),
            r#"{"message":"Not Found","statusCode":404}"#
        );
    }
    assert!(fetcher.calls.lock().unwrap().is_empty());
}

// DOC-13: a failed fetch, a status outside 2xx, or a body that is not a JSON object is the
// reference's 500.
#[tokio::test]
async fn doc_13_failed_fetches_are_internal_errors() {
    let (endpoints, fetcher) = endpoints(None);
    for answer in [
        Answer::Fail,
        Answer::Document(404, LOADABLE),
        Answer::Document(500, LOADABLE),
        Answer::Document(302, LOADABLE),
        Answer::Document(200, "not json"),
        Answer::Document(200, "[]"),
    ] {
        fetcher.set("http://seven/doc", answer);
        let error = endpoints.document_json(Some("type=7")).await.unwrap_err();
        assert!(matches!(error, GatewayError::Internal { .. }));
        assert_eq!(
            body(&error.to_response()),
            r#"{"statusCode":500,"message":"Internal server error"}"#
        );
    }
}

#[test]
fn js_values_from_serde_keep_order_and_read_numbers_as_doubles() {
    let value = JsValue::from_serde(&json!({ "z": 1, "1": [9007199254740993u64, -0.5] }));
    assert_eq!(
        json::stringify(&value),
        r#"{"1":[9007199254740992,-0.5],"z":1}"#
    );
}
