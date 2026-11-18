//! Rate limiting: Parity Checklist area 05 (`docs/parity/05-rate-limiting.md`), the port of
//! the Reference Gateway's `throttlers/`.
//!
//! Requests are counted in fixed-window Buckets kept in Redis, so that several gateway
//! instances share them. There are four kinds (see `CONTEXT.md`):
//!
//! - **Global IP**: per normalised client address, every Routed Request (RL-09);
//! - **Anonymous IP** (optional): per client address, requests that middleware rejected or
//!   whose Identity is not authenticated (RL-10);
//! - **Custom**: per Identity, across all Routes (RL-14);
//! - **Route**: per Identity and Route limit, one Bucket for every entry of a Route's
//!   `x-rate-limits` (RL-15 to RL-17, Deviation D2).
//!
//! This module takes plain inputs (a [`RouteKey`], the Route's [`RouteLimit`]s, the resolved
//! [`Identity`]) and does not depend on the request or Route types. The request pipeline
//! calls its operations in the order below; a full Bucket yields
//! [`RateLimitError::TooManyRequests`] (answer 429 with [`TooManyRequests::body`]), and any
//! other error yields 500 (REQ-24). When [`RateLimiter::is_enabled`] is false every operation
//! is a no-op, and the pipeline must not call the Identity resolver either (RL-01).
//!
//! # Call order for a Routed Request
//!
//! Static Requests, Bypass Requests, WebSocket upgrades and requests without a matching Route
//! never reach the limiter (RL-24). For a Routed Request, after the Route is matched:
//!
//! 1. `let mut state = limiter.request_state(peer, &headers);`
//! 2. `limiter.count_global_ip(&mut state).await?` — before any middleware (RL-09).
//! 3. Run the proxy middleware chain. If a middleware fails, or disallows the request,
//!    first call `limiter.count_anonymous_ip(&mut state).await`: if that returns an error
//!    (429 or 500), answer with it; otherwise answer the middleware's error, or 403
//!    (REQ-18, REQ-19, RL-10).
//! 4. If the limiter is enabled, resolve the Identity: with a resolver configured, call it
//!    with the request (including what middleware attached to it, RL-13), the Route and
//!    `state.client_key()`; a resolver failure answers 500. Without a resolver, use
//!    `Some(state.default_identity())`.
//! 5. `limiter.check_limits(&mut state, identity, RouteKey::new(service, method, pattern),
//!    &limits).await?` — Anonymous IP (unless authenticated), Custom, then Route Buckets in
//!    document order (RL-18).
//! 6. If [`has_status_limits`]`(&limits)`, arrange for
//!    `limiter.count_after_response(&state, route_key, &limits, status)` to run once the
//!    response to the client has completed (the reference uses the response's `finish`
//!    event). Its error cannot change the response; log it.
//! 7. Forward the request.
//!
//! # Call order for an MCP tool call (MCP-15)
//!
//! The reference rate-limits the `/mcp` request before running middleware:
//!
//! 1. `let mut state = limiter.request_state(peer, &headers_of_the_mcp_request);`
//! 2. `limiter.count_global_ip(&mut state).await?`
//! 3. If enabled, resolve the Identity as in step 4 above, with the `/mcp` request and the
//!    tool's Route (so the resolver runs before middleware).
//! 4. `limiter.check_limits(&mut state, identity, RouteKey::new(service, "POST", pattern),
//!    &limits).await?` — the Route Buckets are keyed with method `POST`, and status-scoped
//!    Route Buckets are checked but never counted.
//! 5. Run the proxy middleware chain with the tool's Route. Do not call
//!    `count_anonymous_ip` when it rejects, and never call `count_after_response`.
//!
//! # Redis
//!
//! The connection is opened on first use, replaced whenever it fails (CFG-17), and every
//! operation is bounded by [`REDIS_OPERATION_TIMEOUT`] (Deviation D10). Key names and the
//! script are not part of the External Contract (ADR 0001).

mod bucket;
mod client_address;
mod ipaddr_js;
mod js;
mod limiter;
mod options;
mod rejection;
mod store;

#[cfg(test)]
mod tests;

pub use client_address::{
    DEFAULT_IPV6_SUBNET, InvalidTrustProxy, TrustProxy, TrustedProxies, forwarded_for, normalize_ip,
};
pub use limiter::{
    Identity, RateLimitError, RateLimitState, RateLimiter, RateLimiterConfigError, RouteKey,
    RouteLimit, has_status_limits,
};
pub use options::{BucketLimit, OptionsError, RateLimitOptions};
pub use rejection::{TooManyRequests, ttl_to_human_readable};
pub use store::{
    REDIS_OPERATION_TIMEOUT, RedisConfigError, RedisOptions, RedisTls, RedisTlsOptions, StoreError,
};
