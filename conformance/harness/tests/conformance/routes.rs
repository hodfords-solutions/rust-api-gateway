//! Route matching: OAS-18, OAS-19 (with OAS-21) and OAS-20.

use conformance::client::Request;
use conformance::mock::MockUpstream;
use conformance::runner::{Gateway, Observations, TargetKind, run};

use crate::common::{assert_table, error_view, matched_route, users_gateway};

/// Sends each request with `x-test-echo-route: 1` and reports the matched Route's
/// operation id (or its path when it has none), or the gateway's status when nothing
/// reached the upstream.
async fn match_table(
    mock: &MockUpstream,
    gw: &Gateway,
    cases: &[(&str, &str)],
) -> anyhow::Result<Vec<(String, String)>> {
    let mut observed = Vec::new();
    for (method, target) in cases {
        mock.clear_requests();
        let response = gw
            .send(&Request::new(method, target).header("x-test-echo-route", "1"))
            .await?;
        let outcome = match mock.last_request() {
            Some(request) => {
                let route = matched_route(&request);
                let name = route["operationId"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("(no operationId) {}", route["path"]));
                format!("{name} -> {}", request.target)
            }
            None => format!("HTTP {}", response.status),
        };
        observed.push((format!("{method} {target}"), outcome));
    }
    Ok(observed)
}

/// OAS-19: `path-to-regexp` 8 `match()` semantics, and OAS-21 (first match in document
/// order wins, so `/v1/users/me` matches `/v1/users/{id}`).
#[tokio::test]
async fn oas_19_route_matching() {
    run("oas_19_route_matching", &["OAS-19", "OAS-21"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let cases = [
            ("GET", "/svc/v1/users"),
            ("GET", "/svc/v1/users/"),
            ("GET", "/svc/v1/users//"),
            ("GET", "/svc/V1/USERS"),
            ("GET", "/svc/v1/users?page=2"),
            ("GET", "/svc/v1/users/42"),
            ("GET", "/svc/v1/users/42/"),
            ("GET", "/svc/v1/users/42/extra"),
            ("GET", "/svc/v1/users/a%2Fb"),
            ("GET", "/svc/v1/users/caf%C3%A9"),
            ("GET", "/svc/v1/users/a;b"),
            ("GET", "/svc/v1/users/%zz"),
            ("GET", "/svc/v1/users/me"),
            ("GET", "/svc/v1/users/ME"),
            ("PUT", "/svc/v1/users/42"),
            ("PATCH", "/svc/v1/users/42"),
            ("DELETE", "/svc/v1/users/42"),
            ("HEAD", "/svc/v1/users/42"),
            ("POST", "/svc/v1/users/42"),
            ("GET", "/SVC/v1/users"),
        ];
        let observed = match_table(&mock, &gw, &cases).await?;
        assert_table(
            "OAS-19",
            &observed,
            &[
                ("GET /svc/v1/users", "UsersController_listUsers -> /v1/users/"),
                ("GET /svc/v1/users/", "UsersController_listUsers -> /v1/users/"),
                ("GET /svc/v1/users//", "HTTP 405"),
                ("GET /svc/V1/USERS", "UsersController_listUsers -> /V1/USERS/"),
                ("GET /svc/v1/users?page=2", "UsersController_listUsers -> /v1/users/?page=2"),
                ("GET /svc/v1/users/42", "UsersController_getUser -> /v1/users/42/"),
                ("GET /svc/v1/users/42/", "UsersController_getUser -> /v1/users/42/"),
                ("GET /svc/v1/users/42/extra", "HTTP 405"),
                ("GET /svc/v1/users/a%2Fb", "UsersController_getUser -> /v1/users/a%2Fb/"),
                ("GET /svc/v1/users/caf%C3%A9", "UsersController_getUser -> /v1/users/caf%C3%A9/"),
                ("GET /svc/v1/users/a;b", "UsersController_getUser -> /v1/users/a;b/"),
                ("GET /svc/v1/users/%zz", "HTTP 400"),
                ("GET /svc/v1/users/me", "UsersController_getUser -> /v1/users/me/"),
                ("GET /svc/v1/users/ME", "UsersController_getUser -> /v1/users/ME/"),
                ("PUT /svc/v1/users/42", "UsersController_replaceUser -> /v1/users/42/"),
                ("PATCH /svc/v1/users/42", "UsersController_updateUser -> /v1/users/42/"),
                ("DELETE /svc/v1/users/42", "UsersController_deleteUser -> /v1/users/42/"),
                ("HEAD /svc/v1/users/42", "UsersController_headUser -> /v1/users/42/"),
                ("POST /svc/v1/users/42", "HTTP 405"),
                ("GET /SVC/v1/users", "HTTP 503"),
            ],
        );
        // Malformed percent-encoding never reaches the gateway: Express's router rejects
        // the `/{*splat}` parameter first.
        let malformed = gw.get("/svc/v1/users/%zz").await?;
        assert_eq!(
            malformed.text(),
            r#"{"message":"Failed to decode param '%zz'","error":"Bad Request","statusCode":400}"#
        );
        // A lower-case method is rejected by Node's parser with 400 and never reaches
        // matching. The Rust target answers 405 under Deviation D4 (REQ-15), tested
        // Rust-only, so this is checked against the reference alone.
        if target.kind == TargetKind::Reference {
            let lower = gw.send(&Request::new("get", "/svc/v1/users")).await?;
            assert_eq!(lower.status, 400, "lower-case method");
        }
        let mut obs = Observations::new();
        obs.record("matches", &observed);
        obs.record("malformed percent-encoding", error_view(&malformed));
        Ok(obs)
    })
    .await;
}

/// OAS-20: `x-router-path` forms beyond `:name` (wildcard, optional group, escape).
#[tokio::test]
async fn oas_20_router_path_syntax() {
    run(
        "oas_20_router_path_syntax",
        &["OAS-20"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let cases = [
                ("GET", "/svc/v1/paths/files/a"),
                ("GET", "/svc/v1/paths/files/a/b/c"),
                ("GET", "/svc/v1/paths/files/"),
                ("GET", "/svc/v1/paths/files"),
                ("GET", "/svc/v1/paths/optional"),
                ("GET", "/svc/v1/paths/optional/5"),
                ("GET", "/svc/v1/paths/optional/5/6"),
                ("GET", "/svc/v1/paths/escaped:literal"),
                ("GET", "/svc/v1/paths/escaped%3Aliteral"),
                ("GET", "/svc/v1/paths/escaped"),
                ("GET", "/svc/v1/paths/anonymous/x"),
            ];
            let observed = match_table(&mock, &gw, &cases).await?;
            assert_table(
                "OAS-20",
                &observed,
                &[
                    (
                        "GET /svc/v1/paths/files/a",
                        "PathsController_files -> /v1/paths/files/a/",
                    ),
                    (
                        "GET /svc/v1/paths/files/a/b/c",
                        "PathsController_files -> /v1/paths/files/a/b/c/",
                    ),
                    ("GET /svc/v1/paths/files/", "HTTP 405"),
                    ("GET /svc/v1/paths/files", "HTTP 405"),
                    (
                        "GET /svc/v1/paths/optional",
                        "PathsController_optional -> /v1/paths/optional/",
                    ),
                    (
                        "GET /svc/v1/paths/optional/5",
                        "PathsController_optional -> /v1/paths/optional/5/",
                    ),
                    ("GET /svc/v1/paths/optional/5/6", "HTTP 405"),
                    (
                        "GET /svc/v1/paths/escaped:literal",
                        "PathsController_escaped -> /v1/paths/escaped:literal/",
                    ),
                    ("GET /svc/v1/paths/escaped%3Aliteral", "HTTP 405"),
                    ("GET /svc/v1/paths/escaped", "HTTP 405"),
                    (
                        "GET /svc/v1/paths/anonymous/x",
                        "(no operationId) \"/v1/paths/anonymous/{slug}\" -> /v1/paths/anonymous/x/",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("matches", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// OAS-18: matching reads the WHATWG URL path of the request target, so the query and
/// fragment are ignored, dot segments (also percent-encoded ones) are resolved, `\` reads
/// as `/`, and characters URL parsing percent-encodes are compared encoded. Only the
/// matched Route is recorded; what is forwarded belongs to the request pipeline.
#[tokio::test]
async fn oas_18_matching_uses_the_url_path() {
    run(
        "oas_18_matching_uses_the_url_path",
        &["OAS-18"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let cases = [
                ("GET", "/svc/v1/users?redirect=/v1/users/42"),
                ("GET", "/svc/v1/users#fragment"),
                ("GET", "/svc/v1/users/42?x=1#y"),
                ("GET", "/svc/v1/./users"),
                ("GET", "/svc/v1/users/../users/42"),
                ("GET", "/svc/v1/users/%2e%2e/users/42"),
                ("GET", "/svc/v1/users/42/.."),
                ("GET", "/svc/v1\\users"),
                ("GET", "/svc/v1/users/a^b"),
                ("GET", "/svc/v1/users/a{b}"),
                ("GET", "/svc/v1/users/a|b"),
                ("GET", "/svc/v1/paths/escaped:literal?q"),
            ];
            let observed: Vec<(String, String)> = match_table(&mock, &gw, &cases)
                .await?
                .into_iter()
                .map(|(request, outcome)| {
                    let route = outcome.split(" -> ").next().unwrap_or_default().to_string();
                    (request, route)
                })
                .collect();
            assert_table(
                "OAS-18",
                &observed,
                &[
                    (
                        "GET /svc/v1/users?redirect=/v1/users/42",
                        "UsersController_listUsers",
                    ),
                    ("GET /svc/v1/users#fragment", "UsersController_listUsers"),
                    ("GET /svc/v1/users/42?x=1#y", "UsersController_getUser"),
                    ("GET /svc/v1/./users", "UsersController_listUsers"),
                    ("GET /svc/v1/users/../users/42", "UsersController_getUser"),
                    (
                        "GET /svc/v1/users/%2e%2e/users/42",
                        "UsersController_getUser",
                    ),
                    ("GET /svc/v1/users/42/..", "UsersController_listUsers"),
                    ("GET /svc/v1\\users", "UsersController_listUsers"),
                    ("GET /svc/v1/users/a^b", "UsersController_getUser"),
                    ("GET /svc/v1/users/a{b}", "UsersController_getUser"),
                    ("GET /svc/v1/users/a|b", "UsersController_getUser"),
                    (
                        "GET /svc/v1/paths/escaped:literal?q",
                        "PathsController_escaped",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("matches", &observed);
            Ok(obs)
        },
    )
    .await;
}
