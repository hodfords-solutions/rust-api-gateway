//! The gateway's own endpoints: `/documents` and `/document-json` (area 07, when documents
//! are enabled, CFG-14) and `POST /mcp` (area 08, when MCP is enabled, CFG-13).
//!
//! They take precedence over the proxy and come after the application's routes. Express
//! matches them on the raw path without regard to ASCII case and with one optional trailing
//! slash, which axum's router does not do, so [`dispatch`] matches them itself before handing
//! the request to the proxy (REQ-01). A WebSocket upgrade request never reaches them: Node
//! hands upgrades to the gateway's upgrade listener, not to Express (area 06).

use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Request, Response};

use crate::documents::{DocumentEndpoint, DocumentEndpoints};
use crate::gateway::Shared;

/// The endpoints a gateway was built with.
#[derive(Default)]
pub(crate) struct Endpoints {
    /// `/documents` and `/document-json`, when documents are enabled (CFG-14).
    pub(crate) documents: Option<DocumentEndpoints>,
    /// `POST /mcp`, when MCP is enabled (CFG-13).
    #[cfg(feature = "mcp")]
    pub(crate) mcp: Option<mcp::McpMount>,
}

impl std::fmt::Debug for Endpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Endpoints");
        debug.field("documents", &self.documents.is_some());
        #[cfg(feature = "mcp")]
        debug.field("mcp", &self.mcp.is_some());
        debug.finish()
    }
}

/// Whether a request asks to switch protocols; Node never routes those through Express.
fn is_upgrade(request: &Request<Body>) -> bool {
    request.headers().contains_key(header::UPGRADE)
        && request
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Handles a request no application route claimed: a gateway endpoint if one matches, the
/// proxy otherwise (REQ-01).
pub(crate) async fn dispatch(shared: &Arc<Shared>, request: Request<Body>) -> Response<Body> {
    if !is_upgrade(&request) {
        if let Some(documents) = &shared.endpoints.documents
            && let Some(endpoint) =
                DocumentEndpoint::match_request(request.method(), request.uri().path())
        {
            #[cfg(feature = "otel")]
            crate::otel::record(
                request.extensions(),
                crate::otel::Matched::Endpoint(match endpoint {
                    DocumentEndpoint::Page => "/documents",
                    DocumentEndpoint::Json => "/document-json",
                }),
            );
            return document_response(shared, documents, endpoint, request.uri().query()).await;
        }
        #[cfg(feature = "mcp")]
        if let Some(mount) = &shared.endpoints.mcp
            && mcp::is_mcp_request(request.method(), request.uri().path())
        {
            #[cfg(feature = "otel")]
            crate::otel::record(request.extensions(), crate::otel::Matched::Endpoint("/mcp"));
            return mount.handle(request).await;
        }
    }
    #[cfg(feature = "otel")]
    crate::otel::record(request.extensions(), crate::otel::Matched::Proxy);
    crate::proxy::handle(shared, request).await
}

/// A document endpoint's response, completed as Express's `res.send` completes it:
/// `Content-Length` and a weak `ETag` (the server adds `X-Powered-By`; Node leaves out the
/// body of a `HEAD` response). Errors go through the error renderer (REQ-25).
async fn document_response(
    shared: &Arc<Shared>,
    documents: &DocumentEndpoints,
    endpoint: DocumentEndpoint,
    query: Option<&str>,
) -> Response<Body> {
    let result = match endpoint {
        DocumentEndpoint::Page => Ok(documents.page()),
        DocumentEndpoint::Json => documents.document_json(query).await,
    };
    match result {
        Ok(response) => {
            let (mut parts, body) = response.into_parts();
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
            if let Ok(etag) = HeaderValue::from_str(&crate::proxy::weak_etag(&body)) {
                parts.headers.insert(header::ETAG, etag);
            }
            Response::from_parts(parts, Body::from(Bytes::from(body)))
        }
        Err(error) => {
            if let crate::error::GatewayError::Internal {
                source: Some(source),
            } = &error
            {
                tracing::error!(error = %source, "document endpoint failed");
            }
            crate::proxy::render_error(shared, &error)
        }
    }
}

#[cfg(feature = "mcp")]
pub(crate) mod mcp {
    //! Mounting `POST /mcp` (MCP-01) and starting its tool registry (MCP-05).

    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use http::{Method, Request, Response};

    use crate::error::ErrorRenderer;
    use crate::gateway::Handlers;
    use crate::mcp::backend::GatewayBackend;
    use crate::mcp::{McpEndpoint, McpOptions, RegistryHandle, ToolExecutor, ToolRegistry};
    use crate::openapi::DocumentStore;
    use crate::proxy::pool::UpstreamPool;
    use crate::rate_limit::RateLimiter;

    /// The MCP endpoint, its registry and the registry's refresh task, which lives as long as
    /// the gateway (MCP-05).
    pub(crate) struct McpMount {
        endpoint: McpEndpoint,
        pub(crate) registry: Arc<ToolRegistry>,
        _refresh: RegistryHandle,
    }

    /// What [`McpMount::start`] needs from the gateway.
    pub(crate) struct McpParts {
        pub(crate) store: Arc<DocumentStore>,
        pub(crate) options: McpOptions,
        pub(crate) limiter: RateLimiter,
        pub(crate) handlers: Handlers,
        pub(crate) exclude_headers: Vec<String>,
        pub(crate) pools: HashMap<String, Arc<UpstreamPool>>,
        pub(crate) renderer: Arc<dyn ErrorRenderer>,
    }

    impl McpMount {
        /// Builds the endpoint and starts the registry: a rebuild after the first document
        /// load has settled, then every 5 minutes (MCP-05).
        pub(crate) fn start(parts: McpParts) -> Self {
            let registry = Arc::new(ToolRegistry::new(parts.store, Arc::new(parts.options)));
            let refresh = registry.start();
            let authenticators = parts.handlers.mcp_authentication.iter().cloned().collect();
            let backend = Arc::new(GatewayBackend {
                limiter: parts.limiter,
                handlers: parts.handlers,
                exclude_headers: parts.exclude_headers.clone(),
                pools: parts.pools,
            });
            let executor = Arc::new(ToolExecutor::new(
                Arc::clone(&registry),
                backend,
                parts.exclude_headers,
            ));
            let endpoint =
                McpEndpoint::new(executor, authenticators).with_error_renderer(parts.renderer);
            Self {
                endpoint,
                registry,
                _refresh: refresh,
            }
        }

        /// Handles one `POST /mcp` request.
        pub(crate) async fn handle(&self, request: Request<Body>) -> Response<Body> {
            self.endpoint.handle(request).await.map(Body::new)
        }
    }

    /// Whether Express routes a request to the MCP controller (MCP-01): `POST` on `/mcp`,
    /// the raw path compared without regard to ASCII case, one trailing slash allowed, any
    /// query string. `path` excludes the query.
    pub(crate) fn is_mcp_request(method: &Method, path: &str) -> bool {
        method == Method::POST
            && path
                .strip_suffix('/')
                .unwrap_or(path)
                .eq_ignore_ascii_case("/mcp")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // MCP-01: the paths Express's `@Post('mcp')` matches.
        #[test]
        fn mcp_01_endpoint_matches_like_express() {
            for path in ["/mcp", "/mcp/", "/MCP", "/Mcp/"] {
                assert!(is_mcp_request(&Method::POST, path), "{path}");
            }
            for path in ["/mcp/x", "//mcp", "/mcp//", "/%6dcp", "/mcpx", "/"] {
                assert!(!is_mcp_request(&Method::POST, path), "{path}");
            }
            for method in [Method::GET, Method::PUT, Method::DELETE, Method::HEAD] {
                assert!(!is_mcp_request(&method, "/mcp"), "{method}");
            }
        }
    }
}
