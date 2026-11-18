//! OpenAPI ingestion: OAS-01, OAS-02, OAS-03, OAS-10, OAS-11, OAS-16, OAS-19, OAS-21, and
//! the reference side of OAS-23 (Deviation D3).
//!
//! The refresh scenarios wait for the gateway's 10-second refresh, so each takes up to
//! about 12 seconds.

use std::time::Duration;

use conformance::client::Request;
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::{Document, MockUpstream};
use conformance::runner::{Gateway, Observations, run, run_reference_only};
use serde_json::{Value, json};

use crate::common::{assert_table, error_view, matched_route, users_gateway};

/// Longest wait for a refresh: one 10-second period plus margin.
const REFRESH_WAIT: Duration = Duration::from_secs(15);

/// Sends `method target` with `x-test-echo-route: 1` and describes the outcome: the
/// matched Route's operation id (or path) when the request reached the upstream,
/// otherwise the gateway's status.
async fn routed(
    mock: &MockUpstream,
    gw: &Gateway,
    method: &str,
    target: &str,
) -> anyhow::Result<String> {
    mock.clear_requests();
    let response = gw
        .send(&Request::new(method, target).header("x-test-echo-route", "1"))
        .await?;
    Ok(match mock.last_request() {
        Some(request) => {
            let route = matched_route(&request);
            route["operationId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("(no operationId) {}", route["path"]))
        }
        None => format!("HTTP {}", response.status),
    })
}

/// The echoed Route of a request that reaches the upstream.
async fn echoed_route(
    mock: &MockUpstream,
    gw: &Gateway,
    method: &str,
    target: &str,
) -> anyhow::Result<(u16, Value)> {
    mock.clear_requests();
    let response = gw
        .send(&Request::new(method, target).header("x-test-echo-route", "1"))
        .await?;
    let route = mock
        .last_request()
        .map(|request| matched_route(&request))
        .unwrap_or(Value::Null);
    Ok((response.status, route))
}

/// Waits until `mock`'s document has been fetched more than `after` times, then gives the
/// gateway a moment to apply the response.
async fn wait_for_fetch(mock: &MockUpstream, after: u64) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + REFRESH_WAIT;
    while mock.document_fetches() <= after {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "document of {} not fetched again within {REFRESH_WAIT:?}",
                mock.addr()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

fn document(paths: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "openapi": "3.0.0",
        "info": { "title": "conformance", "version": "1.0.0" },
        "paths": paths,
        "components": {}
    }))
    .expect("document serialises")
}

/// OAS-01: at startup every document is fetched; a failed fetch (error status or a
/// dropped connection) leaves that service without a document while the others load.
#[tokio::test]
async fn oas_01_startup_loads_every_document() {
    run(
        "oas_01_startup_loads_every_document",
        &["OAS-01"],
        |target| async move {
            let ok = MockUpstream::start().await?;
            ok.set_document_json(fixtures::USERS_SERVICE);
            let failing = MockUpstream::start().await?;
            failing.set_document(Document::Status(500));
            let closing = MockUpstream::start().await?;
            closing.set_document(Document::Close);
            let gw = target
                .start(
                    GatewayConfig::new()
                        .service("ok", &ok)
                        .service("failing", &failing)
                        .service("closing", &closing),
                )
                .await?;
            let loaded = gw.loaded_services().await?;
            assert_eq!(loaded, ["ok"]);
            for mock in [&ok, &failing, &closing] {
                assert!(
                    mock.document_fetches() >= 1,
                    "document of {} fetched",
                    mock.addr()
                );
            }
            let observed = vec![
                (
                    "GET /ok/v1/users".to_string(),
                    routed(&ok, &gw, "GET", "/ok/v1/users").await?,
                ),
                (
                    "GET /failing/v1/users".to_string(),
                    routed(&failing, &gw, "GET", "/failing/v1/users").await?,
                ),
                (
                    "GET /closing/v1/users".to_string(),
                    routed(&closing, &gw, "GET", "/closing/v1/users").await?,
                ),
            ];
            assert_table(
                "OAS-01",
                &observed,
                &[
                    ("GET /ok/v1/users", "UsersController_listUsers"),
                    ("GET /failing/v1/users", "HTTP 503"),
                    ("GET /closing/v1/users", "HTTP 503"),
                ],
            );
            let unloaded = gw.get("/failing/v1/users").await?;
            let mut obs = Observations::new();
            obs.record("loaded services", &loaded);
            obs.record("requests", &observed);
            obs.record(
                "request to a service without a document",
                error_view(&unloaded),
            );
            Ok(obs)
        },
    )
    .await;
}

/// OAS-02: a changed document changes routing on the next refresh, without a restart.
#[tokio::test]
async fn oas_02_refresh_changes_routing() {
    run(
        "oas_02_refresh_changes_routing",
        &["OAS-02"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let before = routed(&mock, &gw, "GET", "/svc/v2/new").await?;
            assert_eq!(before, "HTTP 405");

            let fetches = mock.document_fetches();
            mock.set_document_json(fixtures::USERS_SERVICE_V2);
            wait_for_fetch(&mock, fetches).await?;
            let after = routed(&mock, &gw, "GET", "/svc/v2/new").await?;
            assert_eq!(after, "NewController_newRoute");
            let unchanged = routed(&mock, &gw, "GET", "/svc/v1/users").await?;
            assert_eq!(unchanged, "UsersController_listUsers");

            let mut obs = Observations::new();
            obs.record("GET /svc/v2/new before the change", &before);
            obs.record("GET /svc/v2/new after a refresh", &after);
            obs.record("GET /svc/v1/users after a refresh", &unchanged);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-03: a failed refresh keeps the previous document, whatever the failure: an error
/// status, a dropped connection, a body that is not JSON, or a document whose Routes
/// cannot be built. The service is never unloaded.
#[tokio::test]
async fn oas_03_failed_refresh_keeps_the_document() {
    run(
        "oas_03_failed_refresh_keeps_the_document",
        &["OAS-03"],
        |target| async move {
            let mut invalid: Value = serde_json::from_str(fixtures::USERS_SERVICE)?;
            invalid["paths"]["/v1/users"]["get"]["x-router-path"] = json!("/v1/users/(");
            let failures = [
                ("status", Document::Status(500)),
                ("closed", Document::Close),
                ("text", Document::Json(b"<html>not json</html>".to_vec())),
                ("invalid", Document::Json(serde_json::to_vec(&invalid)?)),
            ];
            let mut mocks = Vec::new();
            let mut config = GatewayConfig::new();
            for (prefix, _) in &failures {
                let mock = MockUpstream::start().await?;
                mock.set_document_json(fixtures::USERS_SERVICE);
                config = config.service(prefix, &mock);
                mocks.push(mock);
            }
            let gw = target.start(config).await?;
            let prefixes: Vec<&str> = failures.iter().map(|(p, _)| *p).collect();
            // `loaded_services` lists services in first-load order. The four loads run
            // concurrently, so that order is not fixed; compare them sorted.
            let sorted = |mut list: Vec<String>| {
                list.sort();
                list
            };
            let mut expected: Vec<String> = prefixes.iter().map(|p| p.to_string()).collect();
            expected.sort();
            assert_eq!(sorted(gw.loaded_services().await?), expected);

            let mut fetches = Vec::new();
            for (mock, (_, failure)) in mocks.iter().zip(failures.iter()) {
                fetches.push(mock.document_fetches());
                mock.set_document(failure.clone());
            }
            for (mock, before) in mocks.iter().zip(&fetches) {
                wait_for_fetch(mock, *before).await?;
            }

            let mut observed = Vec::new();
            for (mock, prefix) in mocks.iter().zip(&prefixes) {
                let target = format!("/{prefix}/v1/users");
                observed.push((
                    format!("GET {target}"),
                    routed(mock, &gw, "GET", &target).await?,
                ));
            }
            assert_table(
                "OAS-03",
                &observed,
                &[
                    ("GET /status/v1/users", "UsersController_listUsers"),
                    ("GET /closed/v1/users", "UsersController_listUsers"),
                    ("GET /text/v1/users", "UsersController_listUsers"),
                    ("GET /invalid/v1/users", "UsersController_listUsers"),
                ],
            );
            let loaded = sorted(gw.loaded_services().await?);
            assert_eq!(loaded, expected);
            let mut obs = Observations::new();
            obs.record("requests after a failed refresh", &observed);
            obs.record("loaded services", &loaded);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-10: a match pattern `path-to-regexp` rejects, from `x-router-path` or from the
/// OpenAPI path itself, makes the whole document fail to load.
#[tokio::test]
async fn oas_10_rejected_pattern_fails_the_document() {
    run(
        "oas_10_rejected_pattern_fails_the_document",
        &["OAS-10"],
        |target| async move {
            let router_path = MockUpstream::start().await?;
            router_path.set_document(Document::Json(document(json!({
                "/ok": { "get": { "operationId": "ok" } },
                "/bad": { "get": { "operationId": "bad", "x-router-path": "/bad/:a:b" } }
            }))));
            let openapi_path = MockUpstream::start().await?;
            openapi_path.set_document(Document::Json(document(json!({
                "/ok": { "get": { "operationId": "ok" } },
                "/items({id})": { "get": { "operationId": "items" } }
            }))));
            let control = MockUpstream::start().await?;
            control.set_document(Document::Json(document(json!({
                "/ok": { "get": { "operationId": "ok" } }
            }))));
            let gw = target
                .start(
                    GatewayConfig::new()
                        .service("routerpath", &router_path)
                        .service("openapipath", &openapi_path)
                        .service("control", &control),
                )
                .await?;
            let loaded = gw.loaded_services().await?;
            assert_eq!(loaded, ["control"]);
            let observed = vec![
                (
                    "GET /routerpath/ok".to_string(),
                    routed(&router_path, &gw, "GET", "/routerpath/ok").await?,
                ),
                (
                    "GET /openapipath/ok".to_string(),
                    routed(&openapi_path, &gw, "GET", "/openapipath/ok").await?,
                ),
                (
                    "GET /control/ok".to_string(),
                    routed(&control, &gw, "GET", "/control/ok").await?,
                ),
            ];
            assert_table(
                "OAS-10",
                &observed,
                &[
                    ("GET /routerpath/ok", "HTTP 503"),
                    ("GET /openapipath/ok", "HTTP 503"),
                    ("GET /control/ok", "ok"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("loaded services", &loaded);
            obs.record("requests", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-11: `x-rate-limits` reaches the Route as a list of `{ limit, ttl, status? }`, and
/// an operation without it has no Route limits.
#[tokio::test]
async fn oas_11_rate_limit_extension() {
    run(
        "oas_11_rate_limit_extension",
        &["OAS-11"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let cases = [
                ("GET", "/svc/v1/users"),
                ("GET", "/svc/v1/limits/plain"),
                ("POST", "/svc/v1/limits/on-status"),
                ("GET", "/svc/v1/limits/stacked"),
                ("GET", "/svc/v1/limits/stacked-status"),
            ];
            let mut observed = Vec::new();
            for (method, target) in cases {
                let (status, route) = echoed_route(&mock, &gw, method, target).await?;
                assert_eq!(status, 200, "{method} {target}");
                observed.push((
                    format!("{method} {target}"),
                    route["rateLimits"].to_string(),
                ));
            }
            assert_table(
                "OAS-11",
                &observed,
                &[
                    ("GET /svc/v1/users", "[]"),
                    ("GET /svc/v1/limits/plain", r#"[{"limit":2,"ttl":60}]"#),
                    (
                        "POST /svc/v1/limits/on-status",
                        r#"[{"limit":1,"ttl":60,"status":201}]"#,
                    ),
                    (
                        "GET /svc/v1/limits/stacked",
                        r#"[{"limit":5,"ttl":7200},{"limit":3,"ttl":60}]"#,
                    ),
                    (
                        "GET /svc/v1/limits/stacked-status",
                        r#"[{"limit":1,"ttl":3600,"status":404},{"limit":2,"ttl":60}]"#,
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("rate limits", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-16 (with OAS-12 and OAS-13): the security flags are only information for
/// middleware; a request without any credentials still reaches a Route that requires
/// them.
#[tokio::test]
async fn oas_16_security_flags_never_reject() {
    run(
        "oas_16_security_flags_never_reject",
        &["OAS-16", "OAS-12", "OAS-13"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let cases = [
                ("GET", "/svc/v1/users"),
                ("GET", "/svc/v1/users/1"),
                ("PUT", "/svc/v1/users/1"),
                ("PATCH", "/svc/v1/users/1"),
                ("GET", "/svc/v1/users/me"),
            ];
            let mut observed = Vec::new();
            for (method, target) in cases {
                let (status, route) = echoed_route(&mock, &gw, method, target).await?;
                observed.push((
                    format!("{method} {target}"),
                    format!(
                        "{status} {} bearer={} apiKey={}",
                        route["operationId"].as_str().unwrap_or("-"),
                        route["isBearerAuth"],
                        route["isApiKeyAuth"]
                    ),
                ));
            }
            assert_table(
                "OAS-16",
                &observed,
                &[
                    (
                        "GET /svc/v1/users",
                        "200 UsersController_listUsers bearer=false apiKey=false",
                    ),
                    (
                        "GET /svc/v1/users/1",
                        "200 UsersController_getUser bearer=true apiKey=false",
                    ),
                    (
                        "PUT /svc/v1/users/1",
                        "200 UsersController_replaceUser bearer=false apiKey=true",
                    ),
                    (
                        "PATCH /svc/v1/users/1",
                        "200 UsersController_updateUser bearer=true apiKey=true",
                    ),
                    // `/v1/users/{id}` comes first, so `me` is routed to getUser (OAS-21).
                    (
                        "GET /svc/v1/users/me",
                        "200 UsersController_getUser bearer=true apiKey=false",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("unauthenticated requests", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-21: among Routes of the requested method the first match in document order wins,
/// whether the earlier Route has a parameter or a literal, and Routes of other methods
/// do not take part.
#[tokio::test]
async fn oas_21_first_match_in_document_order() {
    run(
        "oas_21_first_match_in_document_order",
        &["OAS-21"],
        |target| async move {
            let mock = MockUpstream::start().await?;
            mock.set_document(Document::Json(document(json!({
                "/items/{id}": { "get": { "operationId": "items_param" } },
                "/items/special": {
                    "get": { "operationId": "items_literal" },
                    "post": { "operationId": "items_literal_post" }
                },
                "/things/special": { "get": { "operationId": "things_literal" } },
                "/things/{id}": {
                    "get": { "operationId": "things_param" },
                    "post": { "operationId": "things_param_post" }
                },
                "/aliased": { "get": { "operationId": "aliased", "x-router-path": "/things/:id" } }
            }))));
            let gw = target
                .start(GatewayConfig::new().service("svc", &mock))
                .await?;
            let cases = [
                ("GET", "/svc/items/special"),
                ("GET", "/svc/items/1"),
                ("POST", "/svc/items/special"),
                ("POST", "/svc/items/1"),
                ("GET", "/svc/things/special"),
                ("GET", "/svc/things/1"),
                ("POST", "/svc/things/special"),
                ("GET", "/svc/aliased"),
            ];
            let mut observed = Vec::new();
            for (method, target) in cases {
                observed.push((
                    format!("{method} {target}"),
                    routed(&mock, &gw, method, target).await?,
                ));
            }
            assert_table(
                "OAS-21",
                &observed,
                &[
                    ("GET /svc/items/special", "items_param"),
                    ("GET /svc/items/1", "items_param"),
                    ("POST /svc/items/special", "items_literal_post"),
                    ("POST /svc/items/1", "HTTP 405"),
                    ("GET /svc/things/special", "things_literal"),
                    ("GET /svc/things/1", "things_param"),
                    ("POST /svc/things/special", "things_param_post"),
                    ("GET /svc/aliased", "HTTP 405"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("matches", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-19: a matched parameter that is not valid percent-encoded UTF-8 on its own makes
/// the lookup fail. Express decodes the whole path first, so this needs a pattern whose
/// literal text splits an encoded character: `%C3%A9` matched by `:a%A9` leaves `%C3`.
#[tokio::test]
async fn oas_19_undecodable_parameter_fails_the_lookup() {
    run(
        "oas_19_undecodable_parameter_fails_the_lookup",
        &["OAS-19"],
        |target| async move {
            let mock = MockUpstream::start().await?;
            mock.set_document(Document::Json(document(json!({
                "/split": { "get": { "operationId": "split", "x-router-path": "/split/:a%A9" } }
            }))));
            let gw = target
                .start(GatewayConfig::new().service("svc", &mock))
                .await?;
            // Express decodes the whole path first: `x%A9` alone is rejected there.
            let express = routed(&mock, &gw, "GET", "/svc/split/x%A9").await?;
            assert_eq!(express, "HTTP 400");
            let unmatched = routed(&mock, &gw, "GET", "/svc/split/x").await?;
            assert_eq!(unmatched, "HTTP 405");
            // The whole path decodes, but the captured `x%C3` / `%C3` does not.
            let split = routed(&mock, &gw, "GET", "/svc/split/x%C3%A9").await?;
            assert_eq!(split, "HTTP 500");
            let lead_only = routed(&mock, &gw, "GET", "/svc/split/%C3%A9").await?;
            assert_eq!(lead_only, "HTTP 500");
            let response = gw.get("/svc/split/%C3%A9").await?;
            let mut obs = Observations::new();
            obs.record("x%A9", &express);
            obs.record("x", &unmatched);
            obs.record("x%C3%A9", &split);
            obs.record("%C3%A9", &lead_only);
            obs.record("response", error_view(&response));
            Ok(obs)
        },
    )
    .await;
}

/// Deviation D3 (OAS-23), reference side: a document without `info` does not load, so
/// its service answers 503. The Rust behaviour is tested Rust-only.
#[tokio::test]
async fn d3_document_without_info_fails_the_document() {
    run_reference_only(
        "d3_document_without_info_fails_the_document",
        &["OAS-23"],
        |target| async move {
            let mock = MockUpstream::start().await?;
            mock.set_document(Document::Json(serde_json::to_vec(&json!({
                "openapi": "3.0.0",
                "paths": { "/ok": { "get": { "operationId": "ok" } } },
                "components": {}
            }))?));
            let gw = target
                .start(GatewayConfig::new().service("svc", &mock))
                .await?;
            assert_eq!(gw.loaded_services().await?, Vec::<String>::new());
            let response = gw.get("/svc/ok").await?;
            assert_eq!(response.status, 503);
            let mut obs = Observations::new();
            obs.record("request to the service", error_view(&response));
            Ok(obs)
        },
    )
    .await;
}
