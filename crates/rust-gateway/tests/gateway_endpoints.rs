//! Rust-only tests of the gateway's own endpoints, served in-process: the document endpoints
//! under Deviations D5 (DOC-10, DOC-14, DOC-15) and D6 (DOC-04), and the MCP endpoint under
//! Deviations D7 (MCP-17) and D8 (MCP-12), plus its per-service pools (MCP-20) and CFG-13.

mod gateway_common;

use std::collections::HashMap;

use gateway_common::{Upstream, client, options, serve};
use rust_gateway::gateway::Gateway;
use rust_gateway::openapi::{DocumentFetcher, FetchFuture, FetchedDocument};
use rust_gateway::upstream::UpstreamService;
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
    assert_eq!(headers["x-powered-by"], "Express");
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
        gateway_options.mcp = Some(rust_gateway::mcp::McpOptions::enabled());
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
