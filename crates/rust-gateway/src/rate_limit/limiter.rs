//! The limiter's operations (RL-01, RL-09 to RL-19), shaped by the reference's call order.

use std::net::IpAddr;
use std::sync::Arc;

use http::HeaderMap;

use super::bucket::{self, window_ms};
pub use crate::route::RouteLimit;
use super::client_address::{client_key, forwarded_for};
use super::js;
use super::options::{BucketLimit, OptionsError, RateLimitOptions, ValidatedOptions};
use super::rejection::TooManyRequests;
use super::store::{RedisConfigError, RedisOptions, Store, StoreError};

/// Identifies a Route for its Route Buckets: the Upstream Service, the request method and
/// the Route pattern (`routerPath`).
///
/// For an MCP tool call the method is `POST`, the `/mcp` request's method (MCP-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteKey<'a> {
    /// The Upstream Service's prefix (or the Default Service's name).
    pub service: &'a str,
    /// The request method, upper case.
    pub method: &'a str,
    /// The Route pattern.
    pub pattern: &'a str,
}

impl<'a> RouteKey<'a> {
    /// A Route key from its three parts.
    pub fn new(service: &'a str, method: &'a str, pattern: &'a str) -> Self {
        Self {
            service,
            method,
            pattern,
        }
    }
}

/// The status a Route limit is scoped to: `None` when it has none or it is 0, which the
/// reference treats as unset.
fn effective_status(limit: &RouteLimit) -> Option<u16> {
    limit.status.filter(|&status| status != 0)
}

/// Whether any of a Route's limits is status-scoped, so that the pipeline must call
/// [`RateLimiter::count_after_response`] once the response completes (the reference's
/// `checkRouterHasCustomLimit`).
pub fn has_status_limits(limits: &[RouteLimit]) -> bool {
    limits
        .iter()
        .any(|limit| effective_status(limit).is_some())
}

/// What the Identity resolver returned for a request: the key its Custom and Route Buckets
/// are counted against, and whether the requester is authenticated (RL-11, API-15).
///
/// The resolver itself (`keyResolver`) is a user callback that receives the request, the
/// Route and the normalised client address ([`RateLimitState::client_key`]); the request
/// pipeline owns and invokes it, and returns `Option<Identity>` from it. `None`, or a `key`
/// that is absent, empty or only whitespace, means "no key": the Custom and Route Buckets are
/// skipped for the request (RL-12).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// The Bucket key, used exactly as given.
    pub key: Option<String>,
    /// True when the request carries a verified identity; such requests skip the Anonymous
    /// IP Bucket (RL-10).
    pub authenticated: bool,
}

impl Identity {
    /// An unauthenticated Identity with this key (the reference's plain string result).
    pub fn anonymous(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            authenticated: false,
        }
    }

    /// An authenticated Identity with this key.
    pub fn authenticated(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            authenticated: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedIdentity {
    Skip,
    Key(String),
}

/// Per-request rate-limit state, created by [`RateLimiter::request_state`] and passed to
/// every operation for that request. It replaces the symbols the reference attaches to the
/// request object: whether the Global IP and Anonymous IP Buckets have counted the request,
/// and the Identity resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitState {
    client_key: String,
    global_ip_counted: bool,
    anonymous_ip_counted: bool,
    identity: Option<ResolvedIdentity>,
}

impl RateLimitState {
    fn new(client_key: String) -> Self {
        Self {
            client_key,
            global_ip_counted: false,
            anonymous_ip_counted: false,
            identity: None,
        }
    }

    /// The normalised client address (RL-06, RL-07), or `unknown`. This is the `ip` the
    /// Identity resolver receives.
    pub fn client_key(&self) -> &str {
        &self.client_key
    }

    /// The Identity used when no resolver is configured: the normalised client address, not
    /// authenticated (RL-11).
    pub fn default_identity(&self) -> Identity {
        Identity::anonymous(self.client_key.clone())
    }
}

/// A rate-limit operation that did not admit the request.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RateLimitError {
    /// A Bucket is full: answer 429 with [`TooManyRequests::body`].
    #[error(transparent)]
    TooManyRequests(#[from] TooManyRequests),
    /// Redis failed or could not be reached in time: answer 500 (REQ-24, Deviation D10).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A window truncates to less than 1 ms (or is not finite): answer 500, as the
    /// reference's Redis call fails for it.
    #[error("rate-limit window of {ttl} seconds is not usable")]
    InvalidWindow {
        /// The window in seconds.
        ttl: f64,
    },
}

/// The rate limiter could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RateLimiterConfigError {
    /// Invalid rate-limit options (CFG-06, CFG-07, RL-06).
    #[error(transparent)]
    Options(#[from] OptionsError),
    /// Invalid Redis options (CFG-09).
    #[error(transparent)]
    Redis(#[from] RedisConfigError),
}

/// The rate limiter: the port of the reference's `ThrottlerService`, holding the options and
/// the Redis connection. Cheap to clone; clones share the connection.
///
/// See the [module documentation](super) for the order in which the pipeline calls the
/// operations.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    options: ValidatedOptions,
    store: Backend,
}

/// Where Buckets live: Redis, or (in unit tests) memory.
#[derive(Debug)]
enum Backend {
    Redis(Box<Store>),
    #[cfg(test)]
    Memory(memory::MemoryStore),
}

impl Backend {
    async fn check_and_count(
        &self,
        key: &str,
        limit: u64,
        window_ms: u64,
    ) -> Result<bool, StoreError> {
        match self {
            Backend::Redis(store) => store.check_and_count(key, limit, window_ms).await,
            #[cfg(test)]
            Backend::Memory(store) => Ok(store.check_and_count(key, limit, window_ms)),
        }
    }

    async fn count(&self, key: &str) -> Result<u64, StoreError> {
        match self {
            Backend::Redis(store) => store.count(key).await,
            #[cfg(test)]
            Backend::Memory(store) => Ok(store.count(key)),
        }
    }
}

impl RateLimiter {
    /// Validates the options and prepares the Redis connection. No connection is opened
    /// here: it is opened by the first operation that needs a Bucket, so a disabled limiter
    /// never contacts Redis (RL-01).
    pub fn new(
        options: &RateLimitOptions,
        redis: &RedisOptions,
    ) -> Result<Self, RateLimiterConfigError> {
        let options = options.validated()?;
        let store = Backend::Redis(Box::new(Store::new(redis)?));
        Ok(Self {
            inner: Arc::new(Inner { options, store }),
        })
    }

    #[cfg(test)]
    pub(crate) fn in_memory(
        options: &RateLimitOptions,
        store: memory::MemoryStore,
    ) -> Result<Self, RateLimiterConfigError> {
        let options = options.validated()?;
        Ok(Self {
            inner: Arc::new(Inner {
                options,
                store: Backend::Memory(store),
            }),
        })
    }

    /// Whether rate limiting is enabled. When it is not, every operation is a no-op and the
    /// pipeline must not call the Identity resolver.
    pub fn is_enabled(&self) -> bool {
        self.inner.options.enabled
    }

    /// The state for a new request: derives the client address from the connection's peer
    /// address and the `X-Forwarded-For` headers per the `trust proxy` setting, and
    /// normalises it (RL-06, RL-07).
    pub fn request_state(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> RateLimitState {
        let options = &self.inner.options;
        let address = options
            .trusted_proxies
            .client_address(peer, forwarded_for(headers).as_deref());
        RateLimitState::new(client_key(address.as_deref(), options.ipv6_subnet))
    }

    /// Counts the request against its Global IP Bucket (RL-09). At most once per request:
    /// later calls with the same state do nothing.
    pub async fn count_global_ip(&self, state: &mut RateLimitState) -> Result<(), RateLimitError> {
        if !self.is_enabled() || state.global_ip_counted {
            return Ok(());
        }
        state.global_ip_counted = true;
        let key = bucket::global_ip_key(&state.client_key);
        if self
            .check_and_count(&key, self.inner.options.global_ip)
            .await?
        {
            Ok(())
        } else {
            Err(TooManyRequests::GlobalIp.into())
        }
    }

    /// Counts the request against its Anonymous IP Bucket, when that Bucket is configured
    /// (RL-10). At most once per request. The pipeline calls it when middleware rejects the
    /// request; [`check_limits`](Self::check_limits) calls it for unauthenticated requests.
    pub async fn count_anonymous_ip(
        &self,
        state: &mut RateLimitState,
    ) -> Result<(), RateLimitError> {
        let Some(limit) = self.inner.options.anonymous_ip else {
            return Ok(());
        };
        if !self.is_enabled() || state.anonymous_ip_counted {
            return Ok(());
        }
        state.anonymous_ip_counted = true;
        let key = bucket::anonymous_ip_key(&state.client_key);
        if self.check_and_count(&key, limit).await? {
            Ok(())
        } else {
            Err(TooManyRequests::AnonymousIp.into())
        }
    }

    /// Checks the remaining Buckets for a request whose Identity has been resolved, in the
    /// reference's order (RL-18): the Global IP Bucket if it has not counted the request yet,
    /// the Anonymous IP Bucket unless the Identity is authenticated, then (when the Identity
    /// has a key) the Custom Bucket and the Route's Buckets in document order. The first
    /// full Bucket rejects; Buckets before it have counted the request.
    ///
    /// Route Buckets without a status are checked and counted; status-scoped ones are only
    /// checked (RL-15, RL-16). `identity` is the resolver's result, or
    /// [`RateLimitState::default_identity`] when no resolver is configured.
    pub async fn check_limits(
        &self,
        state: &mut RateLimitState,
        identity: Option<Identity>,
        route: RouteKey<'_>,
        limits: &[RouteLimit],
    ) -> Result<(), RateLimitError> {
        if !self.is_enabled() {
            return Ok(());
        }
        self.count_global_ip(state).await?;

        let (key, authenticated) = match identity {
            None => (None, false),
            Some(Identity { key, authenticated }) => {
                (key.filter(|key| !js::trim(key).is_empty()), authenticated)
            }
        };
        state.identity = Some(match &key {
            Some(key) => ResolvedIdentity::Key(key.clone()),
            None => ResolvedIdentity::Skip,
        });

        if !authenticated {
            self.count_anonymous_ip(state).await?;
        }
        let Some(identity) = key else {
            return Ok(());
        };

        let custom = bucket::custom_key(&identity);
        if !self
            .check_and_count(&custom, self.inner.options.custom)
            .await?
        {
            return Err(TooManyRequests::Custom.into());
        }

        for limit in limits {
            let status = effective_status(limit);
            let window = window_ms(limit.ttl);
            let key = bucket::route_key(&route, &identity, limit.limit, window, status);
            let admitted = if status.is_some() {
                self.inner.store.count(&key).await? < limit.limit
            } else {
                self.check_and_count(&key, BucketLimit::new(limit.limit, limit.ttl))
                    .await?
            };
            if !admitted {
                return Err(TooManyRequests::Route { ttl: limit.ttl }.into());
            }
        }
        Ok(())
    }

    /// After the response to a Routed Request has completed, counts it against each
    /// status-scoped Route Bucket whose status equals the response status (RL-16), using the
    /// Identity resolved for the request (RL-19). Nothing is counted when that Identity had
    /// no key; if no Identity was resolved, the client address is used, as in the
    /// reference.
    pub async fn count_after_response(
        &self,
        state: &RateLimitState,
        route: RouteKey<'_>,
        limits: &[RouteLimit],
        status: u16,
    ) -> Result<(), RateLimitError> {
        if !self.is_enabled() {
            return Ok(());
        }
        let identity = match &state.identity {
            Some(ResolvedIdentity::Skip) => return Ok(()),
            Some(ResolvedIdentity::Key(key)) => key.as_str(),
            None => state.client_key.as_str(),
        };
        for limit in limits {
            if effective_status(limit) != Some(status) {
                continue;
            }
            let key = bucket::route_key(
                &route,
                identity,
                limit.limit,
                window_ms(limit.ttl),
                Some(status),
            );
            self.check_and_count(&key, BucketLimit::new(limit.limit, limit.ttl))
                .await?;
        }
        Ok(())
    }

    async fn check_and_count(&self, key: &str, limit: BucketLimit) -> Result<bool, RateLimitError> {
        let window =
            window_ms(limit.ttl).ok_or(RateLimitError::InvalidWindow { ttl: limit.ttl })?;
        Ok(self
            .inner
            .store
            .check_and_count(key, limit.limit, window)
            .await?)
    }
}

#[cfg(test)]
pub(crate) mod memory {
    //! An in-memory Bucket store with the same algorithm, recording every call, for unit
    //! tests (the reference specs use a Redis mock the same way).

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Default)]
    pub(crate) struct MemoryStore {
        state: Arc<Mutex<State>>,
    }

    #[derive(Debug, Default)]
    struct State {
        counts: HashMap<String, u64>,
        calls: Vec<String>,
    }

    impl MemoryStore {
        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Windows never expire here; expiry is covered by the Redis tests.
        pub(crate) fn check_and_count(&self, key: &str, limit: u64, _window_ms: u64) -> bool {
            let mut state = self.lock();
            state.calls.push(format!("count {key}"));
            let current = state.counts.get(key).copied().unwrap_or(0);
            if current > 0 && current >= limit {
                return false;
            }
            state.counts.insert(key.to_owned(), current + 1);
            true
        }

        pub(crate) fn count(&self, key: &str) -> u64 {
            let mut state = self.lock();
            state.calls.push(format!("get {key}"));
            state.counts.get(key).copied().unwrap_or(0)
        }

        /// The count of the Bucket with this key, `None` if it was never opened.
        pub(crate) fn get(&self, key: &str) -> Option<u64> {
            self.lock().counts.get(key).copied()
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.lock().calls.clone()
        }

        pub(crate) fn clear_calls(&self) {
            self.lock().calls.clear();
        }
    }
}
