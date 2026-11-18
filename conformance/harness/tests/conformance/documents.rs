//! Document endpoints (area 07): `GET /documents` and `GET /document-json`, DOC-01 to
//! DOC-15, and their absence when documents are disabled (CFG-14).

use anyhow::{Context, Result, bail};
use conformance::client::{Request, Response};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::{Document, MockUpstream};
use conformance::runner::{Observations, run, run_reference_only};
use serde_json::{Map, Value, json};

use crate::common::{error_view, users_gateway};

/// A JSON value passed to `JSON.parse(...)` in the page's script, after `marker`.
///
/// The reference embeds the JSON raw inside a template literal (`` JSON.parse(`…`) ``); the
/// Rust target embeds it as a JSON string literal (`JSON.parse("…")`, Deviation D6). Both
/// forms are read, so the scenarios compare the values, not the escaping.
fn embedded_json(html: &str, marker: &str) -> Result<Value> {
    let start = html
        .find(marker)
        .with_context(|| format!("page has no {marker:?}"))?
        + marker.len();
    let rest = &html[start..];
    let text = if let Some(raw) = rest.strip_prefix('`') {
        let end = raw.find("`);").context("unterminated template literal")?;
        raw[..end].to_string()
    } else if rest.starts_with('"') {
        let mut de = serde_json::Deserializer::from_str(rest);
        let literal: String = serde::Deserialize::deserialize(&mut de)?;
        literal
    } else {
        bail!("unexpected JSON.parse argument after {marker:?}");
    };
    Ok(serde_json::from_str(&text)?)
}

/// The option lines given to `Scalar.createApiReference`, without the `sources` line (which
/// Deviation D6 changes) and with surrounding whitespace removed.
fn scalar_option_lines(html: &str) -> Result<Vec<String>> {
    let start = html
        .find("Scalar.createApiReference('#app', {")
        .context("no createApiReference call")?;
    let block = &html[start..];
    let end = block
        .find("\n    })")
        .context("unterminated option block")?;
    Ok(block[..end]
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.starts_with("sources:"))
        .map(str::to_string)
        .collect())
}

/// The option block of the reference template (`views/document.hbs:25-60`), minus `sources`.
const REFERENCE_OPTION_LINES: &[&str] = &[
    "layout: 'modern',",
    "defaultOpenAllTags: true,",
    "showOperationId: true,",
    "hideClientButton: true,",
    "showSidebar: true,",
    "showDeveloperTools: 'never',",
    "showToolbar: 'never',",
    "operationTitleSource: 'summary',",
    "theme: 'default',",
    "persistAuth: true,",
    "telemetry: true,",
    "isEditable: false,",
    "isLoading: false,",
    "hideModels: false,",
    "documentDownloadType: 'none',",
    "hideTestRequestButton: false,",
    "hideSearch: false,",
    "hideDarkModeToggle: false,",
    "withDefaultFonts: true,",
    "defaultOpenFirstTag: true,",
    "expandAllModelSections: false,",
    "expandAllResponses: false,",
    "orderSchemaPropertiesBy: 'alpha',",
    "orderRequiredPropertiesFirst: true,",
    "_integration: 'nestjs',",
    "default: false,",
    "agent: {",
    "disabled: true",
    "},",
    "mcp: {",
    "disabled: true",
    "},",
    "...scalarOptions",
];

fn source(prefix: &str) -> Value {
    json!({ "title": prefix, "slug": prefix, "url": format!("document-json?type={prefix}") })
}

/// DOC-01 to DOC-03: the page, its sources and the Scalar options.
///
/// The service `7` is loaded but listed first whatever the load order: the reference keeps
/// its documents in a plain object, which orders array-index keys before other keys.
#[tokio::test]
async fn doc_01_03_documents_page() {
    run(
        "doc_01_03_documents_page",
        &["DOC-01", "DOC-02", "DOC-03"],
        |target| async move {
            let mut obs = Observations::new();
            let users = MockUpstream::start().await?;
            users.set_document_json(fixtures::USERS_SERVICE);
            let other = MockUpstream::start().await?;
            other.set_document_json(fixtures::DEFAULT_SERVICE);
            let broken = MockUpstream::start().await?;
            broken.set_document(Document::Status(500));
            let options = json!({
                "theme": "purple",
                "showSidebar": false,
                "9": "index key",
                "nested": { "list": [1, 2.5, "x"], "flag": null }
            });
            let gw = target
                .start(
                    GatewayConfig::new()
                        .service("svc", &users)
                        .service("broken", &broken)
                        .service("7", &other)
                        .scalar_options(options),
                )
                .await?;
            let loaded = gw.loaded_services().await?;
            assert_eq!(loaded, ["7", "svc"], "ready lists index-like prefixes first");

            let page = gw.get("/documents").await?;
            assert_eq!(page.status, 200);
            assert_eq!(
                page.header("content-type"),
                Some("text/html; charset=utf-8")
            );
            let html = page.text();
            for fragment in [
                "<!doctype html>\n<html>\n<head>\n    <title>API Reference</title>",
                "<div id=\"app\"></div>",
                "<script src=\"https://cdn.jsdelivr.net/npm/@scalar/api-reference\"></script>",
                "Scalar.createApiReference('#app', {",
                "</script>\n</body>\n</html>",
            ] {
                assert!(html.contains(fragment), "page lacks {fragment:?}:\n{html}");
            }
            assert!(html.ends_with("</html>"), "no trailing newline");
            obs.record("status", page.status);
            obs.record("content type", page.header("content-type"));

            let sources = embedded_json(&html, "const apiDocs = JSON.parse(")?;
            assert_eq!(sources, json!([source("7"), source("svc")]));
            obs.record("sources", &sources);

            let embedded = embedded_json(&html, "const scalarOptions = JSON.parse(")?;
            assert_eq!(
                serde_json::to_string(&embedded)?,
                r#"{"9":"index key","theme":"purple","showSidebar":false,"nested":{"list":[1,2.5,"x"],"flag":null}}"#
            );
            obs.record("scalar options", serde_json::to_string(&embedded)?);

            let lines = scalar_option_lines(&html)?;
            assert_eq!(lines, REFERENCE_OPTION_LINES);
            obs.record("option lines", &lines);
            Ok(obs)
        },
    )
    .await;
}

/// DOC-01, DOC-02: without `scalarOptions` the page embeds `{}`, and with no loaded
/// document it embeds an empty source list.
#[tokio::test]
async fn doc_01_02_documents_page_defaults() {
    run(
        "doc_01_02_documents_page_defaults",
        &["DOC-01", "DOC-02", "DOC-03"],
        |target| async move {
            let mut obs = Observations::new();
            let broken = MockUpstream::start().await?;
            broken.set_document(Document::Status(503));
            let gw = target
                .start(GatewayConfig::new().service("svc", &broken))
                .await?;
            let page = gw.get("/documents").await?;
            assert_eq!(page.status, 200);
            let html = page.text();
            let sources = embedded_json(&html, "const apiDocs = JSON.parse(")?;
            let options = embedded_json(&html, "const scalarOptions = JSON.parse(")?;
            assert_eq!(sources, json!([]));
            assert_eq!(options, json!({}));
            obs.record("sources", sources);
            obs.record("scalar options", options);
            Ok(obs)
        },
    )
    .await;
}

/// DOC-01, DOC-05: how the two paths match. Express routes them case-insensitively, with
/// an optional trailing slash, on the raw (undecoded) path, for `GET` and `HEAD` only.
/// Everything else falls through to the proxy (here: 503, no service takes the request).
#[tokio::test]
async fn doc_01_05_endpoint_paths() {
    run(
        "doc_01_05_endpoint_paths",
        &["DOC-01", "DOC-05"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;
            for (method, path, expected) in [
                ("GET", "/documents", 200),
                ("GET", "/documents/", 200),
                ("GET", "/DOCUMENTS", 200),
                ("GET", "/Documents/?type=svc", 200),
                ("GET", "/document-json?type=svc", 200),
                ("GET", "/document-json/?type=svc", 200),
                ("GET", "/DOCUMENT-JSON?type=svc", 200),
                ("GET", "/document-json?type=%73vc", 200),
                ("HEAD", "/documents", 200),
                ("HEAD", "/document-json?type=svc", 200),
                ("GET", "/document%73", 503),
                ("GET", "/documents//", 503),
                ("GET", "/documents/x", 503),
                ("GET", "/document-json/x?type=svc", 503),
                ("POST", "/documents", 503),
                ("PUT", "/document-json?type=svc", 503),
            ] {
                let response = gw.send(&Request::new(method, path)).await?;
                assert_eq!(response.status, expected, "{method} {path}");
                if method == "HEAD" {
                    assert!(response.body.is_empty(), "{method} {path} has a body");
                }
                if expected == 503 {
                    assert_eq!(
                        response.text(),
                        r#"{"message":"Service Unavailable","statusCode":503}"#
                    );
                }
                obs.record(format!("{method} {path}"), response.status);
            }
            Ok(obs)
        },
    )
    .await;
}

/// The users fixture as the reference serves it from `/document-json?type=svc` with the
/// harness defaults (`excludeHeaders: ["auth-user-id", "permission-in-any-guard"]`,
/// `openApiSecurityKeys: ["auth-user-id"]`).
fn expected_users_document(prefix: Option<&str>) -> Result<Map<String, Value>> {
    let Value::Object(mut doc) = serde_json::from_str(fixtures::USERS_SERVICE)? else {
        bail!("fixture is not an object");
    };
    let schemes = doc["components"]["securitySchemes"]
        .as_object_mut()
        .context("fixture has security schemes")?;
    schemes.insert(
        "bearer".into(),
        json!({ "scheme": "bearer", "bearerFormat": "JWT", "type": "http" }),
    );
    schemes.shift_remove("auth-user-id");
    let user = &mut doc["paths"]["/v1/users/{id}"];
    user["get"]["parameters"]
        .as_array_mut()
        .context("getUser parameters")?
        .retain(|p| p["name"] == "x-request-id" || p["in"] != "header");
    user["get"]["security"] = json!([{ "bearer": [] }]);
    user["patch"]["security"] = json!([{ "x-api-key": [] }, { "bearer": [] }]);
    if let Some(prefix) = prefix {
        doc.insert("servers".into(), json!([{ "url": format!("/{prefix}") }]));
    }
    Ok(doc)
}

/// DOC-05 to DOC-09, DOC-11, DOC-12 on the users fixture: the bearer scheme replaces the
/// fixture's own in place, `auth-*` schemes and excluded header parameters go, security
/// requirements naming `auth-user-id` name `bearer`, `servers` becomes the prefix, and
/// everything else (extensions, `x-rate-limits`, key order) is as the upstream sent it.
/// The document is fetched on every call.
#[tokio::test]
async fn doc_05_11_document_json() {
    run(
        "doc_05_11_document_json",
        &[
            "DOC-05", "DOC-06", "DOC-07", "DOC-08", "DOC-09", "DOC-11", "DOC-12",
        ],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let response = gw.get("/document-json?type=svc").await?;
            assert_eq!(response.status, 200);
            assert_eq!(
                response.header("content-type"),
                Some("application/json; charset=utf-8")
            );
            let expected = serde_json::to_string(&expected_users_document(Some("svc"))?)?;
            assert_eq!(response.text(), expected);
            obs.record("document", response.text());

            mock.set_document_json(fixtures::USERS_SERVICE_V2);
            let fresh = gw.get("/document-json?type=svc").await?.json()?;
            assert!(
                fresh["paths"].get("/v2/new").is_some(),
                "the document is fetched on every call"
            );
            obs.record(
                "refetched paths",
                fresh["paths"].as_object().map(|p| p.len()),
            );
            Ok(obs)
        },
    )
    .await;
}

/// DOC-08, DOC-09 with other options: `excludeHeaders` entries are compared as configured
/// (an upper-case entry never matches), and every `openApiSecurityKeys` name becomes
/// `bearer`.
#[tokio::test]
async fn doc_08_09_options() {
    run(
        "doc_08_09_options",
        &["DOC-08", "DOC-09"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |mut c| {
                c.exclude_headers = vec!["X-Request-Id".into(), "auth-user-id".into()];
                c.open_api_security_keys = vec!["x-api-key".into(), "auth-user-id".into()];
                c
            })
            .await?;
            let doc = gw.get("/document-json?type=svc").await?.json()?;
            let user = &doc["paths"]["/v1/users/{id}"];
            let headers: Vec<&Value> = user["get"]["parameters"]
                .as_array()
                .context("parameters")?
                .iter()
                .filter(|p| p["in"] == "header")
                .map(|p| &p["name"])
                .collect();
            assert_eq!(
                headers,
                [&json!("x-request-id"), &json!("Permission-In-Any-Guard")]
            );
            assert_eq!(user["put"]["security"], json!([{ "bearer": [] }]));
            assert_eq!(
                user["patch"]["security"],
                json!([{ "bearer": [] }, { "bearer": [] }])
            );
            obs.record("header parameters", &headers);
            obs.record("put security", &user["put"]["security"]);
            obs.record("patch security", &user["patch"]["security"]);
            Ok(obs)
        },
    )
    .await;
}

/// Starts the users gateway, then makes the upstream serve `document` (the Route table
/// keeps the fixture; `/document-json` fetches the new one) and returns the response.
async fn document_json_for(
    gw: &conformance::runner::Gateway,
    mock: &MockUpstream,
    document: &str,
    prefix: &str,
) -> Result<Response> {
    mock.set_document_json(document);
    gw.get(&format!("/document-json?type={prefix}")).await
}

/// DOC-05 to DOC-09, DOC-11: key order and placement. A replaced key keeps its position, a
/// renamed security requirement moves to the end of its object, added keys (`bearer`,
/// `securitySchemes`, `servers`) go last, and `$ref` or non-header parameters stay.
#[tokio::test]
async fn doc_05_11_key_order() {
    run(
        "doc_05_11_key_order",
        &[
            "DOC-05", "DOC-06", "DOC-07", "DOC-08", "DOC-09", "DOC-11",
        ],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let document = r##"{"openapi":"3.0.0","paths":{"/a":{"get":{"parameters":[{"in":"header","name":"AUTH-USER-ID"},{"in":"query","name":"auth-user-id"},{"in":"header","name":"x-keep"},{"$ref":"#/components/parameters/P"}],"security":[{"auth-user-id":["read"],"other":[]},{"bearer":["x"],"auth-user-id":[]},{}],"responses":{}}}},"components":{"schemas":{},"x-c":1},"x-tail":true}"##;
            let response = document_json_for(&gw, &mock, document, "svc").await?;
            assert_eq!(response.status, 200);
            assert_eq!(
                response.text(),
                r##"{"openapi":"3.0.0","paths":{"/a":{"get":{"parameters":[{"in":"query","name":"auth-user-id"},{"in":"header","name":"x-keep"},{"$ref":"#/components/parameters/P"}],"security":[{"other":[],"bearer":[]},{"bearer":[]},{}],"responses":{}}}},"components":{"schemas":{},"x-c":1,"securitySchemes":{"bearer":{"scheme":"bearer","bearerFormat":"JWT","type":"http"}}},"x-tail":true,"servers":[{"url":"/svc"}]}"##
            );
            obs.record("added keys", response.text());

            let document = r##"{"servers":[{"url":"https://up"}],"paths":{},"components":{"securitySchemes":{"auth-a":{},"z":{"type":"http"},"bearer":{"type":"apiKey"},"auth-":{},"Auth-b":{}}}}"##;
            let response = document_json_for(&gw, &mock, document, "svc").await?;
            assert_eq!(
                response.text(),
                r##"{"servers":[{"url":"/svc"}],"paths":{},"components":{"securitySchemes":{"z":{"type":"http"},"bearer":{"scheme":"bearer","bearerFormat":"JWT","type":"http"},"Auth-b":{}}}}"##
            );
            obs.record("replaced keys", response.text());

            let document = r##"{"paths":{},"components":{"securitySchemes":null}}"##;
            let response = document_json_for(&gw, &mock, document, "svc").await?;
            assert_eq!(
                response.text(),
                r##"{"paths":{},"components":{"securitySchemes":{"bearer":{"scheme":"bearer","bearerFormat":"JWT","type":"http"}}},"servers":[{"url":"/svc"}]}"##
            );
            obs.record("null security schemes", response.text());
            Ok(obs)
        },
    )
    .await;
}

/// DOC-05: the document is re-serialised the way `JSON.stringify` does it: array-index keys
/// first in numeric order, duplicate keys keep their first position and last value, numbers
/// in JavaScript form (big integers rounded), `\/` unescaped, U+2028 and DEL raw, lone
/// surrogates and control characters escaped, and a leading byte-order mark ignored.
#[tokio::test]
async fn doc_05_serialisation() {
    run("doc_05_serialisation", &["DOC-05"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let document = r#"{"openapi":"3.0.0","b":1,"10":2,"2":3,"01":4,"-1":5,"4294967294":6,"4294967295":7,"n":[1.0,1e21,1E5,12345678901234567890,9007199254740993,-0,0.1,1e-7,123456789012345678901234567890,1e400,-1e-400,5e-324,0.000001],"s":"a\/b\u2028\u2029\ud800x\udc00\u001f\u007f<>&é\ud83d\ude00\b\f\n\r\t\"\\","dup":1,"dup":2,"__proto__":{"x":1},"paths":{"/x":{"get":{"parameters":[],"responses":{"201":{},"200":{},"default":{}}}}},"components":{}}"#;
        let response = document_json_for(&gw, &mock, document, "svc").await?;
        assert_eq!(response.status, 200);
        let expected = "{\"2\":3,\"10\":2,\"4294967294\":6,\"openapi\":\"3.0.0\",\"b\":1,\"01\":4,\"-1\":5,\"4294967295\":7,\"n\":[1,1e+21,100000,12345678901234567000,9007199254740992,0,0.1,1e-7,1.2345678901234568e+29,null,0,5e-324,0.000001],\"s\":\"a/b\u{2028}\u{2029}\\ud800x\\udc00\\u001f\u{7f}<>&é😀\\b\\f\\n\\r\\t\\\"\\\\\",\"dup\":2,\"__proto__\":{\"x\":1},\"paths\":{\"/x\":{\"get\":{\"parameters\":[],\"responses\":{\"200\":{},\"201\":{},\"default\":{}}}}},\"components\":{\"securitySchemes\":{\"bearer\":{\"scheme\":\"bearer\",\"bearerFormat\":\"JWT\",\"type\":\"http\"}}},\"servers\":[{\"url\":\"/svc\"}]}";
        assert_eq!(response.text(), expected);
        obs.record("document", response.text());

        let mut with_bom = "\u{feff}".as_bytes().to_vec();
        with_bom.extend_from_slice(br#"{"paths":{},"components":{}}"#);
        mock.set_document(Document::Json(with_bom));
        let response = gw.get("/document-json?type=svc").await?;
        assert_eq!(
            response.text(),
            r#"{"paths":{},"components":{"securitySchemes":{"bearer":{"scheme":"bearer","bearerFormat":"JWT","type":"http"}}},"servers":[{"url":"/svc"}]}"#
        );
        obs.record("byte-order mark", response.text());

        mock.set_document(Document::Json(
            b"{\"paths\":{},\"components\":{},\"bad\":\"\xff\xfe\"}".to_vec(),
        ));
        let response = gw.get("/document-json?type=svc").await?;
        assert_eq!(
            response.text(),
            "{\"paths\":{},\"components\":{\"securitySchemes\":{\"bearer\":{\"scheme\":\"bearer\",\"bearerFormat\":\"JWT\",\"type\":\"http\"}}},\"bad\":\"\u{fffd}\u{fffd}\",\"servers\":[{\"url\":\"/svc\"}]}"
        );
        obs.record("invalid UTF-8", response.text());
        Ok(obs)
    })
    .await;
}

/// DOC-10: a hidden operation removes its whole path item when it is the last method of
/// that path (the case the reference handles without error); other paths are untouched.
#[tokio::test]
async fn doc_10_hidden_operations() {
    run(
        "doc_10_hidden_operations",
        &["DOC-10"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |mut c| {
                c.restful.hide_document_ids = Some(vec![
                    "UsersController_headUser".into(),
                    "LimitsController_plain".into(),
                    "NoSuchOperation".into(),
                ]);
                c
            })
            .await?;
            let response = gw.get("/document-json?type=svc").await?;
            assert_eq!(response.status, 200);
            let mut expected = expected_users_document(Some("svc"))?;
            let paths = expected["paths"].as_object_mut().context("paths")?;
            paths.shift_remove("/v1/users/{id}");
            paths.shift_remove("/v1/limits/plain");
            assert_eq!(response.text(), serde_json::to_string(&expected)?);
            obs.record("document", response.text());
            Ok(obs)
        },
    )
    .await;
}

/// DOC-11: the Default Service's `servers` is left as received, including when absent.
#[tokio::test]
async fn doc_11_default_service_servers() {
    run(
        "doc_11_default_service_servers",
        &["DOC-11"],
        |target| async move {
            let mut obs = Observations::new();
            let mock = MockUpstream::start().await?;
            mock.set_document_json(fixtures::USERS_SERVICE);
            let gw = target
                .start(GatewayConfig::new().service("default", &mock))
                .await?;
            let response = gw.get("/document-json?type=default").await?;
            assert_eq!(response.status, 200);
            assert_eq!(
                response.text(),
                serde_json::to_string(&expected_users_document(None)?)?
            );
            obs.record("fixture", response.text());

            let response = document_json_for(
                &gw,
                &mock,
                r#"{"paths":{},"components":{}}"#,
                "default",
            )
            .await?;
            assert_eq!(
                response.text(),
                r#"{"paths":{},"components":{"securitySchemes":{"bearer":{"scheme":"bearer","bearerFormat":"JWT","type":"http"}}}}"#
            );
            obs.record("no servers", response.text());
            Ok(obs)
        },
    )
    .await;
}

/// DOC-13: a 2xx body that is not a JSON object answers the same 500 as a failed fetch.
/// Deviation D5 does not replace this: there is no document to serve.
#[tokio::test]
async fn doc_13_document_not_an_object() {
    run(
        "doc_13_document_not_an_object",
        &["DOC-13"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            for (label, body) in [
                ("not JSON", "not json"),
                ("empty", ""),
                ("array", "[1]"),
                ("string", r#""s""#),
                ("number", "1"),
                ("null", "null"),
                ("trailing comma", r#"{"paths":{},"components":{},}"#),
            ] {
                let response = document_json_for(&gw, &mock, body, "svc").await?;
                assert_eq!(response.status, 500, "{label}");
                assert_eq!(
                    response.text(),
                    r#"{"statusCode":500,"message":"Internal server error"}"#,
                    "{label}"
                );
                obs.record(label, error_view(&response));
            }
            Ok(obs)
        },
    )
    .await;
}

/// CFG-14: with documents disabled both paths are ordinary proxy requests; with them
/// enabled the endpoints win over a service whose prefix is `documents`, but only for the
/// exact path.
#[tokio::test]
async fn cfg_14_documents_disabled() {
    run(
        "cfg_14_documents_disabled",
        &["CFG-14", "DOC-01"],
        |target| async move {
            let mut obs = Observations::new();
            let mock = MockUpstream::start().await?;
            mock.set_document_json(fixtures::USERS_SERVICE);
            let config = || {
                GatewayConfig::new()
                    .service("svc", &mock)
                    .service("documents", &mock)
            };

            let gw = target.start(config()).await?;
            let page = gw.get("/documents").await?;
            assert_eq!(page.status, 200);
            assert_eq!(
                page.header("content-type"),
                Some("text/html; charset=utf-8")
            );
            let doc = gw.get("/document-json?type=documents").await?.json()?;
            assert_eq!(doc["servers"], json!([{ "url": "/documents" }]));
            let proxied = gw.get("/documents/v1/users").await?;
            assert_eq!(proxied.status, 200);
            assert_eq!(
                mock.last_request().context("forwarded")?.target,
                "/v1/users/"
            );
            obs.record("enabled", json!([page.status, proxied.status]));
            drop(gw);

            let mut disabled = config();
            disabled.restful.is_enable_document = false;
            let gw = target.start(disabled).await?;
            let page = gw.get("/documents").await?;
            assert_eq!(page.status, 405, "service `documents` has no Route for /");
            assert_eq!(
                page.text(),
                r#"{"message":"Method Not Allowed","statusCode":405}"#
            );
            let json = gw.get("/document-json?type=svc").await?;
            assert_eq!(json.status, 503);
            assert_eq!(
                json.text(),
                r#"{"message":"Service Unavailable","statusCode":503}"#
            );
            obs.record("disabled /documents", error_view(&page));
            obs.record("disabled /document-json", error_view(&json));
            Ok(obs)
        },
    )
    .await;
}

/// DOC-10, DOC-14, DOC-15 (Deviation D5): the reference's 500s that the port replaces,
/// beyond those in `deviations::d5_document_json_crashes`, and the shape of a NestJS 404.
#[tokio::test]
async fn d5_document_json_reference_errors() {
    run_reference_only(
        "d5_document_json_reference_errors",
        &["DOC-10", "DOC-14", "DOC-15"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.restful.hide_document_ids = Some(vec!["UsersController_getUser".into()]);
                c
            })
            .await?;
            let hidden = gw.get("/document-json?type=svc").await?;
            assert_eq!(hidden.status, 500, "hidden operation before other methods");
            obs.record("hidden operation not last", error_view(&hidden));

            for (label, document) in [
                (
                    "operation without parameters",
                    r#"{"paths":{"/x":{"get":{"responses":{}}}},"components":{}}"#,
                ),
                (
                    "path-level parameters",
                    r#"{"paths":{"/x":{"parameters":[],"get":{"parameters":[]}}},"components":{}}"#,
                ),
                (
                    "path-level summary",
                    r#"{"paths":{"/x":{"summary":"s","get":{"parameters":[]}}},"components":{}}"#,
                ),
                ("components is a string", r#"{"paths":{},"components":"x"}"#),
                (
                    "security is an object",
                    r#"{"paths":{"/x":{"get":{"parameters":[],"security":{"a":[]}}}},"components":{}}"#,
                ),
                (
                    "header parameter without a name",
                    r#"{"paths":{"/x":{"get":{"parameters":[{"in":"header"}]}}},"components":{}}"#,
                ),
            ] {
                let response = document_json_for(&gw, &mock, document, "svc").await?;
                assert_eq!(response.status, 500, "{label}");
                obs.record(label, error_view(&response));
            }

            let response = document_json_for(
                &gw,
                &mock,
                r#"{"paths":{},"components":[]}"#,
                "svc",
            )
            .await?;
            assert_eq!(response.status, 200);
            assert_eq!(
                response.text(),
                r#"{"paths":{},"components":[],"servers":[{"url":"/svc"}]}"#,
                "an array keeps no bearer scheme"
            );
            obs.record("components is an array", response.text());

            for query in ["type=svc&type=svc", "type[]=svc", "TYPE=svc", "type="] {
                let response = gw.get(&format!("/document-json?{query}")).await?;
                assert_eq!(response.status, 500, "{query}");
                obs.record(query, error_view(&response));
            }

            let not_found = gw.get("*").await?;
            assert_eq!(not_found.status, 404);
            assert_eq!(
                not_found.text(),
                r#"{"message":"Cannot GET *","error":"Not Found","statusCode":404}"#
            );
            obs.record("NestJS 404", error_view(&not_found));
            Ok(obs)
        },
    )
    .await;
}
