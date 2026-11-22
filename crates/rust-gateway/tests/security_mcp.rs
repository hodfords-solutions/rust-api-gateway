//! Security tests of the MCP tool-call path (`docs/security/attack-tests.md`): what the
//! `header_<x>` arguments of a tool call can change in the request the upstream receives.
//!
//! The upstream here records the exact bytes of each request, so a head written by anyone
//! but the gateway would show.
#![cfg(feature = "mcp")]

mod gateway_common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{GatewayRequest, MiddlewareError, ProxyMiddleware, async_trait};
use api_gateway::openapi::{DocumentFetcher, FetchFuture, FetchedDocument};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use api_gateway::upstream::UpstreamService;
use gateway_common::{Served, client, options, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// One request as the upstream read it.
#[derive(Debug, Clone)]
struct RawRequest {
    connection: u64,
    head: String,
    body: Vec<u8>,
}

/// An upstream that records the raw bytes of every request and answers `200 ok`, keeping
/// connections open. The body is read by the head's last `content-length`, waiting at most
/// half a second for it, so that a head that lies about its body cannot hang the test.
struct RawUpstream {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RawRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl RawUpstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let mut connection = 0;
            while let Ok((mut socket, _)) = listener.accept().await {
                connection += 1;
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    loop {
                        let end = loop {
                            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                                break at + 4;
                            }
                            let mut chunk = [0; 4096];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                        buffer.drain(..end);
                        let length = head
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .filter_map(|(_, value)| value.trim().parse::<usize>().ok())
                            .next_back()
                            .unwrap_or(0);
                        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
                        while buffer.len() < length {
                            let mut chunk = [0; 4096];
                            match tokio::time::timeout_at(deadline, socket.read(&mut chunk)).await {
                                Ok(Ok(n)) if n > 0 => buffer.extend_from_slice(&chunk[..n]),
                                Ok(_) => return,
                                Err(_) => break,
                            }
                        }
                        let body: Vec<u8> = buffer.drain(..length.min(buffer.len())).collect();
                        seen.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(RawRequest {
                                connection,
                                head,
                                body,
                            });
                        let answer = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                                      content-length: 2\r\nconnection: keep-alive\r\n\r\nok";
                        if socket.write_all(answer.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RawRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for RawUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serves the one document from memory.
struct Document;

impl DocumentFetcher for Document {
    fn fetch<'a>(&'a self, _url: &'a str) -> FetchFuture<'a> {
        let body = json!({
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
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        }
                    }
                }
            }
        })
        .to_string()
        .into_bytes();
        Box::pin(async move { Ok(FetchedDocument { status: 200, body }) })
    }
}

/// The identity headers the application's middleware owns: `authUserId` (forwarded as
/// `auth-user-id`, also an `excludeHeaders` name), `authorization` and `xWorkspaceId`
/// (forwarded as `x-workspace-id`).
struct Identity;

#[async_trait]
impl ProxyMiddleware for Identity {
    async fn handle(
        &self,
        _route: &Route,
        _request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "u-1");
        proxy_request.add_header("authorization", "Bearer trusted");
        proxy_request.add_header("xWorkspaceId", "w-1");
        Ok(true)
    }
}

async fn mcp_gateway(upstream: &RawUpstream) -> Served {
    let mut gateway_options = options(vec![UpstreamService::new(
        "svc",
        "mem://svc",
        upstream.origin(),
    )]);
    gateway_options.mcp = Some(api_gateway::mcp::McpOptions::enabled());
    let served = serve(
        Gateway::builder(gateway_options)
            .document_fetcher(Document)
            .proxy_middleware(Identity),
        true,
    )
    .await;
    for _ in 0..100 {
        if served
            .gateway
            .mcp_tools()
            .is_some_and(|tools| !tools.is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    served
}

/// Calls a tool and returns its result: the text, prefixed with `error: ` for an error
/// result.
async fn call_tool(served: &Served, name: &str, arguments: Value) -> String {
    let response = client()
        .post(served.url("/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            })
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
    let message: Value = serde_json::from_str(data).unwrap();
    let result = &message["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    if result["isError"] == true {
        format!("error: {text}")
    } else {
        text.to_string()
    }
}

/// A request line, its `host`, `connection` and `content-length` lines, its body length and
/// its connection.
type Framing<'a> = (String, Vec<&'a str>, Vec<&'a str>, Vec<&'a str>, usize, u64);

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The lines of a head named `name`, compared case-insensitively.
fn lines_named<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
    head.split("\r\n")
        .skip(1)
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(n, _)| n.eq_ignore_ascii_case(name))
        })
        .collect()
}

// The exact head of a tool call is built by the gateway alone: a `header_<x>` argument named
// like any header the gateway once used internally is an ordinary header, in any letter case,
// and cannot replace the request line, the headers or the framing; the next request on the
// same connection is read intact.
#[tokio::test]
async fn tool_arguments_cannot_replace_the_upstream_head() {
    let upstream = RawUpstream::start().await;
    let served = mcp_gateway(&upstream).await;
    let port = upstream.addr.port();
    let forged = [
        "DELETE /admin/ HTTP/1.1\r\nhost: elsewhere\r\nauth-user-id: admin\r\n\r\n",
        "GET /users/1/ HTTP/1.1\r\nauth-user-id: admin\r\ncontent-length: 40\r\n\r\n",
    ];
    let names = ["x-rust-gateway-raw-head", "X-Rust-Gateway-Raw-Head"];
    let mut expected = Vec::new();
    for name in names {
        for forged in forged {
            for id in ["1", "2"] {
                let text = call_tool(
                    &served,
                    "svc_getUser",
                    json!({ "path_id": id, format!("header_{name}"): hex(forged) }),
                )
                .await;
                assert_eq!(text, "ok", "{name}");
                expected.push(format!(
                    "GET /users/{id}/ HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\
                     connection: keep-alive\r\ncontent-type: application/json\r\n\
                     {name}: {}\r\nauth-user-id: u-1\r\nauthorization: Bearer trusted\r\n\
                     x-workspace-id: w-1\r\n\r\n",
                    hex(forged)
                ));
            }
        }
    }
    let requests = upstream.requests();
    let heads: Vec<&str> = requests.iter().map(|r| r.head.as_str()).collect();
    assert_eq!(heads, expected);
    assert!(requests.iter().all(|r| r.body.is_empty()));
    assert!(
        requests.iter().all(|r| r.connection == 1),
        "one connection, never desynchronised: {requests:?}"
    );
}

// A `header_<x>` argument whose name differs only in letter case from a header the
// application's middleware sets does not reach the upstream: the middleware's value is the
// one line sent (Deviation D16; the reference sends both lines).
#[tokio::test]
async fn d16_middleware_headers_win_over_arguments_in_any_case() {
    let upstream = RawUpstream::start().await;
    let served = mcp_gateway(&upstream).await;
    let text = call_tool(
        &served,
        "svc_getUser",
        json!({
            "path_id": "1",
            "header_Authorization": "Bearer evil",
            "header_authorization": "Bearer evil too",
            "header_X-Workspace-Id": "w-evil",
            "header_AUTH-USER-ID": "admin",
            "header_x-locale": "en"
        }),
    )
    .await;
    assert_eq!(text, "ok");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let head = &requests[0].head;
    assert_eq!(
        lines_named(head, "authorization"),
        ["authorization: Bearer trusted"]
    );
    assert_eq!(lines_named(head, "x-workspace-id"), ["x-workspace-id: w-1"]);
    assert_eq!(lines_named(head, "auth-user-id"), ["auth-user-id: u-1"]);
    assert_eq!(lines_named(head, "x-locale"), ["x-locale: en"]);
    assert!(!head.contains("evil") && !head.contains("admin"), "{head}");
}

// The framing headers follow undici's rules (MCP-18), so that no argument can make the
// head's framing differ from the body sent: `host` replaces the gateway's, once;
// `content-length` must match the body and is written once; `connection` may only be
// `keep-alive` or `close`; `transfer-encoding`, `keep-alive`, `upgrade` and `expect` refuse
// the call. A refused call sends nothing.
#[tokio::test]
async fn mcp_18_framing_header_arguments_follow_undici() {
    let upstream = RawUpstream::start().await;
    let served = mcp_gateway(&upstream).await;
    let refused = [
        (
            "svc_createUser",
            json!({ "body": { "a": 1 }, "header_Content-Length": "3" }),
            "Request body length does not match content-length header",
        ),
        (
            "svc_createUser",
            json!({ "body": { "a": 1 }, "header_content-length": "+7" }),
            "invalid content-length header",
        ),
        (
            "svc_createUser",
            json!({ "body": { "a": 1 }, "header_content-length": "7", "header_Content-Length": "7" }),
            "duplicate content-length header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_Host": "a", "header_host": "b" }),
            "duplicate host header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_Transfer-Encoding": "chunked" }),
            "invalid transfer-encoding header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_keep-alive": "timeout=5" }),
            "invalid keep-alive header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_upgrade": "websocket" }),
            "invalid upgrade header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_connection": "upgrade, x y" }),
            "invalid connection header",
        ),
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_Expect": "100-continue" }),
            "expect header not supported",
        ),
    ];
    for (tool, arguments, message) in refused {
        assert_eq!(
            call_tool(&served, tool, arguments.clone()).await,
            format!("error: Error executing {tool}: {message}"),
            "{arguments}"
        );
    }
    assert!(upstream.requests().is_empty(), "nothing was sent");

    let accepted = [
        (
            "svc_getUser",
            json!({ "path_id": "1", "header_Host": "other.example" }),
        ),
        (
            "svc_getUser",
            json!({ "path_id": "2", "header_content-length": "9" }),
        ),
        (
            "svc_createUser",
            json!({ "body": { "a": 1 }, "header_content-length": "7" }),
        ),
        ("svc_createUser", json!({ "header_Content-Length": "5" })),
        (
            "svc_getUser",
            json!({ "path_id": "3", "header_Connection": "Keep-Alive" }),
        ),
        (
            "svc_getUser",
            json!({ "path_id": "4", "header_connection": "close" }),
        ),
        ("svc_getUser", json!({ "path_id": "5" })),
    ];
    for (tool, arguments) in accepted {
        assert_eq!(
            call_tool(&served, tool, arguments.clone()).await,
            "ok",
            "{arguments}"
        );
    }
    let requests = upstream.requests();
    let port = upstream.addr.port();
    let summary: Vec<Framing<'_>> = requests
        .iter()
        .map(|r| {
            (
                r.head.lines().next().unwrap_or_default().to_string(),
                lines_named(&r.head, "host"),
                lines_named(&r.head, "connection"),
                lines_named(&r.head, "content-length"),
                r.body.len(),
                r.connection,
            )
        })
        .collect();
    let host = format!("host: 127.0.0.1:{port}");
    assert_eq!(
        summary,
        [
            (
                "GET /users/1/ HTTP/1.1".to_string(),
                vec!["host: other.example"],
                vec!["connection: keep-alive"],
                vec![],
                0,
                1
            ),
            (
                "GET /users/2/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: keep-alive"],
                vec![],
                0,
                1
            ),
            (
                "POST /users/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: keep-alive"],
                vec!["content-length: 7"],
                7,
                1
            ),
            (
                "POST /users/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: keep-alive"],
                vec!["content-length: 0"],
                0,
                1
            ),
            (
                "GET /users/3/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: keep-alive"],
                vec![],
                0,
                1
            ),
            (
                "GET /users/4/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: close"],
                vec![],
                0,
                1
            ),
            // `connection: close` is not reused.
            (
                "GET /users/5/ HTTP/1.1".to_string(),
                vec![host.as_str()],
                vec!["connection: keep-alive"],
                vec![],
                0,
                2
            ),
        ]
    );
}
