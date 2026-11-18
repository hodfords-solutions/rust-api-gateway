//! Route-table, extension and security-flag cases ported from
//! `libs/api-gateway/restful/services/open-api.service.spec.ts`, plus the edge cases of the
//! reference's `getEndpointDetail`.

use serde_json::{Value, json};

use super::*;

fn sample_doc() -> Value {
    json!({
        "info": { "title": "User Service", "version": "1.0.0" },
        "paths": {
            "/users/{id}": {
                "get": {
                    "operationId": "UserController_show",
                    "description": "Get a user",
                    "parameters": [],
                    "responses": {},
                    "security": [{ "bearer": [] }]
                },
                "delete": {
                    "operationId": "UserController_delete",
                    "x-rate-limits": [{ "limit": 5, "ttl": 60 }],
                    "security": [{ "auth-jwt": [] }]
                }
            },
            "/health": {
                "get": {
                    "operationId": "HealthController_check",
                    "x-router-path": "/healthz"
                }
            }
        }
    })
}

fn keys() -> SecurityKeys {
    SecurityKeys {
        bearer: vec!["auth-jwt".into()],
        api_key: vec!["api-key".into()],
    }
}

fn build(doc: &Value) -> ServiceDocument {
    build_service_document("http://users/doc", doc, &keys()).expect("document builds")
}

fn only_route(operation: Value) -> Route {
    let doc = json!({ "info": {}, "paths": { "/x/{id}": { "get": operation } } });
    let document = build(&doc);
    document.routes.routes(RouteMethod::Get)[0].as_ref().clone()
}

fn try_route(operation: Value, security: &SecurityKeys) -> Result<ServiceDocument, BuildError> {
    let doc = json!({ "info": {}, "paths": { "/x/{id}": { "get": operation } } });
    build_service_document("http://users/doc", &doc, security)
}

fn find<'a>(document: &'a ServiceDocument, method: RouteMethod, id: &str) -> &'a Route {
    document
        .routes
        .routes(method)
        .iter()
        .find(|r| r.operation_id.as_deref() == Some(id))
        .expect("route exists")
}

// OpenApiService.convertToExpressPath

#[test]
fn oas_10_converts_swagger_params_to_express_params() {
    assert_eq!(
        convert_to_express_path("/users/{id}/posts/{postId}"),
        "/users/:id/posts/:postId"
    );
}

#[test]
fn oas_10_leaves_paths_without_params_unchanged() {
    assert_eq!(convert_to_express_path("/users"), "/users");
}

#[test]
fn oas_10_only_converts_word_character_names() {
    assert_eq!(
        convert_to_express_path("/a/{a-b}/{c.d}/{e_1}"),
        "/a/{a-b}/{c.d}/:e_1"
    );
    assert_eq!(convert_to_express_path("/a/{é}"), "/a/{é}");
}

// OpenApiService.getEndpointDetail

#[test]
fn oas_06_oas_07_oas_08_builds_router_entries_per_method_with_matchers_and_metadata() {
    let document = build(&sample_doc());
    assert_eq!(document.title.as_deref(), Some("User Service"));
    assert_eq!(document.version.as_deref(), Some("1.0.0"));
    assert_eq!(document.doc_url, "http://users/doc");
    assert_eq!(document.routes.routes(RouteMethod::Get).len(), 2);
    assert_eq!(document.routes.routes(RouteMethod::Delete).len(), 1);
    assert_eq!(document.routes.routes(RouteMethod::Post).len(), 0);

    let show = find(&document, RouteMethod::Get, "UserController_show");
    assert_eq!(show.router_path, "/users/:id");
    assert_eq!(show.path, "/users/{id}");
    assert_eq!(show.description, "Get a user");
    assert!(show.is_bearer_auth);
    assert!(show.match_path("/users/42").unwrap());
    assert!(!show.match_path("/users").unwrap());
}

#[test]
fn oas_10_honors_the_x_router_path_override() {
    let document = build(&sample_doc());
    let health = find(&document, RouteMethod::Get, "HealthController_check");
    assert_eq!(health.router_path, "/healthz");
    assert!(health.match_path("/healthz").unwrap());
    assert!(!health.match_path("/health").unwrap());
}

#[test]
fn oas_11_extracts_x_rate_limits_into_rate_limits() {
    let document = build(&sample_doc());
    let delete = &document.routes.routes(RouteMethod::Delete)[0];
    assert_eq!(
        delete.rate_limits,
        vec![RouteLimit {
            limit: 5,
            ttl: 60.0,
            status: None
        }]
    );
    let show = find(&document, RouteMethod::Get, "UserController_show");
    assert!(show.rate_limits.is_empty());
}

#[test]
fn oas_07_keeps_document_order_within_a_method() {
    let doc = json!({
        "info": {},
        "paths": {
            "/b": { "post": { "operationId": "b-post" }, "get": { "operationId": "b-get" } },
            "/a": { "get": { "operationId": "a-get" } },
            "/c": { "search": { "operationId": "c-search" }, "options": { "operationId": "c-options" }, "head": { "operationId": "c-head" } }
        }
    });
    let document = build(&doc);
    let ids = |m| -> Vec<String> {
        document
            .routes
            .routes(m)
            .iter()
            .filter_map(|r| r.operation_id.clone())
            .collect()
    };
    assert_eq!(ids(RouteMethod::Get), ["b-get", "a-get"]);
    assert_eq!(ids(RouteMethod::Post), ["b-post"]);
    assert_eq!(ids(RouteMethod::Search), ["c-search"]);
    assert_eq!(ids(RouteMethod::Options), ["c-options"]);
    assert_eq!(ids(RouteMethod::Head), ["c-head"]);
    assert_eq!(document.routes.len(), 6);
}

#[test]
fn oas_08_defaults_for_missing_fields() {
    let route = only_route(json!({}));
    assert_eq!(route.operation_id, None);
    assert_eq!(route.description, "");
    assert!(!route.is_bearer_auth);
    assert!(!route.is_api_key_auth);
    assert!(route.rate_limits.is_empty());
    assert!(route.extra.is_empty());
    assert_eq!(route.router_path, "/x/:id");
    let route = only_route(json!({ "operationId": null, "description": null }));
    assert_eq!(route.operation_id, None);
    assert_eq!(route.description, "");
}

#[test]
fn oas_09_reads_camel_cased_extension_keys() {
    // Any spelling that camel-cases to xRouterPath / xRateLimits is read.
    let route = only_route(
        json!({ "x_router_path": "/other", "X-Rate-Limits": [{ "limit": 1, "ttl": 2 }] }),
    );
    assert_eq!(route.router_path, "/other");
    assert_eq!(route.rate_limits.len(), 1);
    // The last spelling wins.
    let route = only_route(json!({ "x-router-path": "/first", "xRouterPath": "/second" }));
    assert_eq!(route.router_path, "/second");
}

#[test]
fn oas_10_falsy_router_path_falls_back_to_the_openapi_path() {
    for falsy in [json!(""), json!(null), json!(false), json!(0)] {
        let route = only_route(json!({ "x-router-path": falsy }));
        assert_eq!(route.router_path, "/x/:id");
    }
}

#[test]
fn oas_10_router_path_lists_compile_together() {
    let route = only_route(json!({ "x-router-path": ["/a", ["/b/:id"], []] }));
    assert_eq!(route.router_path, "/a,/b/:id,");
    assert!(route.match_path("/a").unwrap());
    assert!(route.match_path("/b/1").unwrap());
    assert!(!route.match_path("/x/1").unwrap());
}

#[test]
fn oas_10_rejected_patterns_fail_the_document() {
    for pattern in [
        json!("/bad("),
        json!("/:a:b"),
        json!(["/ok", "/{open"]),
        json!(5),
        json!(true),
        json!({}),
    ] {
        let error = try_route(json!({ "x-router-path": pattern }), &keys()).unwrap_err();
        assert!(
            matches!(
                error,
                BuildError::Pattern { .. } | BuildError::RouterPathType { .. }
            ),
            "{error:?}"
        );
    }
    // A path template path-to-regexp rejects fails as well.
    let doc =
        json!({ "info": {}, "paths": { "/v1/items({id})": { "get": {} }, "/ok": { "get": {} } } });
    let error = build_service_document("u", &doc, &keys()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "get /v1/items({id}): Unexpected ( at index 9: /v1/items(:id); visit https://git.new/pathToRegexpError for info"
    );
}

#[test]
fn oas_10_braces_that_are_not_parameters_are_optional_groups() {
    let doc = json!({ "info": {}, "paths": { "/users/{a-b}": { "get": {} } } });
    let route = build(&doc).routes.routes(RouteMethod::Get)[0].clone();
    assert_eq!(route.router_path, "/users/{a-b}");
    assert!(route.match_path("/users/").unwrap());
    assert!(route.match_path("/users/a-b").unwrap());
}

// OpenApiService security detection

#[test]
fn oas_12_detects_the_built_in_bearer_security_scheme() {
    assert!(only_route(json!({ "security": [{ "bearer": [] }] })).is_bearer_auth);
}

#[test]
fn oas_12_detects_configured_open_api_security_keys() {
    assert!(only_route(json!({ "security": [{ "auth-jwt": [] }] })).is_bearer_auth);
}

#[test]
fn oas_12_returns_false_without_a_security_section_or_with_unknown_schemes() {
    assert!(!only_route(json!({})).is_bearer_auth);
    assert!(!only_route(json!({ "security": [{ "other": [] }] })).is_bearer_auth);
}

#[test]
fn oas_12_any_requirement_counts_and_the_value_must_be_truthy() {
    assert!(
        only_route(json!({ "security": [{ "other": [] }, { "bearer": ["scope"] }] }))
            .is_bearer_auth
    );
    assert!(!only_route(json!({ "security": [{ "bearer": null }] })).is_bearer_auth);
    assert!(!only_route(json!({ "security": [] })).is_bearer_auth);
    assert!(!only_route(json!({ "security": "bearer" })).is_bearer_auth);
}

#[test]
fn oas_13_detects_configured_api_key_security_schemes() {
    assert!(only_route(json!({ "security": [{ "api-key": [] }] })).is_api_key_auth);
    assert!(!only_route(json!({ "security": [{ "bearer": [] }] })).is_api_key_auth);
    assert!(!only_route(json!({})).is_api_key_auth);
}

#[test]
fn oas_13_returns_false_for_the_api_key_check_when_none_are_configured() {
    let none = SecurityKeys {
        bearer: vec!["auth-jwt".into()],
        api_key: vec![],
    };
    let document = try_route(json!({ "security": [{ "api-key": [] }] }), &none).unwrap();
    assert!(!document.routes.routes(RouteMethod::Get)[0].is_api_key_auth);
}

#[test]
fn oas_12_oas_13_security_that_is_not_a_list_fails_the_document() {
    for security in [json!({ "bearer": [] }), json!(1), json!(true)] {
        assert!(matches!(
            try_route(json!({ "security": security }), &keys()),
            Err(BuildError::Security { .. })
        ));
    }
    // A null requirement fails once a check reaches it.
    assert!(try_route(json!({ "security": [null] }), &keys()).is_err());
    // The bearer check stops at its match; the API-key check with no names stops before
    // reading the first requirement.
    let none = SecurityKeys::default();
    let document = try_route(json!({ "security": [{ "bearer": [] }, null] }), &none).unwrap();
    assert!(document.routes.routes(RouteMethod::Get)[0].is_bearer_auth);
    assert!(try_route(json!({ "security": [{ "bearer": [] }, null] }), &keys()).is_err());
}

// OpenApiService.getExtraDetails

#[test]
fn oas_14_keeps_unknown_keys_and_drops_parameters_responses_and_handled_extensions() {
    let route = only_route(json!({
        "operationId": "op",
        "parameters": [],
        "responses": {},
        "x-rate-limits": [],
        "x-router-path": "/x",
        "x-api-mcp": [{ "description": "tool" }]
    }));
    assert_eq!(
        Value::Object(route.extra),
        json!({ "operationId": "op", "xApiMcp": [{ "description": "tool" }] })
    );
}

#[test]
fn oas_14_extra_holds_every_other_camel_cased_key_in_javascript_order() {
    let route = only_route(json!({
        "tags": ["users"],
        "summary": "s",
        "description": "d",
        "requestBody": {},
        "security": [{ "bearer": [] }],
        "x-codeSamples": [],
        "x-custom_thing": 1,
        "2": "two",
        "1": "one"
    }));
    let keys: Vec<&str> = route.extra.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "1",
            "2",
            "tags",
            "summary",
            "description",
            "requestBody",
            "security",
            "xCodeSamples",
            "xCustomThing"
        ]
    );
}

#[test]
fn oas_15_x_api_mcp_is_only_visible_in_extra() {
    let route = only_route(json!({ "x-api-mcp": [{ "description": "List users" }] }));
    assert_eq!(
        route.extra["xApiMcp"],
        json!([{ "description": "List users" }])
    );
    assert_eq!(route.description, "");
}

#[test]
fn oas_08_operations_that_are_not_objects() {
    // null throws in the reference.
    assert!(matches!(
        try_route(Value::Null, &keys()),
        Err(BuildError::NullOperation { .. })
    ));
    // Other values build a Route whose fields are all absent.
    let route = only_route(json!(5));
    assert!(route.extra.is_empty());
    let route = only_route(json!("ab"));
    assert_eq!(Value::Object(route.extra), json!({ "0": "a", "1": "b" }));
    let route = only_route(json!([{ "x-a": 1 }, 2]));
    assert_eq!(
        Value::Object(route.extra),
        json!({ "0": { "xA": 1 }, "1": 2 })
    );
    assert_eq!(route.description, "");
    // Non-string scalar fields are kept as their JSON text.
    let route = only_route(json!({ "operationId": 5, "description": false }));
    assert_eq!(route.operation_id.as_deref(), Some("5"));
    assert_eq!(route.description, "false");
}

// Deviation D3: Rust-only tests (the reference fails to load these documents).

#[test]
fn oas_17_path_item_keys_that_are_not_methods_are_skipped() {
    let doc = json!({
        "info": { "title": "t", "version": "1" },
        "paths": {
            "/health": {
                "parameters": [{ "name": "x", "in": "query" }],
                "summary": "Health",
                "description": "Path-level description",
                "servers": [{ "url": "http://x" }],
                "$ref": "#/components/pathItems/x",
                "trace": { "operationId": "trace" },
                "GET": { "operationId": "upper" },
                "get": { "operationId": "health" }
            }
        }
    });
    let document = build(&doc);
    assert_eq!(document.routes.len(), 1);
    assert_eq!(
        document.routes.routes(RouteMethod::Get)[0]
            .operation_id
            .as_deref(),
        Some("health")
    );
}

#[test]
fn oas_17_the_invalid_path_item_fixture_builds() {
    let doc: Value = serde_json::from_str(include_str!(
        "../../../../../conformance/fixtures/invalid-path-item.json"
    ))
    .unwrap();
    let document = build(&doc);
    assert!(!document.routes.is_empty());
}

#[test]
fn oas_23_a_document_without_info_still_loads() {
    let doc = json!({ "paths": { "/a": { "get": { "operationId": "a" } } } });
    let document = build(&doc);
    assert_eq!(document.title, None);
    assert_eq!(document.version, None);
    assert_eq!(document.routes.len(), 1);
    for info in [json!(null), json!("x"), json!({})] {
        let doc = json!({ "info": info, "paths": {} });
        assert!(build_service_document("u", &doc, &keys()).is_ok());
    }
}

#[test]
fn documents_without_path_items() {
    for doc in [
        json!({}),
        json!({ "paths": null }),
        json!({ "paths": "x" }),
        json!({ "paths": { "/a": null, "/b": "s", "/c": [1] } }),
    ] {
        assert!(build(&doc).routes.is_empty(), "{doc}");
    }
    // `paths` as a list: JavaScript iterates its indices.
    let document = build(&json!({ "paths": [{ "get": { "operationId": "zero" } }] }));
    assert_eq!(document.routes.routes(RouteMethod::Get)[0].path, "0");
    assert_eq!(
        build_service_document("u", &json!("text"), &keys()).unwrap_err(),
        BuildError::NotAnObject
    );
    assert_eq!(
        build_service_document("u", &json!([]), &keys()).unwrap_err(),
        BuildError::NotAnObject
    );
}

#[test]
fn users_service_fixture_builds_the_expected_routes() {
    let doc: Value = serde_json::from_str(include_str!(
        "../../../../../conformance/fixtures/users-service.json"
    ))
    .unwrap();
    let security = SecurityKeys {
        bearer: vec!["auth-user-id".into()],
        api_key: vec!["x-api-key".into()],
    };
    let document = build_service_document("u", &doc, &security).unwrap();
    let get = |url: &str| {
        document
            .routes
            .lookup("GET", url)
            .unwrap()
            .map(|r| (r.operation_id.clone(), r.path.clone()))
    };
    assert_eq!(
        get("/v1/users/me").unwrap().0.as_deref(),
        Some("UsersController_getUser")
    );
    assert_eq!(
        get("/v1/paths/files/a/b").unwrap().0.as_deref(),
        Some("PathsController_files")
    );
    assert_eq!(
        get("/v1/paths/anonymous/x").unwrap(),
        (None, "/v1/paths/anonymous/{slug}".to_string())
    );
    let stacked = document
        .routes
        .lookup("GET", "/v1/limits/stacked-status")
        .unwrap()
        .unwrap();
    assert_eq!(
        stacked.rate_limits,
        vec![
            RouteLimit {
                limit: 1,
                ttl: 3600.0,
                status: Some(404)
            },
            RouteLimit {
                limit: 2,
                ttl: 60.0,
                status: None
            }
        ]
    );
    let me = document
        .routes
        .routes(RouteMethod::Get)
        .iter()
        .find(|r| r.path == "/v1/users/me")
        .unwrap();
    assert!(me.is_bearer_auth && !me.is_api_key_auth);
    let replace = document
        .routes
        .lookup("PUT", "/v1/users/1")
        .unwrap()
        .unwrap();
    assert!(!replace.is_bearer_auth && replace.is_api_key_auth);
    let update = document
        .routes
        .lookup("PATCH", "/v1/users/1")
        .unwrap()
        .unwrap();
    assert!(update.is_bearer_auth && update.is_api_key_auth);
}
