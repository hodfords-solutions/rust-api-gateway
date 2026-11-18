//! The public entry point (API-01, area 01): options, the builder, and the [`Gateway`] an
//! application mounts.
//!
//! ```no_run
//! use api_gateway::gateway::{Gateway, GatewayOptions};
//! use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
//! use api_gateway::upstream::UpstreamService;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let options = GatewayOptions::new(
//!     vec![UpstreamService::new(
//!         "svc",
//!         "http://127.0.0.1:4020/documents-json",
//!         "http://127.0.0.1:4020",
//!     )],
//!     RateLimitOptions::new(true, BucketLimit::new(60, 60.0), BucketLimit::new(60, 60.0)),
//!     RedisOptions::new("127.0.0.1", 6379),
//! );
//! let gateway = Gateway::builder(options).build()?;
//! let listener = api_gateway::server::bind_dual_stack(3000)?;
//! gateway.serve(listener, std::future::pending()).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::Route;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::{Layer, Service};

use crate::documents::{self, DocumentEndpoints};
use crate::endpoints::Endpoints;
use crate::error::{DefaultErrorRenderer, ErrorRenderer};
use crate::handler::{
    DEFAULT_PRIORITY, IdentityResolver, McpAuthentication, ProxyMiddleware, Registry,
    StaticRequestPredicate, WsProxyMiddleware,
};
use crate::openapi::{
    DocumentFetcher, DocumentLoader, DocumentStore, FetchError, HttpDocumentFetcher, LoaderHandle,
    SecurityKeys,
};
use crate::proxy::pool::{Origin, UpstreamPool};
use crate::rate_limit::{RateLimitOptions, RateLimiter, RateLimiterConfigError, RedisOptions};
use crate::request::ServiceSelector;
use crate::server::ServerTimeouts;
use crate::upstream::UpstreamService;

/// Upstream connection-pool options, applied to every Upstream Service (CFG-10).
///
/// The reference passes its `pool` option to undici's `Pool` unchanged. The fields here are
/// the ones with an observable effect; the defaults are undici's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolOptions {
    /// `connections`: the most connections open to one Upstream Service at a time. Further
    /// requests wait for a connection. `None` means no limit.
    pub connections: Option<usize>,
    /// `keepAliveTimeout`: a connection idle for this long is not reused. Default 4 s.
    pub keep_alive_timeout: Duration,
    /// `headersTimeout`: how long to wait for the response head; when it passes, the client
    /// gets the FWD-22 503. Default 300 s; `None` waits forever.
    pub headers_timeout: Option<Duration>,
    /// `bodyTimeout`: how long the response body may stay silent; when it passes, the
    /// response is cut short (FWD-23). Default 300 s; `None` waits forever.
    pub body_timeout: Option<Duration>,
    /// `connectTimeout`: how long connecting may take. Default 10 s; `None` waits forever.
    pub connect_timeout: Option<Duration>,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            connections: None,
            keep_alive_timeout: Duration::from_secs(4),
            headers_timeout: Some(Duration::from_secs(300)),
            body_timeout: Some(Duration::from_secs(300)),
            connect_timeout: Some(Duration::from_secs(10)),
        }
    }
}

impl PoolOptions {
    /// Reads undici `Pool` options as the reference's `pool` option holds them:
    /// `connections`, `keepAliveTimeout`, `headersTimeout`, `bodyTimeout` (milliseconds; 0
    /// disables the two timeouts, as in undici) and `connectTimeout`. Other fields
    /// (`pipelining`, ...) have no equivalent and are ignored.
    pub fn from_undici(value: &Value) -> Self {
        let mut options = PoolOptions::default();
        let millis = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_f64)
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        };
        if let Some(connections) = value.get("connections").and_then(Value::as_f64)
            && connections.is_finite()
            && connections >= 1.0
        {
            options.connections = Some(connections as usize);
        }
        if let Some(timeout) = millis("keepAliveTimeout") {
            options.keep_alive_timeout = timeout;
        }
        for (key, slot) in [
            ("headersTimeout", &mut options.headers_timeout),
            ("bodyTimeout", &mut options.body_timeout),
            ("connectTimeout", &mut options.connect_timeout),
        ] {
            if let Some(timeout) = millis(key) {
                *slot = (!timeout.is_zero()).then_some(timeout);
            }
        }
        options
    }
}

/// Document endpoint options (CFG-08): the reference's `restful` option. Used by the
/// document endpoints (area 07).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentOptions {
    /// `isEnableDocument`: whether `/documents` and `/document-json` exist (CFG-14).
    pub enabled: bool,
    /// `hideDocumentIds`: operation ids left out of the served documents (DOC-10).
    pub hide_document_ids: Vec<String>,
}

/// The gateway's options: the reference's `ApiGatewayOption` (API-01, CFG-01 to CFG-14).
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    /// `apiServices` (CFG-01).
    pub api_services: Vec<UpstreamService>,
    /// `excludeHeaders` (CFG-02): gateway-owned header names removed from every request.
    pub exclude_headers: Vec<String>,
    /// `openApiSecurityKeys` (CFG-03): security-scheme names meaning bearer authentication.
    pub open_api_security_keys: Vec<String>,
    /// `openApiSecurityApiKeys` (CFG-04): security-scheme names meaning API-key
    /// authentication.
    pub open_api_security_api_keys: Vec<String>,
    /// `throttler` (CFG-05 to CFG-07), plus the trusted proxies of RL-06.
    pub rate_limit: RateLimitOptions,
    /// `redis` (CFG-09).
    pub redis: RedisOptions,
    /// `pool` (CFG-10).
    pub pool: PoolOptions,
    /// `bypassRoutePrefixes` (CFG-12).
    pub bypass_route_prefixes: Vec<String>,
    /// `restful` (CFG-08, CFG-14).
    pub documents: DocumentOptions,
    /// `scalarOptions` (CFG-11), passed to the document page.
    pub scalar_options: Option<Value>,
    /// `mcp` (CFG-13): the MCP server, with its callbacks. `POST /mcp` exists only when
    /// this is present and [`McpOptions::enabled`](crate::mcp::McpOptions::enabled).
    #[cfg(feature = "mcp")]
    pub mcp: Option<crate::mcp::McpOptions>,
    /// The client connection timeouts of [`Gateway::serve`] (CFG-20, REQ-28, REQ-29): the
    /// `http.Server` settings the reference's host application runs with. Node's defaults.
    pub server_timeouts: ServerTimeouts,
}

impl GatewayOptions {
    /// The required options; everything else empty or default.
    pub fn new(
        api_services: Vec<UpstreamService>,
        rate_limit: RateLimitOptions,
        redis: RedisOptions,
    ) -> Self {
        Self {
            api_services,
            exclude_headers: Vec::new(),
            open_api_security_keys: Vec::new(),
            open_api_security_api_keys: Vec::new(),
            rate_limit,
            redis,
            pool: PoolOptions::default(),
            bypass_route_prefixes: Vec::new(),
            documents: DocumentOptions::default(),
            scalar_options: None,
            #[cfg(feature = "mcp")]
            mcp: None,
            server_timeouts: ServerTimeouts::default(),
        }
    }
}

/// Why a gateway could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// Invalid rate-limit or Redis options (CFG-06, CFG-07, CFG-09).
    #[error(transparent)]
    RateLimit(#[from] RateLimiterConfigError),
    /// An Upstream Service's `host` is not an `http` or `https` origin.
    #[error("{0}")]
    UpstreamOrigin(String),
    /// The default document fetcher could not be created (it never depends on the host's
    /// trust store).
    #[error(transparent)]
    Fetcher(#[from] FetchError),
    /// [`GatewayBuilder::build`] was called outside a Tokio runtime.
    #[error("the gateway must be built inside a Tokio runtime")]
    NoRuntime,
}

/// The registered handlers (CFG-19).
#[derive(Clone, Default)]
pub struct Handlers {
    pub(crate) proxy: Registry<dyn ProxyMiddleware>,
    pub(crate) websocket: Registry<dyn WsProxyMiddleware>,
    pub(crate) static_request: Registry<dyn StaticRequestPredicate>,
    pub(crate) mcp_authentication: Registry<dyn McpAuthentication>,
    pub(crate) identity_resolver: Option<Arc<dyn IdentityResolver>>,
}

impl std::fmt::Debug for Handlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handlers")
            .field("proxy", &self.proxy.len())
            .field("websocket", &self.websocket.len())
            .field("static_request", &self.static_request.len())
            .field("mcp_authentication", &self.mcp_authentication.len())
            .field("identity_resolver", &self.identity_resolver.is_some())
            .finish()
    }
}

impl Handlers {
    /// WebSocket middleware in run order (area 06).
    pub fn websocket_middleware(&self) -> impl Iterator<Item = &Arc<dyn WsProxyMiddleware>> {
        self.websocket.iter()
    }

    /// MCP authentication handlers in run order (area 08).
    pub fn mcp_authentication(&self) -> impl Iterator<Item = &Arc<dyn McpAuthentication>> {
        self.mcp_authentication.iter()
    }

    /// Proxy middleware in run order.
    pub fn proxy_middleware(&self) -> impl Iterator<Item = &Arc<dyn ProxyMiddleware>> {
        self.proxy.iter()
    }

    /// The Identity resolver, if one is registered.
    pub fn identity_resolver(&self) -> Option<&Arc<dyn IdentityResolver>> {
        self.identity_resolver.as_ref()
    }
}

/// Applies one application layer to the gateway's router (REQ-27).
type ApplyLayer = Arc<dyn Fn(Router) -> Router + Send + Sync>;

/// Builds a [`Gateway`] (API-01).
pub struct GatewayBuilder {
    options: GatewayOptions,
    handlers: Handlers,
    renderer: Arc<dyn ErrorRenderer>,
    routes: Router,
    layers: Vec<ApplyLayer>,
    fetcher: Option<Arc<dyn DocumentFetcher>>,
    x_powered_by: bool,
    upstream_tls: Option<Arc<rustls::ClientConfig>>,
}

impl std::fmt::Debug for GatewayBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayBuilder")
            .field("options", &self.options)
            .field("handlers", &self.handlers)
            .field("x_powered_by", &self.x_powered_by)
            .field("layers", &self.layers.len())
            .finish_non_exhaustive()
    }
}

impl GatewayBuilder {
    /// A builder with no handlers.
    pub fn new(options: GatewayOptions) -> Self {
        Self {
            options,
            handlers: Handlers::default(),
            renderer: Arc::new(DefaultErrorRenderer),
            routes: Router::new(),
            layers: Vec::new(),
            fetcher: None,
            x_powered_by: false,
            upstream_tls: None,
        }
    }

    /// Registers a proxy middleware with the default priority (API-06).
    pub fn proxy_middleware(self, middleware: impl ProxyMiddleware) -> Self {
        self.proxy_middleware_with_priority(DEFAULT_PRIORITY, middleware)
    }

    /// Registers a proxy middleware with a priority; lower runs first (CFG-19).
    pub fn proxy_middleware_with_priority(
        mut self,
        priority: i32,
        middleware: impl ProxyMiddleware,
    ) -> Self {
        self.handlers.proxy.register(priority, Arc::new(middleware));
        self
    }

    /// Registers a WebSocket middleware with the default priority (API-07).
    pub fn websocket_middleware(self, middleware: impl WsProxyMiddleware) -> Self {
        self.websocket_middleware_with_priority(DEFAULT_PRIORITY, middleware)
    }

    /// Registers a WebSocket middleware with a priority (CFG-19).
    pub fn websocket_middleware_with_priority(
        mut self,
        priority: i32,
        middleware: impl WsProxyMiddleware,
    ) -> Self {
        self.handlers
            .websocket
            .register(priority, Arc::new(middleware));
        self
    }

    /// Registers a static-request predicate with the default priority (API-08).
    pub fn static_request(self, predicate: impl StaticRequestPredicate) -> Self {
        self.static_request_with_priority(DEFAULT_PRIORITY, predicate)
    }

    /// Registers a static-request predicate with a priority (CFG-19).
    pub fn static_request_with_priority(
        mut self,
        priority: i32,
        predicate: impl StaticRequestPredicate,
    ) -> Self {
        self.handlers
            .static_request
            .register(priority, Arc::new(predicate));
        self
    }

    /// Registers an MCP authentication handler with the default priority (API-09).
    pub fn mcp_authentication(self, handler: impl McpAuthentication) -> Self {
        self.mcp_authentication_with_priority(DEFAULT_PRIORITY, handler)
    }

    /// Registers an MCP authentication handler with a priority (CFG-19).
    pub fn mcp_authentication_with_priority(
        mut self,
        priority: i32,
        handler: impl McpAuthentication,
    ) -> Self {
        self.handlers
            .mcp_authentication
            .register(priority, Arc::new(handler));
        self
    }

    /// Sets the Identity resolver (API-15). Without one, a request's Identity is its
    /// normalised client address, not authenticated (RL-11).
    pub fn identity_resolver(mut self, resolver: impl IdentityResolver) -> Self {
        self.handlers.identity_resolver = Some(Arc::new(resolver));
        self
    }

    /// Replaces how gateway errors are rendered (REQ-25).
    pub fn error_renderer(mut self, renderer: impl ErrorRenderer + 'static) -> Self {
        self.renderer = Arc::new(renderer);
        self
    }

    /// The application's own routes (CFG-18). They take precedence over the proxy, which
    /// handles every request they do not match. The router must not set a fallback.
    pub fn routes(mut self, routes: Router) -> Self {
        self.routes = routes;
        self
    }

    /// Wraps everything the gateway answers over HTTP in `layer` (REQ-27): the application's
    /// routes ([`routes`](Self::routes)), the gateway's endpoints (`/documents`,
    /// `/document-json`, `/mcp`), the proxy's Routed, Static and Bypass responses, and the
    /// gateway's error responses, its renderer's and Express's own checks (REQ-26, the
    /// asterisk-form target) alike. Any [`tower::Layer`] that [`Router::layer`] takes will do,
    /// such as `tower_http`'s `CorsLayer` or `SetResponseHeaderLayer`, or
    /// [`axum::middleware::from_fn`].
    ///
    /// The layer sees each request before the gateway does anything with it, so it can answer
    /// a request itself (a CORS preflight, say): the proxy, the static predicates, the
    /// middleware chain and the rate limiter then never run. It sees each response last, so
    /// headers it sets override the upstream's or the renderer's. Only the headers Node adds
    /// to every response come after it, in Node's order: `X-Powered-By` when it is turned on
    /// ([`x_powered_by`](Self::x_powered_by)), `Content-Length` for a body of known length,
    /// `Date`, `Connection`, `Keep-Alive` and `Transfer-Encoding`, so a layer should not set
    /// those. `Expect: 100-continue` is answered before the layer runs (FWD-12).
    ///
    /// WebSocket upgrades proxied by [`Gateway::serve`] never reach the layer: they are taken
    /// over before the router (area 06), as Node hands them to the Reference Gateway apart
    /// from Express, and are proxied exactly as without a layer; the answers to rejected
    /// upgrades (Deviation D1) do not pass through it either. Check upgrades with a
    /// [`WsProxyMiddleware`] instead. When [`Gateway::router`] is served another way, an
    /// upgrade request does reach the layer, and the router answers it 501.
    ///
    /// Call it more than once to add more layers: like [`Router::layer`], each wraps those
    /// added before it, so the last added sees the request first and the response last.
    /// Without a layer, every response is exactly what it is without this method.
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        self.layers
            .push(Arc::new(move |router: Router| router.layer(layer.clone())));
        self
    }

    /// Replaces how documents are fetched (the default is a plain HTTP `GET`, OAS-01).
    pub fn document_fetcher(mut self, fetcher: impl DocumentFetcher) -> Self {
        self.fetcher = Some(Arc::new(fetcher));
        self
    }

    /// Whether responses carry `X-Powered-By: Express` (default false, Deviation D11). The
    /// reference sends it on every response, an Express default; `true` restores it exactly
    /// where Express puts it: first among the headers, unless a forwarded response has its
    /// own `x-powered-by`, which is kept instead (FWD-15, REQ-23). The only switch for that
    /// header.
    pub fn x_powered_by(mut self, enabled: bool) -> Self {
        self.x_powered_by = enabled;
        self
    }

    /// The TLS settings for `https` Upstream Services (FWD-19) and `https` document URLs
    /// (OAS-01). The default trusts Mozilla's root certificates, built in, as Node trusts its
    /// bundled store: the host's trust store is never read, so a host without CA
    /// certificates still starts the gateway, and an `https` peer those roots do not cover
    /// fails per request (503, FWD-22) or per document fetch, with the TLS error logged.
    pub fn upstream_tls(mut self, config: rustls::ClientConfig) -> Self {
        self.upstream_tls = Some(Arc::new(config));
        self
    }

    /// Builds the gateway and starts loading the Upstream Services' documents in the
    /// background (OAS-01): startup does not wait for them (CFG-16). Must be called inside a
    /// Tokio runtime.
    pub fn build(self) -> Result<Gateway, BuildError> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(BuildError::NoRuntime);
        }
        let options = self.options;
        let limiter = RateLimiter::new(&options.rate_limit, &options.redis)?;
        let tls = self
            .upstream_tls
            .unwrap_or_else(|| Arc::new(crate::tls::default_client_config()));
        let mut pools = HashMap::new();
        for service in &options.api_services {
            let origin = Origin::parse(&service.host).map_err(BuildError::UpstreamOrigin)?;
            pools.insert(
                service.prefix.clone(),
                Arc::new(UpstreamPool::new(
                    origin,
                    options.pool.clone(),
                    Some(Arc::clone(&tls)),
                )),
            );
        }
        let selector = ServiceSelector::new(
            options
                .api_services
                .iter()
                .map(|s| (s.prefix.clone(), s.direct_prefixes.clone())),
        );
        let store = Arc::new(DocumentStore::new(
            options.api_services.clone(),
            SecurityKeys {
                bearer: options.open_api_security_keys.clone(),
                api_key: options.open_api_security_api_keys.clone(),
            },
        ));
        let fetcher: Arc<dyn DocumentFetcher> = match self.fetcher {
            Some(fetcher) => fetcher,
            None => Arc::new(HttpDocumentFetcher::with_tls((*tls).clone(), None)?),
        };
        let loader = DocumentLoader::new(Arc::clone(&store), Arc::clone(&fetcher)).start();
        let endpoints = Endpoints {
            documents: options.documents.enabled.then(|| {
                DocumentEndpoints::new(
                    Arc::clone(&store),
                    fetcher,
                    documents::DocumentOptions {
                        exclude_headers: options.exclude_headers.clone(),
                        open_api_security_keys: options.open_api_security_keys.clone(),
                        hide_document_ids: options.documents.hide_document_ids.clone(),
                    },
                    options.scalar_options.clone(),
                )
            }),
            #[cfg(feature = "mcp")]
            mcp: match &options.mcp {
                Some(mcp) if mcp.enabled => {
                    let mut mcp_pools = HashMap::new();
                    for service in &options.api_services {
                        let origin =
                            Origin::parse(&service.host).map_err(BuildError::UpstreamOrigin)?;
                        let pool =
                            UpstreamPool::new(origin, options.pool.clone(), Some(Arc::clone(&tls)))
                                .with_raw_heads();
                        mcp_pools.insert(service.prefix.clone(), Arc::new(pool));
                    }
                    Some(crate::endpoints::mcp::McpMount::start(
                        crate::endpoints::mcp::McpParts {
                            store: Arc::clone(&store),
                            options: mcp.clone(),
                            limiter: limiter.clone(),
                            handlers: self.handlers.clone(),
                            exclude_headers: options.exclude_headers.clone(),
                            pools: mcp_pools,
                            renderer: Arc::clone(&self.renderer),
                        },
                    ))
                }
                _ => None,
            },
        };
        Ok(Gateway {
            shared: Arc::new(Shared {
                options,
                store,
                selector,
                limiter,
                pools,
                handlers: self.handlers,
                renderer: self.renderer,
                x_powered_by: self.x_powered_by,
                endpoints,
                _loader: Mutex::new(Some(loader)),
            }),
            routes: self.routes,
            layers: self.layers.into(),
        })
    }
}

/// State shared by every request.
pub(crate) struct Shared {
    pub(crate) options: GatewayOptions,
    pub(crate) store: Arc<DocumentStore>,
    pub(crate) selector: ServiceSelector,
    pub(crate) limiter: RateLimiter,
    pub(crate) pools: HashMap<String, Arc<UpstreamPool>>,
    pub(crate) handlers: Handlers,
    pub(crate) renderer: Arc<dyn ErrorRenderer>,
    pub(crate) x_powered_by: bool,
    pub(crate) endpoints: Endpoints,
    _loader: Mutex<Option<LoaderHandle>>,
}

/// A built gateway (API-01). Cheap to clone; clones share everything, and document loading
/// stops when the last clone is dropped.
#[derive(Clone)]
pub struct Gateway {
    pub(crate) shared: Arc<Shared>,
    routes: Router,
    layers: Arc<[ApplyLayer]>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("services", &self.shared.options.api_services)
            .field("handlers", &self.shared.handlers)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// A builder for these options.
    pub fn builder(options: GatewayOptions) -> GatewayBuilder {
        GatewayBuilder::new(options)
    }

    /// This gateway with `routes` as the application's routes (CFG-18), replacing those
    /// given to the builder. For routes that need the built gateway, such as one reporting
    /// [`documents`](Self::documents). The router must not set a fallback.
    pub fn with_routes(mut self, routes: Router) -> Self {
        self.routes = routes;
        self
    }

    /// The options the gateway was built with.
    pub fn options(&self) -> &GatewayOptions {
        &self.shared.options
    }

    /// The loaded documents and Routes (API-12): the reference's `OpenApiService`.
    pub fn documents(&self) -> &Arc<DocumentStore> {
        &self.shared.store
    }

    /// Resolves once every Upstream Service's first document load has settled (CFG-16).
    pub async fn ready(&self) {
        self.shared.store.ready().await;
    }

    /// Whether every Upstream Service's first document load has settled (CFG-16).
    pub fn is_ready(&self) -> bool {
        self.shared.store.is_ready()
    }

    /// The rate limiter (area 05), for endpoints that rate-limit on their own (MCP-15).
    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.shared.limiter
    }

    /// The registered handlers.
    pub fn handlers(&self) -> &Handlers {
        &self.shared.handlers
    }

    /// The renderer of gateway errors (REQ-25).
    pub fn error_renderer(&self) -> &Arc<dyn ErrorRenderer> {
        &self.shared.renderer
    }

    /// The router to serve: the application's routes (CFG-18), then the gateway's own
    /// endpoints (`/documents` and `/document-json` when documents are enabled, CFG-14;
    /// `POST /mcp` when MCP is enabled, CFG-13; matched as Express matches them, ignoring
    /// ASCII case and one trailing slash), then the proxy for everything else (REQ-01),
    /// wrapped in the HTTP
    /// behaviours the reference gets from Express and Node (REQ-26, the asterisk-form
    /// target, `Date`, `Connection` and `Keep-Alive` on every response, and `X-Powered-By`
    /// when [`GatewayBuilder::x_powered_by`] turns it on). The application's layers
    /// ([`GatewayBuilder::layer`], REQ-27) sit between the two: outside Express's checks,
    /// inside the headers Node adds.
    ///
    /// [`serve`](Self::serve) serves it with the client address and `Expect: 100-continue`
    /// handled as Node does; an application serving it another way must provide
    /// [`axum::extract::ConnectInfo`] for the client address (FWD-07, RL-06).
    pub fn router(&self) -> Router {
        let gateway = self.clone();
        let fallback = move |request: axum::extract::Request| {
            let gateway = gateway.clone();
            async move { crate::endpoints::dispatch(&gateway.shared, request).await }
        };
        let routes = self.routes.clone();
        // OTEL-02: the matched route names an application route's server span.
        #[cfg(feature = "otel")]
        let routes = routes.layer(axum::middleware::from_fn(crate::otel::record_matched_path));
        let router = routes
            .fallback(fallback)
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.shared),
                crate::server::express_checks,
            ));
        // REQ-27: the application's layers, in the order they were added.
        let router = self
            .layers
            .iter()
            .fold(router, |router, apply| apply(router));
        let router = router.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&self.shared),
            crate::server::node_compat,
        ));
        // OTEL-02: the server span covers every response, Express's own included.
        #[cfg(feature = "otel")]
        let router = router.layer(axum::middleware::from_fn(crate::otel::server_span));
        router
    }

    /// The current MCP tools (API-21, MCP-05 to MCP-13): a snapshot of the list `tools/list`
    /// answers, rebuilt after the first document load and every 5 minutes. `None` when MCP
    /// is not enabled (CFG-13).
    #[cfg(feature = "mcp")]
    pub fn mcp_tools(&self) -> Option<Arc<Vec<Arc<crate::mcp::ToolDefinition>>>> {
        self.shared
            .endpoints
            .mcp
            .as_ref()
            .map(|mount| mount.registry.tools())
    }

    /// Serves the gateway on `listener` until `shutdown` resolves, then stops accepting
    /// connections, closes open WebSocket tunnels, and waits for the other connections to
    /// finish their current request.
    ///
    /// WebSocket upgrades (area 06) are proxied only here: they are taken over before they
    /// reach the [`router`](Self::router), which answers them 501 when served another way.
    ///
    /// Use [`server::bind_dual_stack`](crate::server::bind_dual_stack) to listen like Node
    /// does (FWD-07).
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send,
    ) -> io::Result<()> {
        crate::server::serve_gateway(self.router(), Arc::clone(&self.shared), listener, shutdown)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // CFG-05, CFG-08, CFG-11, CFG-12: the options hold the reference's fields; the Identity
    // resolver is optional.
    #[test]
    fn cfg_05_cfg_08_cfg_11_cfg_12_options_hold_the_reference_fields() {
        use crate::rate_limit::BucketLimit;
        let rate_limit =
            RateLimitOptions::new(true, BucketLimit::new(60, 60.0), BucketLimit::new(30, 1.5));
        let mut options = GatewayOptions::new(
            vec![UpstreamService::new("svc", "http://a/doc", "http://a")],
            rate_limit.clone(),
            RedisOptions::new("localhost", 6379),
        );
        assert_eq!(options.rate_limit, rate_limit);
        assert_eq!(options.documents, DocumentOptions::default());
        assert!(!options.documents.enabled);
        options.documents = DocumentOptions {
            enabled: true,
            hide_document_ids: vec!["AppController_getHello".into()],
        };
        options.scalar_options = Some(json!({ "showExtensions": true }));
        options.bypass_route_prefixes = vec!["svc/public".into()];
        assert_eq!(
            options.documents.hide_document_ids,
            ["AppController_getHello"]
        );
        assert_eq!(
            options.scalar_options,
            Some(json!({ "showExtensions": true }))
        );
        assert!(
            GatewayBuilder::new(options)
                .handlers
                .identity_resolver()
                .is_none()
        );
    }

    // CFG-10: the undici options the port maps, with undici's defaults.
    #[test]
    fn cfg_10_pool_options_map_undici_fields() {
        assert_eq!(PoolOptions::from_undici(&json!({})), PoolOptions::default());
        let options = PoolOptions::from_undici(&json!({
            "connections": 100,
            "pipelining": 1,
            "keepAliveTimeout": 1500,
            "headersTimeout": 500,
            "bodyTimeout": 0,
            "connectTimeout": 2000,
        }));
        assert_eq!(
            options,
            PoolOptions {
                connections: Some(100),
                keep_alive_timeout: Duration::from_millis(1500),
                headers_timeout: Some(Duration::from_millis(500)),
                body_timeout: None,
                connect_timeout: Some(Duration::from_secs(2)),
            }
        );
        assert_eq!(
            PoolOptions::from_undici(&json!({"connections": null})).connections,
            None
        );
    }
}
