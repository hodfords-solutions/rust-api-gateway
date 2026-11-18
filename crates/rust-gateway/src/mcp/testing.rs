//! In-memory doubles of [`ToolCallBackend`] and [`McpAuthentication`], for tests of the
//! executor and the endpoint without a server, Redis or upstream.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};

use super::executor::{
    BoxFuture, ToolCallBackend, ToolCallContext, ToolCallRejection, UpstreamCall, UpstreamError,
    UpstreamResponse,
};
use crate::handler::{
    GatewayRequest, McpAuthDecision, McpAuthentication, MiddlewareError, async_trait,
};
use crate::request::ProxyRequest;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What [`RecordingBackend`] saw of one `authorize` call.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorizeRecord {
    /// The tool name.
    pub tool: String,
    /// The header names of the `/mcp` request as received, lower-case, in order.
    pub request_headers: Vec<String>,
    /// The header names the middleware was to see, lower-case, in order.
    pub middleware_headers: Vec<String>,
    /// The arguments (the middleware's request body).
    pub arguments: Map<String, Value>,
}

/// A [`ToolCallBackend`] that records every call and answers from scripts.
///
/// `authorize` returns the next scripted outcome, or a [`ProxyRequest`] with the headers of
/// [`RecordingBackend::middleware_headers`] when none is queued. `send` returns the next
/// scripted response, or `200` with the call's target as the body.
#[derive(Debug, Default)]
pub struct RecordingBackend {
    authorize_outcomes: Mutex<VecDeque<Result<ProxyRequest, ToolCallRejection>>>,
    middleware: Mutex<Vec<(String, String)>>,
    responses: Mutex<VecDeque<Result<UpstreamResponse, UpstreamError>>>,
    authorized: Mutex<Vec<AuthorizeRecord>>,
    sent: Mutex<Vec<(String, UpstreamCall)>>,
}

impl RecordingBackend {
    /// A backend that admits every call and answers 200.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Headers every admitted call's middleware adds.
    pub fn middleware_headers(&self, headers: &[(&str, &str)]) {
        *lock(&self.middleware) = headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
    }

    /// Queues the outcome of the next `authorize`.
    pub fn push_authorize(&self, outcome: Result<ProxyRequest, ToolCallRejection>) {
        lock(&self.authorize_outcomes).push_back(outcome);
    }

    /// Queues the outcome of the next `send`.
    pub fn push_response(&self, outcome: Result<UpstreamResponse, UpstreamError>) {
        lock(&self.responses).push_back(outcome);
    }

    /// Every `authorize` call so far.
    pub fn authorized(&self) -> Vec<AuthorizeRecord> {
        lock(&self.authorized).clone()
    }

    /// Every `send` call so far: the service and the request.
    pub fn sent(&self) -> Vec<(String, UpstreamCall)> {
        lock(&self.sent).clone()
    }
}

fn names(headers: &http::HeaderMap) -> Vec<String> {
    headers.keys().map(|k| k.as_str().to_string()).collect()
}

impl ToolCallBackend for RecordingBackend {
    fn authorize<'a>(
        &'a self,
        context: ToolCallContext<'a>,
    ) -> BoxFuture<'a, Result<ProxyRequest, ToolCallRejection>> {
        lock(&self.authorized).push(AuthorizeRecord {
            tool: context.tool.name.clone(),
            request_headers: names(&context.request.headers),
            middleware_headers: names(context.middleware_headers),
            arguments: context.arguments.clone(),
        });
        let outcome = lock(&self.authorize_outcomes)
            .pop_front()
            .unwrap_or_else(|| {
                let mut proxy_request = ProxyRequest::new();
                proxy_request.add_headers(lock(&self.middleware).clone());
                Ok(proxy_request)
            });
        Box::pin(std::future::ready(outcome))
    }

    fn send<'a>(
        &'a self,
        service: &'a str,
        call: UpstreamCall,
    ) -> BoxFuture<'a, Result<UpstreamResponse, UpstreamError>> {
        let fallback = UpstreamResponse {
            status: 200,
            body: call.path_and_query.clone().into_bytes(),
        };
        lock(&self.sent).push((service.to_string(), call));
        let outcome = lock(&self.responses).pop_front().unwrap_or(Ok(fallback));
        Box::pin(std::future::ready(outcome))
    }
}

/// An [`McpAuthentication`] that decides from the `x-test-mcp-auth` header, like the
/// conformance protocol's handler: `allow` (default), `deny`, `respond` (418 with a JSON
/// body) or `throw`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeaderAuthentication;

#[async_trait]
impl McpAuthentication for HeaderAuthentication {
    async fn authenticate(
        &self,
        request: &mut GatewayRequest,
    ) -> Result<McpAuthDecision, MiddlewareError> {
        match request.header("x-test-mcp-auth").unwrap_or("allow") {
            "allow" => Ok(McpAuthDecision::Allow),
            "deny" => Ok(McpAuthDecision::Deny),
            "respond" => {
                let mut response = http::Response::new(bytes::Bytes::from_static(
                    br#"{"message":"test mcp authentication response"}"#,
                ));
                *response.status_mut() = http::StatusCode::IM_A_TEAPOT;
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json; charset=utf-8"),
                );
                Ok(McpAuthDecision::Respond(response))
            }
            other => Err(MiddlewareError::other(format!(
                "unknown x-test-mcp-auth mode: {other}"
            ))),
        }
    }
}
