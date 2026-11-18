//! The 429 rejection (RL-20 to RL-23, API-13, API-17).

use super::js;

/// A request rejected because a Bucket is full: the port of `TooManyRequestException`.
///
/// The pipeline answers it with [`TooManyRequests::STATUS`] and [`TooManyRequests::body`] as
/// a JSON body (`Content-Type: application/json; charset=utf-8`, REQ-23), and with no
/// `Retry-After` or `X-RateLimit-*` header (RL-22). How long until the full Bucket resets is
/// on the request's [`RateLimitState::retry_after`](super::RateLimitState::retry_after), and
/// reaches the error renderer, which may send `Retry-After` (RL-25).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TooManyRequests {
    /// The Global IP Bucket is full (RL-09, RL-20).
    #[error("the Global IP Bucket is full")]
    GlobalIp,
    /// The Anonymous IP Bucket is full (RL-10, RL-20).
    #[error("the Anonymous IP Bucket is full")]
    AnonymousIp,
    /// The Custom Bucket is full (RL-14, RL-20).
    #[error("the Custom Bucket is full")]
    Custom,
    /// A Route Bucket is full (RL-15, RL-16, RL-21). `ttl` is that limit's window in
    /// seconds.
    #[error("a Route Bucket is full")]
    Route {
        /// The full limit's window, in seconds.
        ttl: f64,
    },
}

impl TooManyRequests {
    /// The HTTP status of every rejection.
    pub const STATUS: u16 = 429;

    /// The `message` field of the body.
    pub fn message(&self) -> &'static str {
        match self {
            TooManyRequests::Route { .. } => "error.too_many_requests_with_custom_ttl",
            _ => "error.too_many_requests",
        }
    }

    /// The exact response body:
    /// `{"message":"error.too_many_requests"}` for a gateway-wide Bucket (RL-20), and
    /// `{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"<text>"}}` for a
    /// Route Bucket, where the text is [`ttl_to_human_readable`] of its window (RL-21).
    pub fn body(&self) -> String {
        match self {
            TooManyRequests::Route { ttl } => {
                let text = json_string(&ttl_to_human_readable(*ttl));
                format!(
                    r#"{{"message":"{}","args":{{"ttl":{text}}}}}"#,
                    self.message()
                )
            }
            _ => format!(r#"{{"message":"{}"}}"#, self.message()),
        }
    }
}

fn json_string(text: &str) -> String {
    // Serialising a string cannot fail; fall back to an empty string literal regardless.
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Describes a window of `ttl` seconds the way the Route rejection does (RL-21):
/// under 60 s `less than a minute`; 60–119 s `the next one minute`; up to 59 minutes
/// `the next N minutes`; 60–119 minutes `the next one hour`; beyond that
/// `the next N hours` (whole minutes and hours, rounded down).
pub fn ttl_to_human_readable(ttl: f64) -> String {
    let minutes = (ttl / 60.0).floor();
    if minutes < 1.0 {
        "less than a minute".to_owned()
    } else if minutes == 1.0 {
        "the next one minute".to_owned()
    } else if minutes < 60.0 {
        format!("the next {} minutes", js::integer_to_string(minutes))
    } else if minutes < 120.0 {
        "the next one hour".to_owned()
    } else {
        format!(
            "the next {} hours",
            js::integer_to_string((minutes / 60.0).floor())
        )
    }
}
