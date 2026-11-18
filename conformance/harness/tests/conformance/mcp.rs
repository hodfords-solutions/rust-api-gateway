//! MCP server: MCP-01 to MCP-04, MCP-06, MCP-07, MCP-10, MCP-12 to MCP-19.
//!
//! The parts of MCP-12 and MCP-17 under Deviations D7 and D8 are left out here; the
//! reference behaviour behind them is checked in `deviations.rs`.

use anyhow::Result;
use conformance::client::Request;
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mcp::{
    self, ACCEPT, call_tool, call_tool_with, exchange_view, list_tools, post, post_raw,
    result_text, rpc, tool_names,
};
use conformance::mock::{Behaviour, MockUpstream, RecordedRequest, Reply};
use conformance::runner::{Gateway, Observations, Target, TargetKind, run, run_reference_only};
use serde_json::{Value, json};

use crate::common::{error_view, users_gateway};
use crate::pipeline::header_lines;

/// The upstream request a tool call produced: method, target, header lines and body.
fn upstream_view(request: &RecordedRequest) -> Value {
    json!({
        "method": request.method,
        "target": request.target,
        "headers": header_lines(request),
        "body": String::from_utf8_lossy(&request.body),
    })
}

/// Calls a tool and returns what reached the upstream, or the tool result when nothing did.
async fn upstream_of(
    gw: &Gateway,
    mock: &MockUpstream,
    tool: &str,
    arguments: Value,
    headers: &[(&str, &str)],
) -> Result<Value> {
    mock.clear_requests();
    let result = call_tool_with(gw.addr(), tool, arguments, headers).await?;
    Ok(match mock.last_request() {
        Some(request) => upstream_view(&request),
        None => json!({ "result": result }),
    })
}

/// A change to a scenario's configuration.
type Configure = Box<dyn FnOnce(GatewayConfig) -> GatewayConfig + Send>;

/// Two services: `svc` (users fixture) and `other` (default-service fixture).
async fn two_services(
    target: &Target,
    configure: impl FnOnce(GatewayConfig) -> GatewayConfig,
) -> Result<(MockUpstream, MockUpstream, Gateway)> {
    let users = MockUpstream::start().await?;
    users.set_document_json(fixtures::USERS_SERVICE);
    let other = MockUpstream::start().await?;
    other.set_document_json(fixtures::DEFAULT_SERVICE);
    let gw = target
        .start(configure(
            GatewayConfig::new()
                .service("svc", &users)
                .service("other", &other),
        ))
        .await?;
    Ok((users, other, gw))
}

/// Tool names with the services in configuration order (`svc`, then `other`), each service's
/// tools in their own order. The tools follow the services' first-load order (MCP-06), and
/// two documents loading at the same time finish in either order, in both targets.
fn in_service_order(mut names: Vec<String>) -> Vec<String> {
    let service = |name: &String| name.starts_with("other");
    names.sort_by_key(service);
    names
}

fn ping() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
}

fn initialize(version: &str) -> Value {
    rpc(
        "initialize",
        json!({
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "conformance", "version": "1" }
        }),
    )
}

/// MCP-01: `POST /mcp` reaches the MCP endpoint (Express matches the path without regard to
/// case or a trailing slash); other methods and paths go to the proxy.
#[tokio::test]
async fn mcp_01_endpoint_routing() {
    run(
        "mcp_01_endpoint_routing",
        &["MCP-01"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let pong = json!([{ "jsonrpc": "2.0", "id": 1, "result": {} }]);
            for path in ["/mcp", "/mcp/", "/MCP", "/Mcp/", "/mcp?x=1"] {
                let mut request = post(&ping());
                request.target = path.to_string();
                let response = gw.send(&request).await?;
                let view = exchange_view(&response);
                assert_eq!(view["status"], 200, "{path}");
                assert_eq!(view["messages"], pong, "{path}");
                obs.record(format!("POST {path}"), view);
            }
            for (method, path) in [
                ("POST", "/mcp/x"),
                ("POST", "//mcp"),
                ("GET", "/mcp"),
                ("DELETE", "/mcp"),
                ("PUT", "/mcp"),
            ] {
                let response = gw
                    .send(&Request::new(method, path).header("Accept", ACCEPT))
                    .await?;
                // No Service Prefix or Direct Prefix claims `/mcp`, and there is no Default
                // Service.
                assert_eq!(response.status, 503, "{method} {path}");
                assert_eq!(
                    response.text(),
                    r#"{"message":"Service Unavailable","statusCode":503}"#
                );
                obs.record(format!("{method} {path}"), error_view(&response));
            }
            assert!(mock.requests().is_empty());
            drop(gw);

            // A service that owns the Direct Prefix `mcp` receives every other method.
            let mock = MockUpstream::start().await?;
            mock.set_document_json(fixtures::USERS_SERVICE);
            let gw = target
                .start(GatewayConfig::new().service_with_direct("svc", &mock, &["mcp"]))
                .await?;
            let get = gw
                .send(&Request::get("/mcp").header("x-test-static", "1"))
                .await?;
            assert_eq!(get.status, 200);
            let forwarded = mock.last_request().expect("GET /mcp forwarded");
            assert_eq!(
                (forwarded.method.as_str(), forwarded.target.as_str()),
                ("GET", "/mcp")
            );
            obs.record(
                "GET /mcp with Direct Prefix mcp",
                format!("{} {}", forwarded.method, forwarded.target),
            );
            mock.clear_requests();
            let post_response = gw.send(&post(&ping())).await?;
            assert_eq!(exchange_view(&post_response)["messages"], pong);
            assert!(mock.requests().is_empty());
            obs.record("POST /mcp with Direct Prefix mcp", "MCP endpoint");
            Ok(obs)
        },
    )
    .await;
}

/// MCP-02, the part both targets share: Streamable HTTP in stateless mode, compared at the
/// JSON-RPC message level, for the exchanges a conforming client makes. Where `rmcp` answers
/// differently (the table at the end of `docs/parity/08-mcp.md`), only what both agree on
/// is compared here (the status); the reference's exact answers are checked in
/// [`mcp_02_transport_reference_only`].
#[tokio::test]
async fn mcp_02_transport() {
    run("mcp_02_transport", &["MCP-02"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| c).await?;

        let cases: Vec<(&str, Request)> = vec![
            ("initialize 2025-06-18", post(&initialize("2025-06-18"))),
            ("initialize 2024-11-05", post(&initialize("2024-11-05"))),
            (
                "initialize unknown version",
                post(&initialize("1999-01-01")),
            ),
            ("ping", post(&ping())),
            (
                "ping with string id",
                post(&json!({ "jsonrpc": "2.0", "id": "abc", "method": "ping" })),
            ),
            (
                "notification",
                post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
            ),
            (
                "unknown method",
                post(&json!({ "jsonrpc": "2.0", "id": 7, "method": "foo/bar", "params": {} })),
            ),
            (
                "content-type with charset",
                Request::post("/mcp")
                    .header("Accept", ACCEPT)
                    .header("content-type", "application/json; charset=utf-8")
                    .body(ping().to_string()),
            ),
            (
                "supported mcp-protocol-version header",
                post(&ping()).header("mcp-protocol-version", "2025-03-26"),
            ),
            (
                "mcp-session-id header",
                post(&ping()).header("mcp-session-id", "abc"),
            ),
        ];
        for (label, request) in cases {
            let response = gw.send(&request).await?;
            let view = exchange_view(&response);
            eprintln!("{label}: {view}");
            obs.record(label, view);
        }

        // Answered by both with the same status, but not the same body.
        let status_only: Vec<(&str, Request, u16)> = vec![
            ("no Accept", Request::post("/mcp").json(&ping()), 406),
            (
                "Accept application/json only",
                Request::post("/mcp")
                    .header("Accept", "application/json")
                    .json(&ping()),
                406,
            ),
            (
                "Accept text/event-stream only",
                Request::post("/mcp")
                    .header("Accept", "text/event-stream")
                    .json(&ping()),
                406,
            ),
            (
                "Accept */*",
                Request::post("/mcp").header("Accept", "*/*").json(&ping()),
                406,
            ),
            (
                "content-type text/plain",
                Request::post("/mcp")
                    .header("Accept", ACCEPT)
                    .header("content-type", "text/plain")
                    .body(ping().to_string()),
                415,
            ),
            (
                "unsupported mcp-protocol-version header",
                post(&ping()).header("mcp-protocol-version", "1999-01-01"),
                400,
            ),
        ];
        for (label, request, status) in status_only {
            let response = gw.send(&request).await?;
            assert_eq!(response.status, status, "{label}");
            obs.record(label, response.status);
        }
        let capability = gw.send(&post(&rpc("resources/list", json!({})))).await?;
        let error = mcp::parse_message(&capability).expect("error message");
        assert_eq!(error["error"]["code"], -32601);
        obs.record(
            "resources/list (no capability)",
            json!({ "status": capability.status, "code": error["error"]["code"] }),
        );

        let init = gw.send(&post(&initialize("2025-06-18"))).await?;
        assert_eq!(init.status, 200);
        assert_eq!(init.header("content-type"), Some("text/event-stream"));
        assert_eq!(init.header("mcp-session-id"), None);
        let unknown = gw.send(&post(&initialize("1999-01-01"))).await?;
        assert_eq!(
            mcp::parse_message(&unknown).expect("initialize result")["result"]["protocolVersion"],
            "2025-11-25"
        );
        let notification = gw
            .send(&post(
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            ))
            .await?;
        assert_eq!((notification.status, notification.body.len()), (202, 0));
        let unknown_method = gw.send(&post(&rpc("foo/bar", json!({})))).await?;
        assert_eq!(unknown_method.status, 200);
        assert_eq!(
            mcp::parse_message(&unknown_method).expect("error")["error"],
            json!({ "code": -32601, "message": "Method not found" })
        );
        Ok(obs)
    })
    .await;
}

/// MCP-02, the reference's answers where `rmcp` differs (the table at the end of
/// `docs/parity/08-mcp.md`): SSE framing, client responses, batches, transport errors as
/// JSON-RPC bodies, and the error for invalid `tools/call` parameters. Deviation D13 accepts
/// `rmcp`'s behaviour here, so this part runs against the reference only; the Rust side is
/// pinned by the Rust-only test `gateway_endpoints::mcp::d13_mcp_transport_follows_rmcp`.
#[tokio::test]
async fn mcp_02_transport_reference_only() {
    run_reference_only("mcp_02_transport_reference_only", &["MCP-02"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| c).await?;

        let cases: Vec<(&str, Request)> = vec![
            ("client response", post(&json!({ "jsonrpc": "2.0", "id": 5, "result": {} }))),
            ("resources/list (no capability)", post(&rpc("resources/list", json!({})))),
            ("malformed JSON", post_raw(r#"{"jsonrpc":"#)),
            ("empty body", post_raw("")),
            ("not JSON-RPC", post_raw(r#"{"foo":1}"#)),
            ("jsonrpc 1.0", post_raw(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)),
            ("null id", post_raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)),
            (
                "batch of requests",
                post(&json!([
                    { "jsonrpc": "2.0", "id": 1, "method": "ping" },
                    { "jsonrpc": "2.0", "id": 2, "method": "ping" }
                ])),
            ),
            (
                "batch of notifications",
                post(&json!([
                    { "jsonrpc": "2.0", "method": "notifications/initialized" },
                    { "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } }
                ])),
            ),
            (
                "batch of a notification and a request",
                post(&json!([
                    { "jsonrpc": "2.0", "method": "notifications/initialized" },
                    { "jsonrpc": "2.0", "id": 5, "method": "ping" }
                ])),
            ),
            ("empty batch", post_raw("[]")),
            ("no Accept", Request::post("/mcp").json(&ping())),
            (
                "Accept application/json only",
                Request::post("/mcp").header("Accept", "application/json").json(&ping()),
            ),
            (
                "Accept text/event-stream only",
                Request::post("/mcp").header("Accept", "text/event-stream").json(&ping()),
            ),
            ("Accept */*", Request::post("/mcp").header("Accept", "*/*").json(&ping())),
            (
                "content-type text/plain",
                Request::post("/mcp")
                    .header("Accept", ACCEPT)
                    .header("content-type", "text/plain")
                    .body(ping().to_string()),
            ),
            (
                "unsupported mcp-protocol-version header",
                post(&ping()).header("mcp-protocol-version", "1999-01-01"),
            ),
        ];
        for (label, request) in cases {
            let response = gw.send(&request).await?;
            let view = exchange_view(&response);
            eprintln!("{label}: {view}");
            obs.record(label, view);
        }

        // A few exact expectations, so that a change in the reference's SDK shows up here.
        let init = gw.send(&post(&initialize("2025-06-18"))).await?;
        assert!(init.text().starts_with("event: message\ndata: "), "{}", init.text());
        let malformed = gw.send(&post_raw("{")).await?;
        assert_eq!(malformed.status, 400);
        assert_eq!(malformed.header("content-type"), Some("application/json"));
        assert_eq!(
            malformed.text(),
            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error: Invalid JSON"},"id":null}"#
        );
        let batch = gw
            .send(&post(&json!([
                { "jsonrpc": "2.0", "id": 1, "method": "ping" },
                { "jsonrpc": "2.0", "id": 2, "method": "ping" }
            ])))
            .await?;
        assert_eq!(mcp::parse_messages(&batch).len(), 2);
        let no_accept = gw.send(&Request::post("/mcp").json(&ping())).await?;
        assert_eq!(no_accept.status, 406);
        assert_eq!(
            no_accept.text(),
            r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Not Acceptable: Client must accept both application/json and text/event-stream"},"id":null}"#
        );

        // Invalid `tools/call` parameters: the error code; the message is the TypeScript SDK's
        // schema-validation text and is only printed.
        for (label, params) in [
            ("tools/call without name", json!({})),
            ("tools/call with array arguments", json!({ "name": "svc_UsersController_getMe", "arguments": [1] })),
        ] {
            let response = gw.send(&post(&rpc("tools/call", params))).await?;
            let message = mcp::parse_message(&response).expect("error message");
            eprintln!("{label}: {message}");
            assert_eq!(message["error"]["code"], -32603);
            obs.record(
                label,
                json!({ "status": response.status, "code": message["error"]["code"] }),
            );
        }
        Ok(obs)
    })
    .await;
}

/// MCP-03: server info defaults and overrides, and the `tools.listChanged` capability.
#[tokio::test]
async fn mcp_03_server_info() {
    run("mcp_03_server_info", &["MCP-03"], |target| async move {
        let mut obs = Observations::new();
        for (label, server_info) in [
            ("default", None),
            (
                "name and version",
                Some(json!({ "name": "my-gateway", "version": "2.3.4" })),
            ),
            ("name only", Some(json!({ "name": "my-gateway" }))),
            ("empty strings", Some(json!({ "name": "", "version": "" }))),
        ] {
            let (_mock, gw) =
                users_gateway(&target, |c| c.mcp(|m| m.server_info = server_info.clone())).await?;
            let reply = mcp::send(gw.addr(), &post(&initialize("2025-06-18"))).await?;
            let result = reply.message.expect("initialize result")["result"].clone();
            assert_eq!(
                result["capabilities"],
                json!({ "tools": { "listChanged": true } })
            );
            obs.record(
                label,
                json!({
                    "serverInfo": result["serverInfo"],
                    "capabilities": result["capabilities"],
                    "keys": result.as_object().map(|o| {
                        let mut keys: Vec<&String> = o.keys().collect();
                        keys.sort();
                        keys
                    }),
                }),
            );
            let expected = match label {
                "name and version" => json!({ "name": "my-gateway", "version": "2.3.4" }),
                "name only" => json!({ "name": "my-gateway", "version": "1.0.0" }),
                _ => json!({ "name": "nestjs-api-gateway", "version": "1.0.0" }),
            };
            assert_eq!(result["serverInfo"], expected, "{label}");
        }
        Ok(obs)
    })
    .await;
}

/// MCP-04: MCP authentication handlers run before any protocol handling.
#[tokio::test]
async fn mcp_04_authentication() {
    run("mcp_04_authentication", &["MCP-04"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;

        let deny = gw
            .send(&post(&ping()).header("x-test-mcp-auth", "deny"))
            .await?;
        assert_eq!(deny.status, 401);
        assert_eq!(
            deny.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(deny.text(), r#"{"error":"Unauthorized"}"#);
        obs.record("deny", error_view(&deny));

        // Authentication runs before the body, Accept and content type are looked at.
        let deny_malformed = gw
            .send(
                &Request::post("/mcp")
                    .header("x-test-mcp-auth", "deny")
                    .body("{"),
            )
            .await?;
        assert_eq!(deny_malformed.status, 401);
        obs.record("deny with a malformed request", error_view(&deny_malformed));

        let respond = gw
            .send(&post(&ping()).header("x-test-mcp-auth", "respond"))
            .await?;
        assert_eq!(respond.status, 418);
        assert_eq!(
            respond.text(),
            r#"{"message":"test mcp authentication response"}"#
        );
        obs.record("handler writes its own response", error_view(&respond));

        let respond_custom = gw
            .send(
                &post(&ping())
                    .header("x-test-mcp-auth", "respond")
                    .header("x-test-error-status", "403")
                    .header("x-test-error-body", r#"{"error":"nope"}"#),
            )
            .await?;
        assert_eq!(respond_custom.status, 403);
        obs.record("handler writes 403", error_view(&respond_custom));

        // A handler that throws: Nest's unhandled-error answer.
        let throws = gw
            .send(&post(&ping()).header("x-test-mcp-auth", "throw"))
            .await?;
        assert_eq!(throws.status, 500);
        assert_eq!(
            throws.text(),
            r#"{"statusCode":500,"message":"Internal server error"}"#
        );
        obs.record("handler fails", error_view(&throws));

        let denied_call = gw
            .send(
                &mcp::request(
                    "tools/call",
                    json!({ "name": "svc_UsersController_getMe", "arguments": {} }),
                )
                .header("x-test-mcp-auth", "deny"),
            )
            .await?;
        assert_eq!(denied_call.status, 401);
        assert!(mock.requests().is_empty());
        obs.record("denied tools/call reaches no upstream", true);

        let allow = gw
            .send(&post(&ping()).header("x-test-mcp-auth", "allow"))
            .await?;
        assert_eq!(allow.status, 200);
        obs.record("allow", exchange_view(&allow));
        Ok(obs)
    })
    .await;
}

/// MCP-06: `allowedServices`, `allowedOperations` and the `filter` callback.
#[tokio::test]
async fn mcp_06_tool_filters() {
    run("mcp_06_tool_filters", &["MCP-06"], |target| async move {
        let mut obs = Observations::new();
        let configs: Vec<(&str, Configure)> = vec![
            ("no filter", Box::new(|c| c)),
            (
                "allowedServices [other]",
                Box::new(|c| c.mcp(|m| m.allowed_services = Some(vec!["other".into()]))),
            ),
            (
                "allowedServices []",
                Box::new(|c| c.mcp(|m| m.allowed_services = Some(vec![]))),
            ),
            (
                "allowedOperations",
                Box::new(|c| {
                    c.mcp(|m| {
                        m.allowed_operations = Some(vec![
                            "UsersController_getUser".into(),
                            "DefaultController_health".into(),
                        ])
                    })
                }),
            ),
            (
                "allowedOperations []",
                Box::new(|c| c.mcp(|m| m.allowed_operations = Some(vec![]))),
            ),
            (
                "filter",
                Box::new(|c| {
                    c.mcp(|m| {
                        m.filter = Some(json!({
                            "rejectServices": ["other"],
                            "rejectPathPrefixes": ["/v1/users", "/v1/paths/anonymous"]
                        }))
                    })
                }),
            ),
            (
                "allowedServices and filter",
                Box::new(|c| {
                    c.mcp(|m| {
                        m.allowed_services = Some(vec!["other".into()]);
                        m.filter = Some(json!({ "rejectPathPrefixes": ["/oauth"] }));
                    })
                }),
            ),
        ];
        for (label, configure) in configs {
            let (_users, _other, gw) = two_services(&target, configure).await?;
            let names = in_service_order(tool_names(gw.addr()).await?);
            eprintln!("{label}: {names:?}");
            obs.record(label, &names);
            match label {
                "allowedServices []" => assert!(names.is_empty()),
                "allowedOperations" => assert_eq!(
                    names,
                    [
                        "svc_UsersController_getUser",
                        "svc_GET_v1_paths_anonymous_slug",
                        "other_DefaultController_health"
                    ]
                ),
                "allowedOperations []" => {
                    assert_eq!(names, ["svc_GET_v1_paths_anonymous_slug"])
                }
                _ => {}
            }
        }
        Ok(obs)
    })
    .await;
}

/// MCP-07: default tool names and the `toolName` callback.
#[tokio::test]
async fn mcp_07_tool_names() {
    run("mcp_07_tool_names", &["MCP-07"], |target| async move {
        let mut obs = Observations::new();
        let (_users, _other, gw) = two_services(&target, |c| c).await?;
        let names = in_service_order(tool_names(gw.addr()).await?);
        assert!(names.contains(&"svc_GET_v1_paths_anonymous_slug".to_string()));
        obs.record("default names", &names);
        drop(gw);

        let template = "{serviceName}.{method}.{operationId}.{path}|{defaultName}";
        let (users, _other, gw) =
            two_services(&target, |c| c.mcp(|m| m.tool_name = Some(template.into()))).await?;
        let names = in_service_order(tool_names(gw.addr()).await?);
        assert!(
            names.contains(
                &"svc.get../v1/paths/anonymous/{slug}|svc_GET_v1_paths_anonymous_slug".to_string()
            ),
            "{names:?}"
        );
        obs.record("toolName template", &names);

        let renamed = "svc.get.UsersController_getMe./v1/users/me|svc_UsersController_getMe";
        let result = call_tool(gw.addr(), renamed, json!({})).await?;
        assert_eq!(result.get("isError"), None);
        assert_eq!(
            users.last_request().expect("forwarded").target,
            "/v1/users/me/"
        );
        obs.record(
            "call by the overridden name",
            users.last_request().map(|r| r.target),
        );
        let old = call_tool(gw.addr(), "svc_UsersController_getMe", json!({})).await?;
        assert_eq!(
            result_text(&old),
            "Tool not found: svc_UsersController_getMe"
        );
        obs.record("call by the default name", old);
        Ok(obs)
    })
    .await;
}

/// MCP-10, MCP-12 (shared parts) and MCP-13: what `tools/list` returns.
#[tokio::test]
async fn mcp_13_tools_list() {
    run(
        "mcp_13_tools_list",
        &["MCP-10", "MCP-12", "MCP-13"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;
            let mut tools = list_tools(gw.addr()).await?;
            for tool in &tools {
                let keys: Vec<&String> = tool.as_object().expect("tool object").keys().collect();
                assert_eq!(keys, ["name", "description", "inputSchema"]);
            }
            let find = |tools: &[Value], name: &str| -> Value {
                tools
                    .iter()
                    .find(|t| t["name"] == name)
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            assert_eq!(
                find(&tools, "svc_UsersController_listUsers"),
                json!({
                    "name": "svc_UsersController_listUsers",
                    "description": "Returns every user.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query_page": { "type": "number", "description": "query parameter: page" },
                            "query_sort": { "type": "string", "description": "query parameter: sort", "enum": ["asc", "desc"] }
                        }
                    }
                })
            );
            assert_eq!(
                find(&tools, "svc_PathsController_createTree")["inputSchema"],
                json!({
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "description": "Request body",
                            "properties": {
                                "label": { "type": "string" },
                                "children": { "type": "array", "items": {} }
                            },
                            "required": ["label", "children"]
                        }
                    },
                    "required": ["body"]
                })
            );
            // Deviation D8: the second reference to AddressDto (`workAddress`) is `{}` in the
            // reference and resolved in the port. Everything else is shared.
            let address = json!({
                "type": "object",
                "properties": {
                    "street": { "type": "string", "description": "Street line" },
                    "city": { "type": "string" }
                },
                "required": ["street", "city"]
            });
            for tool in &mut tools {
                if let Some(work) = tool
                    .pointer_mut("/inputSchema/properties/body/properties/workAddress")
                {
                    match target.kind {
                        TargetKind::Reference => assert_eq!(*work, json!({})),
                        TargetKind::Rust => assert_eq!(*work, address),
                    }
                    *work = json!("<D8>");
                }
            }
            let create = find(&tools, "svc_UsersController_createUser");
            assert_eq!(
                create["inputSchema"]["properties"]["body"]["properties"]["homeAddress"],
                json!({
                    "type": "object",
                    "properties": {
                        "street": { "type": "string", "description": "Street line" },
                        "city": { "type": "string" }
                    },
                    "required": ["street", "city"]
                })
            );
            assert_eq!(create["inputSchema"]["properties"]["body"]["description"], "The new user");
            obs.record("tools", &tools);
            drop(gw);

            // parameterFilter
            let (_mock, gw) = users_gateway(&target, |c| {
                c.mcp(|m| {
                    m.parameter_filter =
                        Some(json!({ "rejectNames": ["page"], "rejectIn": ["header"] }))
                })
            })
            .await?;
            let tools = list_tools(gw.addr()).await?;
            let list = find(&tools, "svc_UsersController_listUsers")["inputSchema"].clone();
            assert_eq!(
                list,
                json!({
                    "type": "object",
                    "properties": {
                        "query_sort": { "type": "string", "description": "query parameter: sort", "enum": ["asc", "desc"] }
                    }
                })
            );
            obs.record("parameterFilter: listUsers", list);
            let get = find(&tools, "svc_UsersController_getUser")["inputSchema"].clone();
            assert_eq!(
                get,
                json!({
                    "type": "object",
                    "properties": { "path_id": { "type": "string", "description": "path parameter: id" } },
                    "required": ["path_id"]
                })
            );
            obs.record("parameterFilter: getUser", get);
            Ok(obs)
        },
    )
    .await;
}

/// MCP-14: an unknown tool.
#[tokio::test]
async fn mcp_14_unknown_tool() {
    run("mcp_14_unknown_tool", &["MCP-14"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| {
            c.mcp(|m| m.allowed_operations = Some(vec!["UsersController_getMe".into()]))
        })
        .await?;
        for name in ["svc_nope", "svc_UsersController_listUsers", "", "SVC_USERSCONTROLLER_GETME"] {
            let result = call_tool(gw.addr(), name, json!({})).await?;
            assert_eq!(
                result,
                json!({ "content": [{ "type": "text", "text": format!("Tool not found: {name}") }], "isError": true })
            );
            obs.record(format!("{name:?}"), result);
        }
        assert!(mock.requests().is_empty());
        Ok(obs)
    })
    .await;
}

/// MCP-15: rate limiting runs against the `/mcp` request before the middleware.
#[tokio::test]
async fn mcp_15_rate_limit_order() {
    run("mcp_15_rate_limit_order", &["MCP-15"], |target| async move {
        let mut obs = Observations::new();

        // The Identity resolver runs before the middleware sets `authUserId`, so both calls are
        // counted against the client address.
        let (_mock, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_custom_rate_limit = 1.0)).await?;
        let first = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &[("x-test-auth-user", "a")]).await?;
        let second = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &[("x-test-auth-user", "b")]).await?;
        assert_eq!(first.get("isError"), None);
        assert_eq!(result_text(&second), "Error executing svc_UsersController_getMe: error.too_many_requests");
        obs.record("custom bucket ignores the middleware's user", json!([first.get("isError"), second]));
        // ... whereas an explicit Identity from the protocol header is seen.
        let other = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &[("x-test-identity", "k")]).await?;
        assert_eq!(other.get("isError"), None);
        obs.record("explicit identity has its own custom bucket", other.get("isError"));
        drop(gw);

        // A call the middleware denies has already been counted.
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut texts = Vec::new();
        for _ in 0..3 {
            let result = call_tool_with(gw.addr(), "svc_LimitsController_plain", json!({}), &[("x-test-middleware", "deny")]).await?;
            texts.push(result_text(&result));
        }
        assert_eq!(
            texts,
            [
                "Request forbidden by gateway middleware",
                "Request forbidden by gateway middleware",
                "Error executing svc_LimitsController_plain: error.too_many_requests_with_custom_ttl"
            ]
        );
        obs.record("denied calls count against the Route Bucket", &texts);

        // The Route Bucket of a tool call is keyed with method POST, so REST GETs on the same
        // Route do not share it.
        let mut rest = Vec::new();
        for _ in 0..3 {
            rest.push(gw.get("/svc/v1/limits/plain").await?.status);
        }
        assert_eq!(rest, [200, 200, 429]);
        obs.record("REST GETs after the tool calls", &rest);
        drop(gw);
        let (mock2, gw) = users_gateway(&target, |c| c).await?;
        let mut rest = Vec::new();
        for _ in 0..2 {
            rest.push(gw.get("/svc/v1/limits/plain").await?.status);
        }
        let mut calls = Vec::new();
        for _ in 0..3 {
            let result = call_tool(gw.addr(), "svc_LimitsController_plain", json!({})).await?;
            calls.push(json!({ "isError": result.get("isError"), "text": result_text(&result).starts_with("Error") }));
        }
        assert_eq!(rest, [200, 200]);
        obs.record("tool calls after two REST GETs", &calls);
        assert_eq!(calls[1]["isError"], Value::Null);
        assert_eq!(calls[2]["isError"], true);
        drop(gw);
        drop((mock, mock2));

        // Status-scoped Route Buckets are checked but never counted.
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.route("/v1/limits/on-status", Behaviour::Respond(Reply::new(201).body("created")));
        let mut results = Vec::new();
        for _ in 0..3 {
            let result = call_tool(gw.addr(), "svc_LimitsController_onStatus", json!({})).await?;
            results.push(result);
        }
        for result in &results {
            assert_eq!(result_text(result), "created");
        }
        obs.record("status-scoped Route Bucket never fills", &results);
        drop(gw);

        // The Anonymous IP Bucket is counted for unauthenticated Identities, before middleware.
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| {
                t.anonymous_ip_rate_limit = Some(1.0);
                t.anonymous_ip_rate_limit_ttl = Some(60.0);
            })
        })
        .await?;
        let authenticated = [("x-test-identity", "u1"), ("x-test-identity-authenticated", "true")];
        let mut texts = Vec::new();
        for headers in [&authenticated[..], &[], &[], &[("x-test-auth-user", "u2")]] {
            let result = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), headers).await?;
            texts.push(if result.get("isError").is_some() { result_text(&result) } else { "ok".into() });
        }
        assert_eq!(
            texts,
            [
                "ok",
                "ok",
                "Error executing svc_UsersController_getMe: error.too_many_requests",
                "Error executing svc_UsersController_getMe: error.too_many_requests"
            ]
        );
        obs.record("anonymous IP bucket", &texts);
        drop(gw);

        // A failing Identity resolver is an error result with its message; with rate limiting
        // disabled the resolver is not called.
        let (_mock, gw) = users_gateway(&target, |c| c).await?;
        let failed = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &[("x-test-identity-throw", "1")]).await?;
        assert_eq!(result_text(&failed), "Error executing svc_UsersController_getMe: test identity resolver failure");
        obs.record("resolver fails", failed);
        drop(gw);
        let (_mock, gw) = users_gateway(&target, |c| c.throttler(|t| t.is_enable = false)).await?;
        let disabled = call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &[("x-test-identity-throw", "1")]).await?;
        assert_eq!(disabled.get("isError"), None);
        obs.record("rate limiting disabled", disabled.get("isError"));
        Ok(obs)
    })
    .await;
}

/// MCP-16: what the middleware sees and how its answers become tool results.
#[tokio::test]
async fn mcp_16_middleware() {
    run("mcp_16_middleware", &["MCP-16"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;

        let args = json!({ "path_id": "7", "query_x": 1, "body": { "a": [1, "b"] } });
        let result = call_tool_with(
            gw.addr(),
            "svc_UsersController_getUser",
            args.clone(),
            &[
                ("x-test-echo-request", "1"),
                ("auth-user-id", "spoofed"),
                ("Permission-In-Any-Guard", "spoofed"),
                ("x-other", "kept"),
            ],
        )
        .await?;
        assert_eq!(result.get("isError"), None);
        let upstream = mock.last_request().expect("forwarded");
        let seen: Value =
            serde_json::from_str(upstream.header("x-test-request").expect("x-test-request"))?;
        assert_eq!(seen["body"], args);
        let names: Vec<&str> = seen["headers"]
            .as_array()
            .expect("names")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(names.contains(&"x-other"));
        assert!(!names.contains(&"auth-user-id"));
        assert!(!names.contains(&"permission-in-any-guard"));
        obs.record("middleware sees", &seen);

        for (mode, extra) in [
            ("deny", vec![]),
            ("throw", vec![]),
            (
                "throw",
                vec![
                    (
                        "x-test-error-body",
                        r#"{"message":"custom","statusCode":499}"#,
                    ),
                    ("x-test-error-status", "499"),
                ],
            ),
            ("throw", vec![("x-test-error-body", "plain text")]),
            (
                "throw",
                vec![("x-test-error-body", r#"{"error":"no message"}"#)],
            ),
            ("crash", vec![]),
        ] {
            mock.clear_requests();
            let mut headers = vec![("x-test-middleware", mode)];
            headers.extend(extra.iter().copied());
            let result =
                call_tool_with(gw.addr(), "svc_UsersController_getMe", json!({}), &headers).await?;
            assert_eq!(result["isError"], true);
            assert!(mock.requests().is_empty());
            eprintln!("{mode} {extra:?}: {}", result_text(&result));
            obs.record(format!("{mode} {extra:?}"), result);
        }

        // Headers the middleware adds reach the upstream (kebab-cased).
        let view = upstream_of(
            &gw,
            &mock,
            "svc_UsersController_getMe",
            json!({}),
            &[("x-test-auth-user", "42")],
        )
        .await?;
        assert!(
            view["headers"]
                .as_array()
                .expect("headers")
                .contains(&json!("auth-user-id: 42"))
        );
        obs.record("middleware headers", view);
        Ok(obs)
    })
    .await;
}

/// MCP-17: the upstream request built from the arguments (values with no character that
/// Deviation D7 would encode).
#[tokio::test]
async fn mcp_17_upstream_request() {
    run(
        "mcp_17_upstream_request",
        &["MCP-17"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let cases: Vec<(&str, &str, Value)> = vec![
                (
                    "path and trailing slash",
                    "svc_UsersController_getUser",
                    json!({ "path_id": "abc-1_2.3~x" }),
                ),
                (
                    "numeric path value",
                    "svc_UsersController_getUser",
                    json!({ "path_id": 12.0 }),
                ),
                (
                    "boolean path value",
                    "svc_UsersController_getUser",
                    json!({ "path_id": true }),
                ),
                (
                    "missing path argument",
                    "svc_UsersController_getUser",
                    json!({ "path_nope": "x" }),
                ),
                (
                    "no arguments, no path parameter",
                    "svc_UsersController_getMe",
                    json!({}),
                ),
                (
                    "unknown argument shapes",
                    "svc_UsersController_getMe",
                    json!({ "other": 1, "Path_x": 1, "bodyx": 2 }),
                ),
                (
                    "query encoding",
                    "svc_UsersController_listUsers",
                    json!({
                        "query_page": 1.50,
                        "query_sort": "a b&c=d/\u{e9}?+*-._~!()'",
                        "query_2": true,
                        "query_n": null,
                        "query_o": { "a": 1 },
                        "query_arr": [1, [2, 3], null, { "x": 1 }],
                        "query_big": 1e21,
                        "query_small": 1e-7,
                        "query_neg0": -0.0,
                        "query_empty": "",
                        "query_": "blank name",
                        "query_a b": "space in name"
                    }),
                ),
                (
                    "only empty query values",
                    "svc_UsersController_listUsers",
                    json!({ "query_a": "" }),
                ),
                (
                    "GET ignores the body",
                    "svc_UsersController_getUser",
                    json!({ "path_id": "7", "body": { "a": 1 } }),
                ),
                (
                    "DELETE ignores the body",
                    "svc_UsersController_deleteUser",
                    json!({ "path_id": "7", "body": { "a": 1 } }),
                ),
                (
                    "HEAD",
                    "svc_UsersController_headUser",
                    json!({ "path_id": "7" }),
                ),
                (
                    "POST without body",
                    "svc_UsersController_createUser",
                    json!({}),
                ),
                (
                    "POST with object body",
                    "svc_UsersController_createUser",
                    json!({ "body": { "b": 1.0, "a": [1, "x", null], "c": "\u{e9}", "d": 1e21 } }),
                ),
                (
                    "POST with string body",
                    "svc_UsersController_createUser",
                    json!({ "body": "str" }),
                ),
                (
                    "POST with empty object",
                    "svc_UsersController_createUser",
                    json!({ "body": {} }),
                ),
                (
                    "POST with empty array",
                    "svc_UsersController_createUser",
                    json!({ "body": [] }),
                ),
                (
                    "POST with 0",
                    "svc_UsersController_createUser",
                    json!({ "body": 0 }),
                ),
                (
                    "POST with false",
                    "svc_UsersController_createUser",
                    json!({ "body": false }),
                ),
                (
                    "POST with null",
                    "svc_UsersController_createUser",
                    json!({ "body": null }),
                ),
                (
                    "POST with empty string",
                    "svc_UsersController_createUser",
                    json!({ "body": "" }),
                ),
                (
                    "PUT with body",
                    "svc_UsersController_replaceUser",
                    json!({ "path_id": "7", "body": { "name": "n" } }),
                ),
                (
                    "PATCH with body",
                    "svc_UsersController_updateUser",
                    json!({ "path_id": "7", "body": [1] }),
                ),
            ];
            for (label, tool, args) in cases {
                let view = upstream_of(&gw, &mock, tool, args, &[]).await?;
                eprintln!("{label}: {view}");
                obs.record(label, view);
            }
            let check = |label: &str, expected: &str| {
                let entry = obs
                    .entries()
                    .iter()
                    .find(|(l, _)| l == label)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Null);
                assert_eq!(
                    format!(
                        "{} {}",
                        entry["method"].as_str().unwrap_or("?"),
                        entry["target"].as_str().unwrap_or("?")
                    ),
                    expected,
                    "{label}"
                );
            };
            check("path and trailing slash", "GET /v1/users/abc-1_2.3~x/");
            check("numeric path value", "GET /v1/users/12/");
            check("missing path argument", "GET /v1/users/{id}/");
            check("only empty query values", "GET /v1/users/?a=");
            check("HEAD", "HEAD /v1/users/7/");
            check("POST without body", "POST /v1/users/");
            Ok(obs)
        },
    )
    .await;
}

/// MCP-18: upstream headers.
#[tokio::test]
async fn mcp_18_upstream_headers() {
    run(
        "mcp_18_upstream_headers",
        &["MCP-18"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;

            let plain = upstream_of(
                &gw,
                &mock,
                "svc_UsersController_getMe",
                json!({}),
                &[("x-custom", "c")],
            )
            .await?;
            assert_eq!(
                plain["headers"],
                json!([
                    "host: 127.0.0.1:<port>",
                    "connection: keep-alive",
                    "content-type: application/json"
                ])
            );
            obs.record("no header arguments; /mcp headers are not forwarded", plain);

            let args = json!({
                "path_id": "7",
                "header_x-request-id": "arg",
                "header_Content-Type": "text/plain",
                "header_content-type": "text/x",
                "header_x-empty": "",
                "header_Auth-User-Id": "spoof",
                "header_permission-in-any-guard": "spoof",
                "header_x-num": 0,
                "header_x-false": false,
                "header_x-null": null,
                "header_7": "seven",
                "header_X-Up": "Up"
            });
            let mixed = upstream_of(
                &gw,
                &mock,
                "svc_UsersController_getUser",
                args,
                &[(
                    "x-test-add-headers",
                    r#"{"x-request-id":"mw","xEmpty":"","x-mw":"1","X-Up":"mw","x-num":""}"#,
                )],
            )
            .await?;
            assert_eq!(
                mixed["headers"],
                json!([
                    "host: 127.0.0.1:<port>",
                    "connection: keep-alive",
                    "content-type: text/x",
                    "7: seven",
                    "x-request-id: mw",
                    "content-type: text/plain",
                    "x-false: false",
                    "x-null: null",
                    "x-up: Up",
                    "x-mw: 1",
                    "x-up: mw"
                ])
            );
            obs.record("precedence", mixed);

            let posted = upstream_of(
                &gw,
                &mock,
                "svc_UsersController_createUser",
                json!({ "body": { "a": 1 }, "header_x-a": "1" }),
                &[("x-test-auth-user", "9")],
            )
            .await?;
            obs.record("POST with body and middleware user", posted);
            Ok(obs)
        },
    )
    .await;
}

/// MCP-19: tool results for success, upstream errors, rate-limit rejections and upstream
/// failures.
#[tokio::test]
async fn mcp_19_tool_results() {
    run("mcp_19_tool_results", &["MCP-19"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.route(
            "/v1/users/404",
            Behaviour::Respond(Reply::json(404, &json!({ "error": "nope" }))),
        );
        mock.route("/v1/users/down", Behaviour::CloseBeforeHeaders);
        mock.route(
            "/v1/users/7",
            Behaviour::Respond(Reply::new(200).body("caf\u{e9} \u{2713}")),
        );
        mock.route(
            "/v1/users/bin",
            Behaviour::Respond(Reply::new(200).body(vec![0x66, 0xff, 0x6f])),
        );
        mock.route("/v1/users/302", Behaviour::redirect(302, "/elsewhere"));

        let ok = call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "7" })).await?;
        assert_eq!(result_text(&ok), "caf\u{e9} \u{2713}");
        assert_eq!(ok.get("isError"), None);
        obs.record("upstream 200", &ok);

        let bin = call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "bin" })).await?;
        assert_eq!(result_text(&bin), "f\u{fffd}o");
        obs.record("invalid UTF-8 is replaced", &bin);

        let redirect = call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "302" })).await?;
        assert_eq!(redirect.get("isError"), None);
        obs.record("redirects are not followed", &redirect);

        let not_found =
            call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "404" })).await?;
        assert_eq!(
            result_text(&not_found),
            r#"Error executing svc_UsersController_getUser: Downstream service returned 404: {"error":"nope"}"#
        );
        assert_eq!(not_found["isError"], true);
        obs.record("upstream 404", &not_found);

        let down =
            call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "down" })).await?;
        // The message is the HTTP client's own error text (undici: "other side closed"), so
        // only its prefix is part of the contract.
        assert_eq!(down["isError"], true);
        let text = result_text(&down);
        let prefix = "Error executing svc_UsersController_getUser: ";
        assert!(text.starts_with(prefix), "{text}");
        eprintln!("upstream closes before headers: {text}");
        obs.record(
            "upstream closes before headers",
            json!({ "isError": true, "prefix": prefix }),
        );

        // Route Bucket: GET /v1/limits/plain allows 2 per 60 s.
        let mut route = json!(null);
        for _ in 0..3 {
            route = call_tool(gw.addr(), "svc_LimitsController_plain", json!({})).await?;
        }
        assert_eq!(
            result_text(&route),
            "Error executing svc_LimitsController_plain: error.too_many_requests_with_custom_ttl"
        );
        assert_eq!(route["isError"], true);
        obs.record("route bucket full", &route);

        let unknown = call_tool(gw.addr(), "svc_nope", json!({})).await?;
        assert_eq!(result_text(&unknown), "Tool not found: svc_nope");
        obs.record("unknown tool", &unknown);
        drop(gw);

        // Custom Bucket: one call per Identity.
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| t.global_custom_rate_limit = 1.0)
        })
        .await?;
        call_tool(gw.addr(), "svc_UsersController_listUsers", json!({})).await?;
        let custom = call_tool(gw.addr(), "svc_UsersController_listUsers", json!({})).await?;
        assert_eq!(
            result_text(&custom),
            "Error executing svc_UsersController_listUsers: error.too_many_requests"
        );
        obs.record("custom bucket full", &custom);
        drop(gw);

        // Global IP Bucket.
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| t.global_ip_rate_limit = 1.0)
        })
        .await?;
        call_tool(gw.addr(), "svc_UsersController_listUsers", json!({})).await?;
        let ip = call_tool(gw.addr(), "svc_UsersController_listUsers", json!({})).await?;
        assert_eq!(
            result_text(&ip),
            "Error executing svc_UsersController_listUsers: error.too_many_requests"
        );
        obs.record("global ip bucket full", &ip);
        Ok(obs)
    })
    .await;
}

/// CFG-13: without the `mcp` option, or with `enabled: false`, there is no MCP endpoint:
/// `POST /mcp` is an ordinary request for the proxy.
#[tokio::test]
async fn cfg_13_mcp_disabled() {
    run("cfg_13_mcp_disabled", &["CFG-13"], |target| async move {
        let mut obs = Observations::new();
        let configs: Vec<(&str, Configure)> = vec![
            (
                "no mcp option",
                Box::new(|mut c: GatewayConfig| {
                    c.mcp = None;
                    c
                }),
            ),
            ("enabled false", Box::new(|c| c.mcp(|m| m.enabled = false))),
        ];
        for (label, configure) in configs {
            // No service claims `/mcp`: the proxy's 503.
            let (mock, gw) = users_gateway(&target, configure).await?;
            let response = gw.send(&post(&ping())).await?;
            assert_eq!(response.status, 503, "{label}");
            assert_eq!(
                response.text(),
                r#"{"message":"Service Unavailable","statusCode":503}"#
            );
            assert!(mock.requests().is_empty());
            obs.record(label, error_view(&response));
            drop(gw);

            // A service owning the Direct Prefix `mcp` receives it (a Static Request, so no
            // Route is needed).
            let mock = MockUpstream::start().await?;
            mock.set_document_json(fixtures::USERS_SERVICE);
            let configs: Configure = match label {
                "no mcp option" => Box::new(|mut c: GatewayConfig| {
                    c.mcp = None;
                    c
                }),
                _ => Box::new(|c| c.mcp(|m| m.enabled = false)),
            };
            let gw = target
                .start(configs(GatewayConfig::new().service_with_direct(
                    "svc",
                    &mock,
                    &["mcp"],
                )))
                .await?;
            let forwarded = gw.send(&post(&ping()).header("x-test-static", "1")).await?;
            assert_eq!(forwarded.status, 200, "{label}");
            let request = mock.last_request().expect("POST /mcp forwarded");
            assert_eq!(
                (request.method.as_str(), request.target.as_str()),
                ("POST", "/mcp")
            );
            obs.record(
                format!("{label}: Direct Prefix mcp"),
                format!("{} {}", request.method, request.target),
            );
        }
        Ok(obs)
    })
    .await;
}
