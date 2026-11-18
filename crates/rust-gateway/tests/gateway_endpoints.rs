//! Rust-only tests of the gateway's own endpoints, served in-process: the document endpoints
//! under Deviations D5 (DOC-10, DOC-14, DOC-15) and D6 (DOC-04), and the MCP endpoint under
//! Deviations D7 (MCP-17) and D8 (MCP-12), plus its per-service pools (MCP-20) and CFG-13.

mod gateway_common;

use std::collections::HashMap;

use api_gateway::gateway::Gateway;
use api_gateway::openapi::{DocumentFetcher, FetchFuture, FetchedDocument};
use api_gateway::upstream::UpstreamService;
use gateway_common::{Upstream, client, options, serve};
use serde_json::{Value, json};

/// Serves documents from memory, by URL.
struct Documents(HashMap<String, Value>);

impl DocumentFetcher for Documents {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        let body = self
            .0
            .get(url)
            .map(|doc| doc.to_string().into_bytes())
            .unwrap_or_default();
        let status = if self.0.contains_key(url) { 200 } else { 404 };
        Box::pin(async move { Ok(FetchedDocument { status, body }) })
    }
}

/// A service `svc` whose document is `document`, sending requests to `upstream`.
pub(crate) fn service(upstream: &Upstream) -> UpstreamService {
    UpstreamService::new("svc", "mem://svc", upstream.origin())
}

pub(crate) fn fetcher(document: Value) -> Documents {
    Documents(HashMap::from([("mem://svc".to_string(), document)]))
}

/// The document of the D5 cases: a hidden operation that is not the last method of its
/// path, an operation without `parameters`, no `components`, and shapes the reference
/// throws on.
fn d5_document() -> Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "d5", "version": "1" },
        "paths": {
            "/hidden": {
                "get": { "operationId": "hidden" },
                "post": { "operationId": "kept" }
            },
            "/plain": { "get": { "operationId": "plain" }, "x-other": 1 },
            "/odd": {
                "get": {
                    "operationId": "odd",
                    "parameters": "not a list",
                    "security": [{ "auth-user-id": [] }, 5]
                }
            }
        }
    })
}

// DOC-10, DOC-14, DOC-15 (D5): `/document-json` never fails for the shapes the reference
// throws on, and a missing or unknown `type` is 404.
#[tokio::test]
async fn d5_document_json_never_crashes() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![service(&upstream)]);
    gateway_options.documents.enabled = true;
    gateway_options.documents.hide_document_ids = vec!["hidden".into()];
    gateway_options.open_api_security_keys = vec!["auth-user-id".into()];
    let served = serve(
        Gateway::builder(gateway_options).document_fetcher(fetcher(d5_document())),
        true,
    )
    .await;
    let http = client();

    let response = http
        .get(served.url("/document-json?type=svc"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    let document: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    // DOC-10: the path of a hidden operation is gone with all its methods.
    assert!(document["paths"].get("/hidden").is_none());
    // DOC-15: no `components` gets the bearer scheme; odd shapes are left alone.
    assert_eq!(
        document["components"],
        json!({ "securitySchemes": { "bearer": { "scheme": "bearer", "bearerFormat": "JWT", "type": "http" } } })
    );
    assert_eq!(document["paths"]["/odd"]["get"]["parameters"], "not a list");
    assert_eq!(
        document["paths"]["/odd"]["get"]["security"],
        json!([{ "bearer": [] }, 5])
    );
    assert_eq!(document["servers"], json!([{ "url": "/svc" }]));

    // DOC-14: a missing, unknown or repeated `type` is NestJS's 404.
    for target in [
        "/document-json",
        "/document-json?type=nope",
        "/document-json?type=svc&type=svc",
        "/DOCUMENT-JSON/?type=",
    ] {
        let response = http.get(served.url(target)).send().await.unwrap();
        assert_eq!(response.status(), 404, "{target}");
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"message":"Not Found","statusCode":404}"#,
            "{target}"
        );
    }
    assert!(upstream.seen().is_empty(), "no document request is proxied");
}

// DOC-01, DOC-04 (D6): the page as Express sends it, values escaped, `?type=` matched on
// the slug in the page script; `HEAD` has the headers and no body; disabled documents leave
// both paths to the proxy (CFG-14).
#[tokio::test]
async fn d6_documents_page_escapes_and_selects_by_slug() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![service(&upstream)]);
    gateway_options.documents.enabled = true;
    gateway_options.scalar_options = Some(json!({ "x": "</script><script>alert(1)</script>" }));
    let served = serve(
        Gateway::builder(gateway_options).document_fetcher(fetcher(d5_document())),
        true,
    )
    .await;
    let http = client();

    let response = http
        .get(served.url("/Documents/?type=svc"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(
        !headers.contains_key("x-powered-by"),
        "off by default (D11)"
    );
    assert!(headers["etag"].to_str().unwrap().starts_with("W/\""));
    let length: usize = headers["content-length"].to_str().unwrap().parse().unwrap();
    let html = response.text().await.unwrap();
    assert_eq!(html.len(), length);
    assert!(!html.contains("</script><script>alert(1)"));
    assert!(html.contains(r#"\u003c/script\u003e\u003cscript\u003ealert(1)"#));
    assert!(html.contains("doc.slug === urlParams.get('type') ? { ...doc, default: true } : doc"));
    assert_eq!(html.matches("</script>").count(), 2);

    let head = http.head(served.url("/documents")).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], headers["content-length"]);
    assert!(head.bytes().await.unwrap().is_empty());
    served.shutdown().await.unwrap();

    // CFG-14: disabled, both paths reach the proxy (a Static Request is not needed: no
    // service claims them and there is no Default Service, so the proxy answers 503).
    let gateway_options = options(vec![service(&upstream)]);
    let served = serve(
        Gateway::builder(gateway_options).document_fetcher(fetcher(d5_document())),
        true,
    )
    .await;
    for path in ["/documents", "/document-json?type=svc"] {
        let response = http.get(served.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 503, "{path}");
    }
}

#[cfg(feature = "mcp")]
mod mcp {
    use super::*;

    /// The document of the MCP cases: a path parameter, and a schema referenced twice.
    fn mcp_document() -> Value {
        json!({
            "openapi": "3.0.0",
            "info": { "title": "mcp", "version": "1" },
            "paths": {
                "/users/{id}": {
                    "get": {
                        "operationId": "getUser",
                        "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }]
                    }
                },
                "/users": {
                    "post": {
                        "operationId": "createUser",
                        "requestBody": {
                            "required": true,
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/User" } } }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "properties": {
                            "home": { "$ref": "#/components/schemas/Address" },
                            "work": { "$ref": "#/components/schemas/Address" }
                        }
                    },
                    "Address": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } }
                    }
                }
            }
        })
    }

    /// Sends one JSON-RPC request to `/mcp` and returns the message of its SSE answer.
    async fn rpc(served: &gateway_common::Served, method: &str, params: Value) -> Value {
        let response = client()
            .post(served.url("/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(
                json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
                    .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_else(|| panic!("no data line in {text:?}"));
        serde_json::from_str(data).unwrap()
    }

    async fn mcp_gateway(upstream: &Upstream) -> gateway_common::Served {
        let mut gateway_options = options(vec![service(upstream)]);
        gateway_options.mcp = Some(api_gateway::mcp::McpOptions::enabled());
        let served = serve(
            Gateway::builder(gateway_options).document_fetcher(fetcher(mcp_document())),
            true,
        )
        .await;
        // The registry rebuilds once the first load has settled (MCP-05).
        for _ in 0..100 {
            if served
                .gateway
                .mcp_tools()
                .is_some_and(|tools| !tools.is_empty())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        served
    }

    /// One `POST /mcp` exchange as `<status> [<content-type>] <body>`, plus `cache-control`
    /// for an event stream.
    async fn mcp_exchange(
        served: &gateway_common::Served,
        headers: &[(&str, &str)],
        body: impl Into<reqwest::Body>,
    ) -> String {
        let mut request = client().post(served.url("/mcp"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.body(body).send().await.unwrap();
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_else(|| "-".into())
        };
        let content_type = header("content-type");
        let cache = if content_type == "text/event-stream" {
            format!(" (cache-control {})", header("cache-control"))
        } else {
            String::new()
        };
        let text = response.text().await.unwrap();
        format!("{status} [{content_type}]{cache} {text}")
    }

    /// A labelled `POST /mcp`: its headers and body.
    type Case = (&'static str, Vec<(&'static str, &'static str)>, String);

    // MCP-02 (D13): where the MCP transport follows `rmcp` instead of the reference's
    // TypeScript SDK, every difference listed at the end of docs/parity/08-mcp.md, pinned as
    // the served gateway answers it, so that an `rmcp` upgrade that changes one fails here.
    // The reference's answers are checked by `mcp::mcp_02_transport_reference_only`.
    #[tokio::test]
    async fn d13_mcp_transport_follows_rmcp() {
        const ACCEPT: (&str, &str) = ("accept", "application/json, text/event-stream");
        const JSON: (&str, &str) = ("content-type", "application/json");
        let upstream = Upstream::start().await;
        let served = mcp_gateway(&upstream).await;
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let version = |v: &'static str| ("mcp-protocol-version", v);
        let call = |params: Value| {
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params })
                .to_string()
        };
        let cases: Vec<Case> = vec![
            ("SSE framing", vec![ACCEPT, JSON], ping.into()),
            (
                "batch of requests",
                vec![ACCEPT, JSON],
                format!("[{ping},{}]", ping.replace("\"id\":1", "\"id\":2")),
            ),
            (
                "batch of a notification",
                vec![ACCEPT, JSON],
                r#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#.into(),
            ),
            ("empty batch", vec![ACCEPT, JSON], "[]".into()),
            (
                "JSON-RPC response from the client",
                vec![ACCEPT, JSON],
                r#"{"jsonrpc":"2.0","id":5,"result":{}}"#.into(),
            ),
            (
                "malformed JSON",
                vec![ACCEPT, JSON],
                r#"{"jsonrpc":"#.into(),
            ),
            ("empty body", vec![ACCEPT, JSON], String::new()),
            ("not JSON-RPC", vec![ACCEPT, JSON], r#"{"foo":1}"#.into()),
            (
                "jsonrpc 1.0",
                vec![ACCEPT, JSON],
                r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#.into(),
            ),
            (
                "null id",
                vec![ACCEPT, JSON],
                r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#.into(),
            ),
            ("no Accept", vec![JSON], ping.into()),
            (
                "Accept without event-stream",
                vec![("accept", "application/json"), JSON],
                ping.into(),
            ),
            ("Accept */*", vec![("accept", "*/*"), JSON], ping.into()),
            (
                "content type not JSON",
                vec![ACCEPT, ("content-type", "text/plain")],
                ping.into(),
            ),
            (
                "unsupported mcp-protocol-version",
                vec![ACCEPT, JSON, version("1999-01-01")],
                ping.into(),
            ),
            (
                "mcp-protocol-version 2024-10-07",
                vec![ACCEPT, JSON, version("2024-10-07")],
                ping.into(),
            ),
            (
                "mcp-protocol-version 2026-07-28",
                vec![ACCEPT, JSON, version("2026-07-28")],
                ping.into(),
            ),
            (
                "resources/list",
                vec![ACCEPT, JSON],
                r#"{"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}"#.into(),
            ),
            (
                "tools/call without name",
                vec![ACCEPT, JSON],
                call(json!({})),
            ),
            (
                "tools/call with array arguments",
                vec![ACCEPT, JSON],
                call(json!({ "name": "svc_getUser", "arguments": [1] })),
            ),
        ];
        let mut observed = Vec::new();
        for (label, headers, body) in cases {
            observed.push((
                label.to_string(),
                mcp_exchange(&served, &headers, body).await,
            ));
        }
        // Larger than the 4 MiB both SDKs accept.
        let large = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
            "x".repeat(4 * 1024 * 1024)
        );
        observed.push((
            "body larger than 4 MiB".to_string(),
            mcp_exchange(&served, &[ACCEPT, JSON], large).await,
        ));

        let expected: Vec<(&str, &str)> = vec![
            (
                "SSE framing",
                "200 [text/event-stream] (cache-control no-cache) data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
            ),
            (
                "batch of requests",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "batch of a notification",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "empty batch",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "JSON-RPC response from the client",
                "400 [application/json] {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32600,\"message\":\"stateless server does not accept JSON-RPC responses\"}}",
            ),
            (
                "malformed JSON",
                "415 [-] fail to deserialize request body EOF while parsing a value at line 1 column 11",
            ),
            (
                "empty body",
                "415 [-] fail to deserialize request body EOF while parsing a value at line 1 column 0",
            ),
            (
                "not JSON-RPC",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "jsonrpc 1.0",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "null id",
                "415 [-] fail to deserialize request body unrecognized JSON-RPC message",
            ),
            (
                "no Accept",
                "406 [-] Not Acceptable: Client must accept both application/json and text/event-stream",
            ),
            (
                "Accept without event-stream",
                "406 [-] Not Acceptable: Client must accept both application/json and text/event-stream",
            ),
            (
                "Accept */*",
                "406 [-] Not Acceptable: Client must accept both application/json and text/event-stream",
            ),
            (
                "content type not JSON",
                "415 [-] Unsupported Media Type: Content-Type must be application/json",
            ),
            (
                "unsupported mcp-protocol-version",
                "400 [-] Bad Request: Unsupported MCP-Protocol-Version: 1999-01-01",
            ),
            (
                "mcp-protocol-version 2024-10-07",
                "400 [-] Bad Request: Unsupported MCP-Protocol-Version: 2024-10-07",
            ),
            (
                "mcp-protocol-version 2026-07-28",
                "400 [application/json] {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32020,\"message\":\"missing required Mcp-Method header\"}}",
            ),
            (
                "resources/list",
                "200 [text/event-stream] (cache-control no-cache) data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"resources/list\"}}\n\n",
            ),
            (
                "tools/call without name",
                "200 [text/event-stream] (cache-control no-cache) data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n\n",
            ),
            (
                "tools/call with array arguments",
                "200 [text/event-stream] (cache-control no-cache) data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n\n",
            ),
            (
                "body larger than 4 MiB",
                "413 [-] Payload Too Large: request body exceeds 4194304 bytes",
            ),
        ];
        let expected: Vec<(String, String)> = expected
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        if observed != expected {
            for (label, line) in &observed {
                println!("            ({label:?}, {line:?}),");
            }
        }
        assert_eq!(observed, expected);
    }

    // MCP-12 (D8): a schema referenced twice resolves both times.
    #[tokio::test]
    async fn d8_repeated_references_resolve_every_time() {
        let upstream = Upstream::start().await;
        let served = mcp_gateway(&upstream).await;
        let list = rpc(&served, "tools/list", json!({})).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        let create = tools
            .iter()
            .find(|t| t["name"] == "svc_createUser")
            .unwrap();
        let address = json!({ "type": "object", "properties": { "city": { "type": "string" } } });
        let body = &create["inputSchema"]["properties"]["body"]["properties"];
        assert_eq!(body["home"], address);
        assert_eq!(body["work"], address);
    }

    // MCP-17 (D7): path arguments are encoded and cannot leave their segment; an empty or dot
    // segment refuses the call before anything is sent.
    #[tokio::test]
    async fn d7_path_arguments_stay_in_their_segment() {
        let upstream = Upstream::start().await;
        let served = mcp_gateway(&upstream).await;
        let call = |id: Value| {
            let served = &served;
            async move {
                rpc(
                    served,
                    "tools/call",
                    json!({ "name": "svc_getUser", "arguments": { "path_id": id } }),
                )
                .await["result"]
                    .clone()
            }
        };

        let result = call(json!("../admin?x=1#f")).await;
        assert_eq!(result.get("isError"), None, "{result}");
        let seen = upstream.seen();
        assert_eq!(
            seen.last().unwrap().target,
            "/users/..%2Fadmin%3Fx%3D1%23f/"
        );

        for refused in [json!(""), json!("."), json!("..")] {
            let before = upstream.seen().len();
            let result = call(refused.clone()).await;
            assert_eq!(result["isError"], true, "{refused}");
            assert_eq!(
                result["content"][0]["text"],
                "Error executing svc_getUser: Invalid path argument path_id",
                "{refused}"
            );
            assert_eq!(upstream.seen().len(), before, "{refused}: nothing sent");
        }
    }

    // MCP-20, MCP-18: tool calls go over a pool per Upstream Service, without the Service
    // Prefix, reusing its connection; the head is undici's (names in their case, repeated
    // names apart). CFG-13: the tool list is readable from the gateway.
    #[tokio::test]
    async fn mcp_20_tool_calls_use_a_pool_per_service() {
        let upstream = Upstream::start().await;
        let served = mcp_gateway(&upstream).await;
        let tools = served.gateway.mcp_tools().expect("MCP enabled");
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["svc_getUser", "svc_createUser"]
        );
        for id in ["1", "2"] {
            let result = rpc(
                &served,
                "tools/call",
                json!({
                    "name": "svc_getUser",
                    "arguments": { "path_id": id, "header_X-Up": "a", "header_x-up": "b", "header_x-mid": "m" }
                }),
            )
            .await;
            assert_eq!(
                result["result"]["content"][0]["text"],
                format!("upstream /users/{id}/")
            );
        }
        let seen = upstream.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].connection, seen[1].connection,
            "the connection is reused"
        );
        assert_eq!(seen[0].target, "/users/1/");
        let names: Vec<&str> = seen[0].headers.iter().map(|(n, _)| n.as_str()).collect();
        // hyper's server lower-cases names; the order shows the lines were written apart.
        assert_eq!(
            names,
            [
                "host",
                "connection",
                "content-type",
                "x-up",
                "x-up",
                "x-mid"
            ]
        );
        assert_eq!(
            seen[0]
                .headers
                .iter()
                .filter(|(n, _)| n == "x-up")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        // CFG-13: without `mcp`, no tools and no endpoint.
        let disabled = serve(
            Gateway::builder(options(vec![service(&upstream)]))
                .document_fetcher(fetcher(mcp_document())),
            true,
        )
        .await;
        assert!(disabled.gateway.mcp_tools().is_none());
        let response = client()
            .post(disabled.url("/mcp"))
            .header("accept", "application/json, text/event-stream")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }
}
