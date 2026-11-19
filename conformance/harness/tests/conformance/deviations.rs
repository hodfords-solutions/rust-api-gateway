//! Reference behaviour behind the approved Deviations (docs/parity/README.md). These run
//! against the reference only: the Rust behaviour for these rows is tested Rust-only.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use conformance::client::{self, RawConn, ReadOutcome, Request};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mcp::{self, call_tool};
use conformance::mock::{Behaviour, Document, MockUpstream};
use conformance::runner::{Observations, run_reference_only};
use serde_json::json;

use crate::common::{assert_table, error_view, users_gateway};

/// D1 (WS-05): a rejected WebSocket upgrade gets no answer and the socket stays open.
#[tokio::test]
async fn d1_rejected_upgrade_hangs() {
    run_reference_only(
        "d1_rejected_upgrade_hangs",
        &["WS-05"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;
            for (label, request) in [
                (
                    "middleware denies",
                    client::upgrade_request("/svc/socket").header("x-test-middleware", "deny"),
                ),
                (
                    "middleware throws 401",
                    client::upgrade_request("/svc/socket").header("x-test-middleware", "throw"),
                ),
                (
                    "no Upstream Service",
                    client::upgrade_request("/nowhere/socket"),
                ),
                // The method check of WS-06 runs after the middleware: a rejected POST
                // upgrade hangs instead of being closed.
                (
                    "middleware denies a POST upgrade",
                    post_upgrade().header("x-test-middleware", "deny"),
                ),
            ] {
                let mut conn = RawConn::connect(gw.addr()).await?;
                conn.send(&request).await?;
                let outcome = conn.read_some(Duration::from_secs(3)).await;
                assert_eq!(
                    outcome,
                    ReadOutcome::Timeout,
                    "{label}: no answer, still open"
                );
                obs.record(label, "no answer after 3 s, connection open");
            }
            Ok(obs)
        },
    )
    .await;
}

fn post_upgrade() -> Request {
    let mut request = client::upgrade_request("/svc/socket");
    request.method = "POST".into();
    request
}

/// D15 (WS-15): an upstream that answers an upgrade without upgrading: the reference gives
/// the client a status line with the upstream's status and reason, the body, then closes the
/// connection. No upstream header is passed on; an HTTP/1.1 client gets the body chunked when
/// the upstream sent no `content-length`. The Rust target answers 502 and never relays the
/// upstream's answer (`gateway_websocket_gate::d15_an_upstream_that_does_not_switch_protocols_gets_a_502`).
#[tokio::test]
async fn d15_reference_relays_a_refused_upgrade() {
    run_reference_only(
        "d15_reference_relays_a_refused_upgrade",
        &["WS-15"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let body = br#"{"message":"no upgrade for you"}"#.to_vec();
            for (path, content_length) in [("/svc/sized", true), ("/svc/unsized", false)] {
                mock.route(
                    path,
                    Behaviour::RejectUpgrade {
                        status: 401,
                        reason: "Nope Nope".into(),
                        body: body.clone(),
                        content_length,
                    },
                );
            }
            let mut observed = Vec::new();
            for (label, path, version) in [
                ("HTTP/1.1, content-length", "/svc/sized", "HTTP/1.1"),
                ("HTTP/1.1, no content-length", "/svc/unsized", "HTTP/1.1"),
                ("HTTP/1.0, content-length", "/svc/sized", "HTTP/1.0"),
                ("HTTP/1.0, no content-length", "/svc/unsized", "HTTP/1.0"),
            ] {
                let mut request = client::upgrade_request(path);
                request.version = version;
                let mut conn = RawConn::connect(gw.addr()).await?;
                conn.send(&request).await?;
                let bytes = conn.read_to_close(Duration::from_secs(10)).await?;
                let split = bytes
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .context("no head")?;
                let head = String::from_utf8_lossy(&bytes[..split + 4]).to_string();
                let rest = &bytes[split + 4..];
                let received = if head.contains("transfer-encoding: chunked") {
                    let (decoded, terminated) = decode_chunked(rest)?;
                    ensure!(terminated, "{label}: no last chunk");
                    decoded
                } else {
                    rest.to_vec()
                };
                ensure!(
                    received == body,
                    "{label}: body {:?}",
                    String::from_utf8_lossy(&received)
                );
                observed.push((label.to_string(), head));
            }
            assert_table(
                "D15 (WS-15)",
                &observed,
                &[
                    ("HTTP/1.1, content-length", "HTTP/1.1 401 Nope Nope\r\n\r\n"),
                    (
                        "HTTP/1.1, no content-length",
                        "HTTP/1.1 401 Nope Nope\r\ntransfer-encoding: chunked\r\n\r\n",
                    ),
                    ("HTTP/1.0, content-length", "HTTP/1.0 401 Nope Nope\r\n\r\n"),
                    (
                        "HTTP/1.0, no content-length",
                        "HTTP/1.0 401 Nope Nope\r\n\r\n",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("heads", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// D15 (WS-17, WS-18): the reference upgrades on every path, whatever
/// `websocketRoutePrefixes` says, and never rate limits an upgrade. The Rust target refuses
/// paths off the list with 403 and counts upgrades against the IP Buckets
/// (`gateway_websocket_gate::d15_*`).
#[tokio::test]
async fn d15_reference_upgrades_every_path_without_limits() {
    run_reference_only(
        "d15_reference_upgrades_every_path_without_limits",
        &["WS-17", "WS-18"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.websocket_route_prefixes = Some(vec!["svc/socket".into()]);
                c.throttler(|t| {
                    t.global_ip_rate_limit = 1.0;
                    t.anonymous_ip_rate_limit = Some(1.0);
                    t.anonymous_ip_rate_limit_ttl = Some(60.0);
                })
            })
            .await?;
            mock.set_default(Behaviour::WebSocketEcho);
            // The sockets stay open until the end: the reference crashes (EPIPE) when a
            // client drops a tunnelled socket it is still writing to.
            let mut sockets = Vec::new();
            let mut upgrades = Vec::new();
            for path in ["/svc/v1/users", "/svc/socket", "/svc/socket"] {
                let upgrade = client::websocket(gw.addr(), &client::upgrade_request(path)).await?;
                upgrades.push(format!("{path} {}", upgrade.response_head.status));
                sockets.push(upgrade.socket);
            }
            assert_eq!(
                upgrades,
                ["/svc/v1/users 101", "/svc/socket 101", "/svc/socket 101"]
            );
            obs.record("upgrades with a global limit of 1", &upgrades);
            drop(gw);
            drop(sockets);
            Ok(obs)
        },
    )
    .await;
}

/// Decodes a chunked body; returns it and whether the terminating chunk arrived.
fn decode_chunked(mut bytes: &[u8]) -> Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    loop {
        let Some(end) = bytes.windows(2).position(|w| w == b"\r\n") else {
            return Ok((body, false));
        };
        let size = usize::from_str_radix(std::str::from_utf8(&bytes[..end])?.trim(), 16)?;
        bytes = &bytes[end + 2..];
        if size == 0 {
            ensure!(bytes == b"\r\n", "bytes after the last chunk: {bytes:?}");
            return Ok((body, true));
        }
        ensure!(bytes.len() >= size + 2, "truncated chunk");
        body.extend_from_slice(&bytes[..size]);
        ensure!(
            &bytes[size..size + 2] == b"\r\n",
            "chunk not followed by CRLF"
        );
        bytes = &bytes[size + 2..];
    }
}

/// D2 (RL-17): all limits of a Route share one counter, and so do same-pattern Routes of
/// different services.
#[tokio::test]
async fn d2_route_limits_share_one_counter() {
    run_reference_only("d2_route_limits_share_one_counter", &["RL-17"], |target| async move {
        let mut obs = Observations::new();
        let users = MockUpstream::start().await?;
        users.set_document_json(fixtures::USERS_SERVICE);
        let other = MockUpstream::start().await?;
        other.set_document_json(fixtures::DEFAULT_SERVICE);
        let gw = target
            .start(GatewayConfig::new().service("a", &users).service("b", &other))
            .await?;

        // GET /v1/limits/stacked has [{limit 5, ttl 7200}, {limit 3, ttl 60}]. Each request
        // counts twice on one key, so the second request already exceeds the limit of 3.
        let first = gw.get("/a/v1/limits/stacked").await?;
        let second = gw.get("/a/v1/limits/stacked").await?;
        assert_eq!(first.status, 200);
        assert_eq!(second.status, 429);
        assert_eq!(
            second.text(),
            r#"{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"the next one minute"}}"#
        );
        obs.record("stacked limits, second request", error_view(&second));

        // GET /v1/limits/plain (limit 2) exists in both services: a, b, a exceeds it.
        let statuses = vec![
            gw.get("/a/v1/limits/plain").await?.status,
            gw.get("/b/v1/limits/plain").await?.status,
            gw.get("/a/v1/limits/plain").await?.status,
        ];
        assert_eq!(statuses, vec![200, 200, 429]);
        obs.record("same pattern across services", statuses);
        Ok(obs)
    })
    .await;
}

/// D3 (OAS-17): a path item with a non-operation key makes the whole document fail to load.
#[tokio::test]
async fn d3_path_item_parameters_fail_the_document() {
    run_reference_only(
        "d3_path_item_parameters_fail_the_document",
        &["OAS-17"],
        |target| async move {
            let mut obs = Observations::new();
            let mock = MockUpstream::start().await?;
            mock.set_document_json(fixtures::INVALID_PATH_ITEM);
            let gw = target
                .start(GatewayConfig::new().service("svc", &mock))
                .await?;
            assert_eq!(gw.loaded_services().await?, Vec::<String>::new());
            let response = gw.get("/svc/health").await?;
            assert_eq!(response.status, 503);
            obs.record("request to the service", error_view(&response));
            Ok(obs)
        },
    )
    .await;
}

/// D4 (REQ-08, REQ-15): an unknown method, and a Routed Request to a Default Service whose
/// document is not loaded, answer 500.
#[tokio::test]
async fn d4_unknown_method_and_unloaded_default_answer_500() {
    run_reference_only(
        "d4_unknown_method_and_unloaded_default_answer_500",
        &["REQ-08", "REQ-15"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;
            let unknown = gw.send(&Request::new("PROPFIND", "/svc/v1/users")).await?;
            assert_eq!(unknown.status, 500);
            obs.record("PROPFIND on a Routed Request", error_view(&unknown));
            drop(gw);

            let default = MockUpstream::start().await?;
            default.set_document(Document::Status(500));
            let gw = target
                .start(GatewayConfig::new().service("default", &default))
                .await?;
            let unloaded = gw.get("/anything").await?;
            assert_eq!(unloaded.status, 500);
            obs.record("Default Service without a document", error_view(&unloaded));
            Ok(obs)
        },
    )
    .await;
}

/// D5 (DOC-14, DOC-15): `/document-json` crashes on an unknown `type` and on a document
/// without `components` or with an operation without `parameters`.
#[tokio::test]
async fn d5_document_json_crashes() {
    run_reference_only("d5_document_json_crashes", &["DOC-14", "DOC-15"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let unknown = gw.get("/document-json?type=nope").await?;
        assert_eq!(unknown.status, 500);
        obs.record("unknown type", error_view(&unknown));
        let missing = gw.get("/document-json").await?;
        assert_eq!(missing.status, 500);
        obs.record("missing type", error_view(&missing));

        mock.set_document_json(
            r#"{"openapi":"3.0.0","info":{"title":"t","version":"1"},"paths":{"/x":{"get":{"responses":{}}}}}"#,
        );
        let bare = gw.get("/document-json?type=svc").await?;
        assert_eq!(bare.status, 500);
        obs.record("no components, no parameters", error_view(&bare));
        Ok(obs)
    })
    .await;
}

/// D6 (DOC-04): `/documents` embeds JSON unescaped, and its `?type=` check compares a field
/// the sources do not have.
#[tokio::test]
async fn d6_documents_page_embeds_json_unescaped() {
    run_reference_only(
        "d6_documents_page_embeds_json_unescaped",
        &["DOC-04"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| {
                c.scalar_options(json!({ "x": "</script><script>alert(1)</script>" }))
            })
            .await?;
            let page = gw.get("/documents?type=svc").await?;
            assert_eq!(page.status, 200);
            let html = page.text();
            assert!(html.contains(r#"{"x":"</script><script>alert(1)</script>"}"#));
            assert!(html.contains("doc.name === type"));
            assert!(
                html.contains(r#"[{"title":"svc","slug":"svc","url":"document-json?type=svc"}]"#)
            );
            // The computed `type` is never given to Scalar: the sources go as they are, none
            // marked `default`, so Scalar opens its own default source whatever `?type=` says.
            assert!(html.contains("        sources: apiDocs,\n"));
            let call = &html[html.find("Scalar.createApiReference").expect("Scalar call")..];
            assert!(!call.contains("type"), "{call}");
            obs.record("scalarOptions embedded raw", true);
            obs.record("type check compares doc.name", true);
            obs.record("sources passed unmarked, type unused", true);
            Ok(obs)
        },
    )
    .await;
}

/// D7 (MCP-17) and D8 (MCP-12): MCP path arguments are substituted raw, and a schema
/// referenced twice resolves only the first time.
#[tokio::test]
async fn d7_d8_mcp_raw_path_and_single_ref_resolution() {
    run_reference_only(
        "d7_d8_mcp_raw_path_and_single_ref_resolution",
        &["MCP-17", "MCP-12"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;

            call_tool(
                gw.addr(),
                "svc_UsersController_getUser",
                json!({ "path_id": "../limits/plain" }),
            )
            .await?;
            let forwarded = mock.last_request().expect("forwarded").target;
            assert_eq!(forwarded, "/v1/users/../limits/plain/");
            obs.record("path_id ../limits/plain reaches", forwarded);

            let list = mcp::send(gw.addr(), &mcp::request("tools/list", json!({}))).await?;
            let tools = list.message.expect("tools/list result")["result"]["tools"].clone();
            let create = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == "svc_UsersController_createUser")
                .expect("createUser tool")
                .clone();
            let body = &create["inputSchema"]["properties"]["body"]["properties"];
            assert_eq!(body["homeAddress"]["type"], "object");
            assert_eq!(body["workAddress"], json!({}));
            obs.record("second AddressDto reference", body["workAddress"].clone());
            Ok(obs)
        },
    )
    .await;
}
