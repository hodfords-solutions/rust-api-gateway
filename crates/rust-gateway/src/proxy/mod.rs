//! The proxy: the request pipeline (area 03), rate limiting in it (area 05) and forwarding
//! (area 04). It is the router's fallback, so it handles every request that no application
//! route or gateway endpoint claims (REQ-01).
//!
//! Order, as the Reference Gateway runs it (REQ-16; Express's own checks of REQ-26 and the
//! asterisk-form target run before, in [`crate::server`]):
//!
//! 1. Normalise the URL (REQ-02) and remove the `excludeHeaders` (REQ-03).
//! 2. Ask the static predicates (REQ-09), classify (REQ-12), select the Upstream Service for
//!    every class (REQ-04 to REQ-07, REQ-11).
//! 3. Routed Requests only: the supported method (REQ-15, D4), the loaded document (REQ-08,
//!    D4), the Route (REQ-14), the Global IP Bucket (RL-09), the middleware chain (REQ-17 to
//!    REQ-19, with the Anonymous IP Bucket of RL-10 on rejection), the Identity (RL-11 to
//!    RL-13), the remaining Buckets (RL-18).
//! 4. Forward (FWD-01 to FWD-23), counting status-scoped Route Buckets once the response has
//!    been sent (RL-16).

mod body;
mod forward;
pub(crate) mod pool;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Request, Response};

use crate::error::GatewayError;
use crate::gateway::Shared;
use crate::handler::{GatewayRequest, IdentityContext};
use crate::rate_limit::{RateLimitError, RouteKey, has_status_limits};
use crate::request::{
    ProxyRequest, RequestClass, classify, is_supported_method, normalize_request_url,
    strip_service_prefix,
};

pub(crate) use forward::{render_error, weak_etag};

/// Handles one request that reached the proxy.
pub(crate) async fn handle(shared: &Arc<Shared>, request: Request<Body>) -> Response<Body> {
    match run(shared, request).await {
        Ok(response) => response,
        Err(error) => {
            if let GatewayError::Internal {
                source: Some(source),
            } = &error
            {
                tracing::error!(error = %source, "request failed");
            }
            render_error(shared, &error)
        }
    }
}

/// Whether a request asks to switch to WebSocket (area 06).
fn is_upgrade_request(request: &Request<Body>) -> bool {
    request.headers().contains_key(http::header::UPGRADE)
        && request
            .headers()
            .get_all(http::header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

async fn run(shared: &Arc<Shared>, request: Request<Body>) -> Result<Response<Body>, GatewayError> {
    if is_upgrade_request(&request) {
        return websocket_upgrade(shared, request).await;
    }
    let (parts, body) = request.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let target = parts.uri.to_string();
    let url = normalize_request_url(&target).map_err(GatewayError::internal)?;

    // REQ-03: the excluded headers, removed without disturbing the order of the others.
    let exclude = &shared.options.exclude_headers;
    let lines: Vec<(HeaderName, HeaderValue)> = parts
        .headers
        .iter()
        .filter(|(name, _)| {
            !exclude
                .iter()
                .any(|excluded| name.as_str().eq_ignore_ascii_case(excluded))
        })
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let mut headers = HeaderMap::with_capacity(lines.len());
    for (name, value) in &lines {
        headers.append(name.clone(), value.clone());
    }
    let mut request =
        GatewayRequest::new(parts.method.clone(), parts.uri, url.clone(), headers, peer)
            .with_version(parts.version);

    // REQ-09, REQ-12.
    let mut is_static = false;
    for predicate in shared.handlers.static_request.iter() {
        if predicate
            .is_static(&request)
            .await
            .map_err(|e| GatewayError::Internal { source: Some(e) })?
        {
            is_static = true;
            break;
        }
    }
    let class = classify(is_static, &url, &shared.options.bypass_route_prefixes);

    // REQ-04 to REQ-07, REQ-11.
    let store = &shared.store;
    let selected = shared
        .selector
        .select(&url, |prefix| store.has_document(prefix))
        .into_result()?;
    let prefix = selected.prefix.to_string();

    let mut proxy_request = ProxyRequest::new();
    let mut after_response = None;
    if class == RequestClass::Routed {
        let method = parts.method.clone();
        // REQ-15 (Deviation D4), REQ-08 (Deviation D4), REQ-14.
        if !is_supported_method(&method) {
            return Err(GatewayError::UnsupportedMethod {
                method: method.to_string(),
            });
        }
        selected.require_document(|prefix| store.has_document(prefix))?;
        let route = store
            .route(
                &prefix,
                method.as_str(),
                strip_service_prefix(&prefix, &url),
            )
            .map_err(GatewayError::internal)?
            .ok_or(GatewayError::MethodNotAllowed)?;

        let limiter = &shared.limiter;
        let mut state = limiter.request_state(peer.map(|p| p.ip()), request.headers());
        limiter
            .count_global_ip(&mut state)
            .await
            .map_err(rate_limit_error)?;

        // REQ-17: excluded names cannot come from anywhere but a middleware.
        for name in exclude {
            proxy_request.remove_header(name);
        }
        for middleware in shared.handlers.proxy.iter() {
            let rejection = match middleware
                .handle(&route, &mut request, &mut proxy_request)
                .await
            {
                Ok(true) => continue,
                Ok(false) => GatewayError::Forbidden,
                Err(error) => error.into_gateway_error(),
            };
            // RL-10: a rejected request counts against the Anonymous IP Bucket first.
            limiter
                .count_anonymous_ip(&mut state)
                .await
                .map_err(rate_limit_error)?;
            return Err(rejection);
        }

        // RL-11 to RL-13: the resolver runs after middleware, only when limiting is on.
        let identity = if limiter.is_enabled() {
            match &shared.handlers.identity_resolver {
                Some(resolver) => resolver
                    .resolve(IdentityContext {
                        request: &request,
                        route: &route,
                        ip: state.client_key(),
                    })
                    .await
                    .map_err(|e| GatewayError::Internal { source: Some(e) })?,
                None => Some(state.default_identity()),
            }
        } else {
            None
        };
        limiter
            .check_limits(
                &mut state,
                identity,
                RouteKey::new(&prefix, method.as_str(), &route.router_path),
                &route.rate_limits,
            )
            .await
            .map_err(rate_limit_error)?;
        if has_status_limits(&route.rate_limits) {
            after_response = Some(forward::AfterResponse {
                limiter: limiter.clone(),
                state,
                service: prefix.clone(),
                method: method.as_str().to_string(),
                route: Arc::clone(&route),
            });
        }
    }

    let pool = shared
        .pools
        .get(&prefix)
        .ok_or(GatewayError::NoUpstreamService)?;
    let middleware_headers = if class == RequestClass::Routed {
        proxy_request.kebab_headers()
    } else {
        Vec::new()
    };
    forward::forward(
        shared,
        pool,
        forward::Outgoing {
            method: parts.method,
            prefix: &prefix,
            url: &url,
            class,
            client_headers: &lines,
            peer,
            middleware_headers,
        },
        body,
        after_response,
    )
    .await
}

/// A rejection or failure of a rate-limit operation: 429 with the Bucket's body, or 500
/// (REQ-24, Deviation D10).
fn rate_limit_error(error: RateLimitError) -> GatewayError {
    match error {
        RateLimitError::TooManyRequests(rejection) => GatewayError::RateLimited {
            body: serde_json::from_str(&rejection.body())
                .unwrap_or_else(|_| serde_json::Value::String(rejection.message().to_string())),
        },
        other => GatewayError::internal(other),
    }
}

/// WebSocket upgrades (area 06) are not handled yet. The Reference Gateway tunnels them
/// (WS-01 onwards); until that is ported, an upgrade request is answered with 501 so it is
/// never forwarded as a plain request with its upgrade headers stripped.
async fn websocket_upgrade(
    shared: &Arc<Shared>,
    _request: Request<Body>,
) -> Result<Response<Body>, GatewayError> {
    let _ = shared;
    let mut response = Response::new(Body::from(
        r#"{"message":"WebSocket upgrades are not supported yet","statusCode":501}"#,
    ));
    *response.status_mut() = http::StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(crate::error::JSON_UTF8),
    );
    Ok(response)
}
