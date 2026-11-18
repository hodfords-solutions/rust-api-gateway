//! Rate-limit options and their validation (CFG-05 to CFG-07, API-14).

use super::client_address::{DEFAULT_IPV6_SUBNET, InvalidTrustProxy, TrustProxy, TrustedProxies};

/// One limit of a gateway-wide Bucket: at most `limit` requests per window of `ttl` seconds.
///
/// A window may be fractional; it is truncated to whole milliseconds when a Bucket opens
/// (RL-04). A window that truncates to less than 1 ms makes every request needing that
/// Bucket fail (the reference's Redis call fails the same way).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketLimit {
    /// Requests admitted per window. 0 behaves like 1 (RL-03).
    pub limit: u64,
    /// Window length in seconds.
    pub ttl: f64,
}

impl BucketLimit {
    /// A limit of `limit` requests per `ttl` seconds.
    pub fn new(limit: u64, ttl: f64) -> Self {
        Self { limit, ttl }
    }
}

/// Rate-limit options: the port of the Reference Gateway's `ThrottlerOption`.
///
/// The Identity resolver (`keyResolver`) is not here: it receives the request and the Route,
/// so the request pipeline owns and invokes it, and passes its result (an
/// [`Identity`](super::Identity)) to [`RateLimiter::check_limits`](super::RateLimiter::check_limits).
///
/// `trust_proxy` is not part of `ThrottlerOption` in the reference: it is the Express
/// application's `trust proxy` setting, which decides the client address the IP-based
/// Buckets are keyed by (RL-06).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitOptions {
    /// `isEnable`. When false, no Bucket is ever checked or counted and Redis is never
    /// contacted (RL-01).
    pub enabled: bool,
    /// `globalIpRateLimit` and `globalIpRateLimitTTL`: the Global IP Bucket (RL-09).
    pub global_ip: BucketLimit,
    /// `globalCustomRateLimit` and `globalCustomRateLimitTTL`: the Custom Bucket (RL-14).
    pub custom: BucketLimit,
    /// `anonymousIpRateLimit`: the Anonymous IP Bucket's limit (RL-10). Must be set together
    /// with `anonymous_ip_ttl`.
    pub anonymous_ip_limit: Option<u64>,
    /// `anonymousIpRateLimitTTL`: the Anonymous IP Bucket's window in seconds. Must be set
    /// together with `anonymous_ip_limit`.
    pub anonymous_ip_ttl: Option<f64>,
    /// `ipv6Subnet`: the prefix length IPv6 client addresses are masked to, 1 to 128.
    /// `None` means [`DEFAULT_IPV6_SUBNET`].
    pub ipv6_subnet: Option<u32>,
    /// Which proxies are trusted to report the client address (Express `trust proxy`).
    pub trust_proxy: TrustProxy,
}

impl RateLimitOptions {
    /// The required options; the Anonymous IP Bucket is off, `ipv6_subnet` is the default
    /// and no proxy is trusted.
    pub fn new(enabled: bool, global_ip: BucketLimit, custom: BucketLimit) -> Self {
        Self {
            enabled,
            global_ip,
            custom,
            anonymous_ip_limit: None,
            anonymous_ip_ttl: None,
            ipv6_subnet: None,
            trust_proxy: TrustProxy::None,
        }
    }

    /// Checks the options the way the reference's `ThrottlerService` constructor does
    /// (CFG-06, CFG-07), plus the `trust proxy` setting the way Express compiles it. The
    /// checks apply whether or not rate limiting is enabled.
    pub fn validate(&self) -> Result<(), OptionsError> {
        self.validated().map(|_| ())
    }

    pub(crate) fn validated(&self) -> Result<ValidatedOptions, OptionsError> {
        let anonymous_ip = match (self.anonymous_ip_limit, self.anonymous_ip_ttl) {
            (None, None) => None,
            (Some(limit), Some(ttl)) => {
                if limit == 0 || !(ttl.is_finite() && ttl > 0.0) {
                    return Err(OptionsError::AnonymousIpNotPositive);
                }
                Some(BucketLimit { limit, ttl })
            }
            _ => return Err(OptionsError::AnonymousIpNotPaired),
        };
        let ipv6_subnet = self.ipv6_subnet.unwrap_or(DEFAULT_IPV6_SUBNET);
        if !(1..=128).contains(&ipv6_subnet) {
            return Err(OptionsError::Ipv6Subnet(ipv6_subnet));
        }
        let trusted_proxies = TrustedProxies::new(&self.trust_proxy)?;
        Ok(ValidatedOptions {
            enabled: self.enabled,
            global_ip: self.global_ip,
            custom: self.custom,
            anonymous_ip,
            ipv6_subnet,
            trusted_proxies,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedOptions {
    pub(crate) enabled: bool,
    pub(crate) global_ip: BucketLimit,
    pub(crate) custom: BucketLimit,
    pub(crate) anonymous_ip: Option<BucketLimit>,
    pub(crate) ipv6_subnet: u32,
    pub(crate) trusted_proxies: TrustedProxies,
}

/// Invalid [`RateLimitOptions`]. The messages are the reference's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    /// Only one of `anonymous_ip_limit` and `anonymous_ip_ttl` is set (CFG-06).
    #[error(
        "ThrottlerOption.anonymousIpRateLimit and anonymousIpRateLimitTTL must be set together."
    )]
    AnonymousIpNotPaired,
    /// The Anonymous IP limit is 0, or its window is not a positive finite number (CFG-06).
    #[error(
        "ThrottlerOption.anonymousIpRateLimit and anonymousIpRateLimitTTL must be positive numbers."
    )]
    AnonymousIpNotPositive,
    /// `ipv6_subnet` is outside 1 to 128 (CFG-07).
    #[error("ThrottlerOption.ipv6Subnet must be an integer between 1 and 128. Received: {0}")]
    Ipv6Subnet(u32),
    /// The `trust proxy` setting does not compile (RL-06).
    #[error(transparent)]
    TrustProxy(#[from] InvalidTrustProxy),
}
