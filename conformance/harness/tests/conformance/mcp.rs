//! MCP tool calls: MCP-19.

use conformance::mcp::{call_tool, result_text};
use conformance::mock::{Behaviour, Reply};
use conformance::runner::{Observations, run};
use serde_json::json;

use crate::common::users_gateway;

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

        let ok = call_tool(gw.addr(), "svc_UsersController_getUser", json!({ "path_id": "7" })).await?;
        assert_eq!(result_text(&ok), "caf\u{e9} \u{2713}");
        assert_eq!(ok.get("isError"), None);
        obs.record("upstream 200", &ok);

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
