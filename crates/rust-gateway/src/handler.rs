//! The handlers an application registers on the gateway (CFG-19, API-06 to API-09, API-15).
//!
//! The Reference Gateway discovers its handlers through NestJS decorators. Here they are
//! registered explicitly on the [`GatewayBuilder`](crate::gateway::GatewayBuilder), each with
//! a priority (default [`DEFAULT_PRIORITY`]). Handlers of one kind run in ascending
//! priority; equal priorities keep registration order.
//!
//! | Reference | Trait |
//! |---|---|
//! | `@ProxyMiddleware` + `ProxyMiddlewareHandler` | [`ProxyMiddleware`] |
//! | `@WsProxyMiddleware` + `WsProxyMiddlewareHandler` | [`WsProxyMiddleware`] |
//! | `@ProxyValidation` + `ProxyValidationHandler` | [`StaticRequestPredicate`] |
//! | `@McpAuthenticationMiddleware` + `McpAuthenticationMiddlewareHandler` | [`McpAuthentication`] |
//! | `ThrottlerOption.keyResolver` | [`IdentityResolver`] |
//!
//! Every trait is object-safe and asynchronous (written with [`async_trait`]). Handlers see
//! the request as a [`GatewayRequest`]: the sanitised request (normalised URL, `excludeHeaders`
//! removed) plus an [`Extensions`] map through which a middleware attaches typed values that
//! later middleware and the Identity resolver read (REQ-20), the counterpart of the fields
//! the reference's middleware set on the Express request object.

use std::error::Error as StdError;
use std::net::SocketAddr;
use std::sync::Arc;

pub use async_trait::async_trait;
use bytes::Bytes;
use http::{Extensions, HeaderMap, Method, Response, StatusCode, Uri, Version};

use crate::error::{GatewayError, MiddlewareBody};
use crate::rate_limit::Identity;
use crate::request::ProxyRequest;
use crate::route::Route;

/// A boxed error, for failures that are not HTTP errors.
pub type BoxError = Box<dyn StdError + Send + Sync>;

/// The priority a handler gets when none is given, as in the reference's decorators.
pub const DEFAULT_PRIORITY: i32 = 1;

/// A request as handlers see it.
///
/// The URL is already normalised (REQ-02) and the `excludeHeaders` are already removed from
/// the headers (REQ-03). Handlers do not see the body of a proxied request: it is streamed to
/// the upstream untouched (FWD-13). For an MCP tool call, [`body`](Self::body) holds the tool
/// arguments (MCP-16).
#[derive(Debug)]
pub struct GatewayRequest {
    method: Method,
    uri: Uri,
    version: Version,
    url: String,
    headers: HeaderMap,
    peer: Option<SocketAddr>,
    body: Option<serde_json::Value>,
    extensions: Extensions,
}

impl GatewayRequest {
    /// A request view. The server builds these; tests of handlers can too.
    ///
    /// `uri` is the request target as received, `url` its normalised form
    /// ([`normalize_request_url`](crate::request::normalize_request_url)), and `peer` the
    /// client connection's address.
    pub fn new(
        method: Method,
        uri: Uri,
        url: impl Into<String>,
        headers: HeaderMap,
        peer: Option<SocketAddr>,
    ) -> Self {
        Self {
            method,
            uri,
            version: Version::HTTP_11,
            url: url.into(),
            headers,
            peer,
            body: None,
            extensions: Extensions::new(),
        }
    }

    pub(crate) fn with_version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }

    /// This request with a parsed body (the tool arguments of an MCP tool call, MCP-16).
    pub fn with_body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    /// This request with `extensions` as its attached values, replacing any.
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// The attached values, taken out of the request (for handing them to the next stage).
    pub fn into_extensions(self) -> Extensions {
        self.extensions
    }

    /// The request method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The request target as the client sent it.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// The HTTP version of the request.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The normalised URL, path and query (the reference's `request.url` after
    /// sanitising), for example `/svc/v1/users?a=1`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The request headers, without the `excludeHeaders`.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The first value of a header as a string, if it is valid visible ASCII.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The client connection's address, when the server knows it.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// The request body as the reference's middleware sees `request.body`: the tool
    /// arguments for an MCP tool call (MCP-16), `None` for a proxied request, whose body is
    /// streamed and never parsed.
    pub fn body(&self) -> Option<&serde_json::Value> {
        self.body.as_ref()
    }

    /// Values attached to the request by handlers (REQ-20).
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Values attached to the request by handlers, mutably (REQ-20).
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}

/// How a middleware failed (REQ-19, REQ-24).
#[derive(Debug, thiserror::Error)]
pub enum MiddlewareError {
    /// An HTTP error with its own status and body, returned to the client (REQ-19): the
    /// counterpart of throwing an `HttpException`.
    #[error("middleware failed with status {status}")]
    Http {
        /// The response status.
        status: StatusCode,
        /// The response body.
        body: MiddlewareBody,
    },
    /// Any other failure: the client gets 500 (REQ-24).
    #[error(transparent)]
    Other(BoxError),
}

impl MiddlewareError {
    /// An HTTP error whose body is a JSON value.
    pub fn json(status: StatusCode, body: serde_json::Value) -> Self {
        MiddlewareError::Http {
            status,
            body: MiddlewareBody::Json(body),
        }
    }

    /// An HTTP error whose body is a string.
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        MiddlewareError::Http {
            status,
            body: MiddlewareBody::Text(body.into()),
        }
    }

    /// A failure that is not an HTTP error.
    pub fn other(error: impl Into<BoxError>) -> Self {
        MiddlewareError::Other(error.into())
    }

    pub(crate) fn into_gateway_error(self) -> GatewayError {
        match self {
            MiddlewareError::Http { status, body } => GatewayError::Middleware { status, body },
            MiddlewareError::Other(source) => GatewayError::Internal {
                source: Some(source),
            },
        }
    }
}

/// A proxy middleware (API-06): runs for every Routed Request after its Route was found and
/// the Global IP Bucket counted it (REQ-16, REQ-17).
///
/// It returns `Ok(true)` to let the request continue, `Ok(false)` to reject it with 403
/// (REQ-18), or an error (REQ-19, REQ-24). It may add headers to `proxy_request`, which are
/// forwarded with kebab-cased names (REQ-21), and attach values to the request (REQ-20).
#[async_trait]
pub trait ProxyMiddleware: Send + Sync + 'static {
    /// Handles one request.
    async fn handle(
        &self,
        route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError>;
}

/// A WebSocket middleware (API-07): runs for every WebSocket upgrade (area 06, WS-04).
///
/// Same contract as [`ProxyMiddleware`], without a Route.
#[async_trait]
pub trait WsProxyMiddleware: Send + Sync + 'static {
    /// Handles one upgrade request.
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError>;
}

/// A static-request predicate (API-08): a request is a Static Request when any registered
/// predicate returns true for it (REQ-09). An error answers 500.
#[async_trait]
pub trait StaticRequestPredicate: Send + Sync + 'static {
    /// Whether the request is a Static Request.
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError>;
}

/// What an MCP authentication handler decided (area 08, MCP-04).
#[derive(Debug)]
pub enum McpAuthDecision {
    /// The request may call MCP tools.
    Allow,
    /// The gateway answers 401 `{"error":"Unauthorized"}`.
    Deny,
    /// The handler answers the request itself with this response (the reference's handler
    /// writes to the response object and returns false).
    Respond(Response<Bytes>),
}

/// An MCP authentication handler (API-09), run for `POST /mcp` (area 08).
///
/// Handlers run in ascending priority before the MCP protocol reads the request (MCP-04).
/// The request is the `/mcp` request as received: its URL normalised, its headers complete
/// (the `excludeHeaders` are removed later, before the proxy middleware runs). Values a
/// handler attaches to the request are visible to the Identity resolver and the proxy
/// middleware of the tool calls it makes (REQ-20). An [`MiddlewareError::Http`] is answered
/// with its status and body, any other error with 500.
#[async_trait]
pub trait McpAuthentication: Send + Sync + 'static {
    /// Authenticates one MCP request.
    async fn authenticate(
        &self,
        request: &mut GatewayRequest,
    ) -> Result<McpAuthDecision, MiddlewareError>;
}

/// What an Identity resolver receives (the reference's `ThrottlerKeyResolverContext`).
#[derive(Debug, Clone, Copy)]
pub struct IdentityContext<'a> {
    /// The request, including what middleware attached to it (RL-13).
    pub request: &'a GatewayRequest,
    /// The matched Route.
    pub route: &'a Route,
    /// The normalised client address (RL-06, RL-07).
    pub ip: &'a str,
}

/// The Identity resolver (API-15): decides the key a request's Custom and Route Buckets are
/// counted against, and whether it is authenticated (RL-11, RL-12).
///
/// `Ok(None)`, or an Identity whose key is absent, empty or whitespace, means "no key". An
/// error answers 500 (REQ-24).
#[async_trait]
pub trait IdentityResolver: Send + Sync + 'static {
    /// Resolves the Identity of one request.
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError>;
}

/// Handlers of one kind, kept in run order: ascending priority, then registration order.
pub(crate) struct Registry<T: ?Sized> {
    entries: Vec<(i32, Arc<T>)>,
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T: ?Sized> Clone for Registry<T> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<T: ?Sized> Registry<T> {
    /// Adds a handler. Inserting after every entry of lower or equal priority keeps the list
    /// sorted and stable.
    pub(crate) fn register(&mut self, priority: i32, handler: Arc<T>) {
        let at = self
            .entries
            .iter()
            .position(|(p, _)| *p > priority)
            .unwrap_or(self.entries.len());
        self.entries.insert(at, (priority, handler));
    }

    /// The handlers in run order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<T>> {
        self.entries.iter().map(|(_, handler)| handler)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl<T: ?Sized> std::fmt::Debug for Registry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "priorities",
                &self.entries.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CFG-19: ascending priority, ties in registration order.
    #[test]
    fn cfg_19_handlers_run_in_ascending_priority_then_registration_order() {
        let mut registry: Registry<str> = Registry::default();
        for (priority, name) in [
            (1, "a"),
            (0, "b"),
            (5, "c"),
            (1, "d"),
            (-3, "e"),
            (5, "f"),
            (1, "g"),
        ] {
            registry.register(priority, Arc::from(name));
        }
        let order: Vec<&str> = registry.iter().map(|s| &**s).collect();
        assert_eq!(order, ["e", "b", "a", "d", "g", "c", "f"]);
        assert_eq!(registry.len(), 7);
    }

    // REQ-20: values attached by one handler are visible to the next.
    #[test]
    fn req_20_extensions_carry_values_between_handlers() {
        #[derive(Debug, Clone, PartialEq)]
        struct AuthUserId(String);
        let mut request = GatewayRequest::new(
            Method::GET,
            Uri::from_static("/svc/x"),
            "/svc/x",
            HeaderMap::new(),
            None,
        );
        request
            .extensions_mut()
            .insert(AuthUserId("42".to_string()));
        assert_eq!(
            request.extensions().get::<AuthUserId>(),
            Some(&AuthUserId("42".to_string()))
        );
    }
}
