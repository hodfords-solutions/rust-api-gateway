//! Reference behaviour behind the approved Deviations (docs/parity/README.md). These run
//! against the reference only: the Rust behaviour for these rows is tested Rust-only.

use std::time::Duration;

use anyhow::Context as _;

use conformance::client::{self, RawConn, ReadOutcome, Request};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mcp::{self, call_tool};
use conformance::mock::{Document, MockUpstream};
use conformance::runner::{Observations, run_reference_only};
use serde_json::json;

use crate::common::{error_view, users_gateway};

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

/// D15 (WS-04, WS-15): an upgrade request on a path that is a Route skips that Route's
/// checks: the proxy middleware does not run (the upstream gets no `x-test-route`, which only
/// the proxy middleware adds) and no Bucket counts it, so a client whose Global IP Bucket is
/// full still reaches the upstream; and an upstream that does not upgrade has its body
/// relayed. Rust: the Route's checks run and only the status line is passed on (tested
/// Rust-only in `security_websocket.rs` and `security_rate_limit.rs`).
#[tokio::test]
async fn d15_upgrades_skip_route_checks_and_relay_bodies() {
    run_reference_only(
        "d15_upgrades_skip_route_checks_and_relay_bodies",
        &["WS-04", "WS-15"],
        |target| async move {
            let (mock, gw) =
                users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 1.0)).await?;
            let mut obs = Observations::new();
            let upgrade =
                || client::upgrade_request("/svc/v1/users").header("x-test-echo-route", "1");

            // The proxy middleware does not run, and the upstream's answer is relayed.
            let mut conn = RawConn::connect(gw.addr()).await?;
            conn.send(&upgrade()).await?;
            let relayed = conn.read_to_close(Duration::from_secs(5)).await?;
            let recorded = mock
                .last_request()
                .context("the upgrade reached the upstream")?;
            let has_route_header = recorded
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("x-test-route"));
            assert!(!has_route_header, "the proxy middleware ran");
            obs.record("upgrade on a Route: upstream target", &recorded.target);
            obs.record("upgrade on a Route: proxy middleware ran", has_route_header);
            let text = String::from_utf8_lossy(&relayed).into_owned();
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text:?}");
            let body_relayed = text.contains("\"target\":\"/svc/v1/users\"");
            assert!(body_relayed, "{text:?}");
            obs.record("upstream's 200 body relayed to the client", body_relayed);

            // The Global IP Bucket (limit 1) rejects plain requests, not upgrades.
            let first = gw.send(&Request::get("/svc/v1/users")).await?;
            let second = gw.send(&Request::get("/svc/v1/users")).await?;
            assert_eq!((first.status, second.status), (200, 429));
            mock.clear_requests();
            let mut conn = RawConn::connect(gw.addr()).await?;
            conn.send(&upgrade()).await?;
            let after = conn.read_to_close(Duration::from_secs(5)).await?;
            let status_line = String::from_utf8_lossy(&after)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            obs.record("plain requests with the Bucket full", second.status);
            obs.record("upgrade with the Bucket full: status line", status_line);
            obs.record(
                "upgrade with the Bucket full: upstream reached",
                mock.last_request().is_some(),
            );
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

/// D16 (MCP-18): a `header_<x>` argument whose name differs only in letter case from a
/// middleware header is sent too, before the middleware's line.
#[tokio::test]
async fn d16_mcp_argument_and_middleware_headers_differing_in_case_are_both_sent() {
    run_reference_only(
        "d16_mcp_argument_and_middleware_headers_differing_in_case_are_both_sent",
        &["MCP-18"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.clear_requests();
            mcp::call_tool_with(
                gw.addr(),
                "svc_UsersController_getUser",
                json!({
                    "path_id": "7",
                    "header_Authorization": "Bearer argument",
                    "header_X-Workspace-Id": "w-argument"
                }),
                &[(
                    "x-test-add-headers",
                    r#"{"authorization":"Bearer middleware","xWorkspaceId":"w-middleware"}"#,
                )],
            )
            .await?;
            let lines = crate::pipeline::header_lines(&mock.last_request().expect("forwarded"));
            let named = |name: &str| {
                lines
                    .iter()
                    .filter(|line| line.split_once(':').is_some_and(|(n, _)| n == name))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                named("authorization"),
                [
                    "authorization: Bearer argument",
                    "authorization: Bearer middleware"
                ]
            );
            assert_eq!(
                named("x-workspace-id"),
                ["x-workspace-id: w-argument", "x-workspace-id: w-middleware"]
            );
            obs.record("header lines", lines);
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
