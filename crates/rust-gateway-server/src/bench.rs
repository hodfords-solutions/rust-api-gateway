//! The benchmark middleware set (EX-05), selected with `GATEWAY_MIDDLEWARE=bench` next to
//! `GATEWAY_CONFIG`. `benches/reference/src/middleware.ts` implements the same rules for
//! the Reference Gateway; `benches/README.md` describes the benchmark.
//!
//! It stands for what production middleware is assumed to do (an assumption nobody has
//! confirmed yet):
//!
//! - **Proxy middleware**: a request with `Authorization: Bearer <token>` must carry an HS256
//!   JWT signed with `BENCH_JWT_SECRET`, unexpired and with a string `sub`; then
//!   `authUserId` (forwarded as `auth-user-id`) is set to `sub`. A bad token is rejected
//!   with 401 `{"message":"Unauthorized","statusCode":401}`, as Nest's
//!   `UnauthorizedException` renders. A request without a token is rejected the same way
//!   when its Route requires bearer authentication, and passes anonymously otherwise.
//! - **Identity resolver**: the user id, authenticated, when the middleware set one;
//!   otherwise the client address, not authenticated.
//! - **WebSocket middleware**: the same token check, without a Route.
//! - **Static requests** and **MCP authentication**: as the reference sample (EX-03).
//!
//! The verification options match `jsonwebtoken`'s `verify` in Node with
//! `{ algorithms: ['HS256'] }`: `exp` and `nbf` are checked only when present, with no
//! clock tolerance, and the audience and issuer are not checked.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{HeaderValue, Response, StatusCode, header};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use rust_gateway::Gateway;
use rust_gateway::error::JSON_UTF8;
use rust_gateway::handler::{
    BoxError, GatewayRequest, IdentityContext, IdentityResolver, MiddlewareError, ProxyMiddleware,
    WsProxyMiddleware, async_trait,
};
use rust_gateway::rate_limit::Identity;
use rust_gateway::request::ProxyRequest;
use rust_gateway::route::Route;
use serde::Deserialize;
use serde_json::json;

use crate::config::{ConfigError, Settings};
use crate::protocol::AuthUserId;
use crate::sample;

/// Whether `GATEWAY_MIDDLEWARE` selects this middleware set.
pub fn selected() -> bool {
    std::env::var("GATEWAY_MIDDLEWARE").as_deref() == Ok("bench")
}

/// The gateway with the benchmark middleware, and `GET /__bench/ready` answering once the
/// documents are loaded.
pub fn gateway(settings: &Settings) -> Result<Gateway, ConfigError> {
    let secret = std::env::var("BENCH_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or("GATEWAY_MIDDLEWARE=bench needs BENCH_JWT_SECRET")?;
    let verifier = Arc::new(Verifier::new(secret.as_bytes())?);
    let gateway = Gateway::builder(settings.options.clone())
        .proxy_middleware(JwtMiddleware(Arc::clone(&verifier)))
        .websocket_middleware(JwtWsMiddleware(verifier))
        .static_request(sample::SampleStaticRequest)
        .mcp_authentication(sample::SampleMcpAuthentication)
        .identity_resolver(UserOrAddress)
        .build()?;
    let documents = Arc::clone(gateway.documents());
    let routes = Router::new().route(
        "/__bench/ready",
        get(move || {
            let documents = Arc::clone(&documents);
            async move {
                documents.ready().await;
                let body = json!({ "services": documents.loaded_services() }).to_string();
                let mut response = Response::new(Body::from(body));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_UTF8));
                response
            }
        }),
    );
    Ok(gateway.with_routes(routes))
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<serde_json::Value>,
}

struct Verifier {
    key: DecodingKey,
    validation: Validation,
}

impl Verifier {
    fn new(secret: &[u8]) -> Result<Self, ConfigError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.required_spec_claims = HashSet::new();
        validation.leeway = 0;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = false;
        Ok(Self {
            key: DecodingKey::from_secret(secret)?,
            validation,
        })
    }

    /// The token's `sub` when the token is valid and `sub` is a non-empty string.
    fn subject(&self, token: &str) -> Option<String> {
        let data = jsonwebtoken::decode::<Claims>(token, &self.key, &self.validation).ok()?;
        match data.claims.sub {
            Some(serde_json::Value::String(sub)) if !sub.is_empty() => Some(sub),
            _ => None,
        }
    }
}

fn unauthorized() -> MiddlewareError {
    MiddlewareError::json(
        StatusCode::UNAUTHORIZED,
        json!({ "message": "Unauthorized", "statusCode": 401 }),
    )
}

/// The token of an `Authorization: Bearer <token>` header.
fn bearer(request: &GatewayRequest) -> Option<&str> {
    request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Shared by the proxy and WebSocket middleware: `Ok(true)` to continue, 401 otherwise.
fn authenticate(
    verifier: &Verifier,
    requires_bearer: bool,
    request: &mut GatewayRequest,
    proxy_request: &mut ProxyRequest,
) -> Result<bool, MiddlewareError> {
    match bearer(request) {
        Some(token) => {
            let user = verifier.subject(token).ok_or_else(unauthorized)?;
            proxy_request.add_header("authUserId", user.clone());
            request.extensions_mut().insert(AuthUserId(user));
            Ok(true)
        }
        None if requires_bearer => Err(unauthorized()),
        None => Ok(true),
    }
}

struct JwtMiddleware(Arc<Verifier>);

#[async_trait]
impl ProxyMiddleware for JwtMiddleware {
    async fn handle(
        &self,
        route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        authenticate(&self.0, route.is_bearer_auth, request, proxy_request)
    }
}

struct JwtWsMiddleware(Arc<Verifier>);

#[async_trait]
impl WsProxyMiddleware for JwtWsMiddleware {
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        authenticate(&self.0, false, request, proxy_request)
    }
}

/// The user id when the middleware authenticated the request, else the client address.
struct UserOrAddress;

#[async_trait]
impl IdentityResolver for UserOrAddress {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        Ok(Some(
            match context.request.extensions().get::<AuthUserId>() {
                Some(AuthUserId(user)) => Identity::authenticated(user.clone()),
                None => Identity::anonymous(context.ip.to_string()),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, Uri};
    use jsonwebtoken::{EncodingKey, Header};
    use rust_gateway::rate_limit::Identity;

    const SECRET: &[u8] = b"bench-secret";

    fn token(claims: serde_json::Value, secret: &[u8]) -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret).unwrap(),
        )
        .unwrap()
    }

    fn request(authorization: Option<&str>) -> GatewayRequest {
        let mut headers = HeaderMap::new();
        if let Some(value) = authorization {
            headers.insert("authorization", value.parse().unwrap());
        }
        GatewayRequest::new(
            Method::GET,
            Uri::from_static("/svc/v1/orders/42"),
            "/svc/v1/orders/42",
            headers,
            None,
        )
    }

    fn run(
        authorization: Option<&str>,
        requires_bearer: bool,
    ) -> (Result<bool, u16>, ProxyRequest, GatewayRequest) {
        let verifier = Verifier::new(SECRET).unwrap();
        let mut request = request(authorization);
        let mut proxy_request = ProxyRequest::new();
        let result = authenticate(&verifier, requires_bearer, &mut request, &mut proxy_request)
            .map_err(|error| match error {
                MiddlewareError::Http { status, .. } => status.as_u16(),
                MiddlewareError::Other(_) => 500,
            });
        (result, proxy_request, request)
    }

    // EX-05: a valid token sets `auth-user-id` to its subject.
    #[test]
    fn ex_05_valid_token_sets_the_user() {
        let exp = 4_000_000_000u64;
        let header = format!(
            "Bearer {}",
            token(json!({"sub": "user-42", "exp": exp}), SECRET)
        );
        let (result, proxy_request, request) = run(Some(&header), true);
        assert_eq!(result, Ok(true));
        assert_eq!(
            proxy_request.kebab_headers(),
            vec![("auth-user-id".to_string(), "user-42".to_string())]
        );
        assert!(
            matches!(request.extensions().get::<AuthUserId>(), Some(AuthUserId(u)) if u == "user-42")
        );
    }

    // EX-05: a token signed with another secret, an expired token and a token without a
    // string subject are rejected with 401.
    #[test]
    fn ex_05_bad_tokens_are_rejected() {
        for header in [
            format!(
                "Bearer {}",
                token(json!({"sub": "user-42"}), b"other-secret")
            ),
            format!(
                "Bearer {}",
                token(json!({"sub": "user-42", "exp": 1}), SECRET)
            ),
            format!("Bearer {}", token(json!({"sub": 42}), SECRET)),
            "Bearer not-a-jwt".to_string(),
        ] {
            let (result, proxy_request, _) = run(Some(&header), false);
            assert_eq!(result, Err(401), "{header}");
            assert!(proxy_request.is_empty());
        }
    }

    // EX-05: without a token, only Routes that require bearer authentication reject.
    #[test]
    fn ex_05_missing_token_depends_on_the_route() {
        assert_eq!(run(None, true).0, Err(401));
        assert_eq!(run(None, false).0, Ok(true));
        // Not a bearer scheme: treated as no token.
        assert_eq!(run(Some("Basic dXNlcjpwYXNz"), true).0, Err(401));
    }

    // EX-05: the Identity is the user id when authenticated, else the client address.
    #[tokio::test]
    async fn ex_05_identity_is_user_or_address() {
        let route_document = json!({"paths": {"/v1/orders/{id}": {"get": {}}}});
        let built = rust_gateway::openapi::build_service_document(
            "http://svc/openapi.json",
            &route_document,
            &Default::default(),
        )
        .unwrap();
        let route = built.routes.iter().next().unwrap().clone();

        let mut request = request(None);
        let anonymous = UserOrAddress
            .resolve(IdentityContext {
                request: &request,
                route: &route,
                ip: "172.18.0.5",
            })
            .await
            .unwrap();
        assert_eq!(anonymous, Some(Identity::anonymous("172.18.0.5")));

        request
            .extensions_mut()
            .insert(AuthUserId("user-42".into()));
        let user = UserOrAddress
            .resolve(IdentityContext {
                request: &request,
                route: &route,
                ip: "172.18.0.5",
            })
            .await
            .unwrap();
        assert_eq!(user, Some(Identity::authenticated("user-42")));
    }
}
