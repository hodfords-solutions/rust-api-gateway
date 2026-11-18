//! Bucket keys and the fixed-window algorithm (RL-02 to RL-05, RL-17).
//!
//! Key names and the script are not part of the External Contract (ADR 0001); the algorithm's
//! observable behaviour is the reference's `LUA_INCREASE_AND_GET_SCRIPT`.

use super::RouteKey;

/// Check-and-count in one atomic step, so that concurrent callers on any number of gateway
/// instances never admit more than `limit` requests per window (RL-05).
///
/// - No counter (or an expired one): open a window of `ARGV[2]` milliseconds with count 1
///   and admit. So the request that opens a window is always admitted, even for limit 0
///   (RL-03).
/// - Count below `ARGV[1]`: increment and admit. The window is not extended (RL-02).
/// - Otherwise: reject without counting.
///
/// Returns `{0}` when admitted, and `{1, PTTL}` when rejected: the window's remaining
/// milliseconds, read in the same atomic step so that a 429 can say when the Bucket resets
/// without another round trip (RL-25).
pub(crate) const CHECK_AND_COUNT: &str = r"
local current = tonumber(redis.call('GET', KEYS[1]) or '0')
if current > 0 then
    if current >= tonumber(ARGV[1]) then
        return {1, redis.call('PTTL', KEYS[1])}
    end
    redis.call('INCR', KEYS[1])
    return {0}
end
redis.call('SET', KEYS[1], 1, 'PX', ARGV[2])
return {0}
";

const PREFIX: &str = "rgw:rl:";

/// Builds an unambiguous key: each part is written with its byte length, so no choice of
/// Identity or Route pattern can make two different Buckets share a key.
fn key(kind: &str, parts: &[&str]) -> String {
    let mut key = format!("{PREFIX}{kind}");
    for part in parts {
        key.push(':');
        key.push_str(&part.len().to_string());
        key.push(':');
        key.push_str(part);
    }
    key
}

pub(crate) fn global_ip_key(client_key: &str) -> String {
    key("ip", &[client_key])
}

pub(crate) fn anonymous_ip_key(client_key: &str) -> String {
    key("anon-ip", &[client_key])
}

pub(crate) fn custom_key(identity: &str) -> String {
    key("custom", &[identity])
}

/// A Route Bucket's key (Deviation D2, RL-17): one Bucket per Upstream Service, method,
/// Route pattern, Identity, limit, window and status.
pub(crate) fn route_key(
    route: &RouteKey<'_>,
    identity: &str,
    limit: u64,
    window_ms: Option<u64>,
    status: Option<u16>,
) -> String {
    let limit = limit.to_string();
    let window = window_ms.map_or_else(|| "-".to_owned(), |ms| ms.to_string());
    let status = status.map_or_else(|| "-".to_owned(), |s| s.to_string());
    key(
        "route",
        &[
            route.service,
            route.method,
            route.pattern,
            identity,
            &limit,
            &window,
            &status,
        ],
    )
}

/// A window in seconds as whole milliseconds, truncated like the reference's
/// `Math.floor(ttl * 1000)` (RL-04). `None` when that is not a usable expiry (below 1 ms,
/// not finite, or beyond Redis's range), where the reference's Redis call fails.
pub(crate) fn window_ms(ttl: f64) -> Option<u64> {
    let ms = (ttl * 1000.0).floor();
    if (1.0..9.0e15).contains(&ms) {
        Some(ms as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rl_04_windows_are_truncated_to_whole_milliseconds() {
        assert_eq!(window_ms(60.0), Some(60_000));
        assert_eq!(window_ms(0.5), Some(500));
        assert_eq!(window_ms(1.9999), Some(1999));
        assert_eq!(window_ms(0.0015), Some(1));
        assert_eq!(window_ms(1.0005), Some(1000));
        assert_eq!(window_ms(0.0009), None);
        assert_eq!(window_ms(0.0), None);
        assert_eq!(window_ms(-1.0), None);
        assert_eq!(window_ms(f64::NAN), None);
        assert_eq!(window_ms(f64::INFINITY), None);
    }

    #[test]
    fn keys_cannot_collide_across_parts() {
        let a = RouteKey::new("svc", "GET", "/a:1:b");
        let b = RouteKey::new("svc", "GET", "/a");
        assert_ne!(
            route_key(&a, "x", 1, Some(1000), None),
            route_key(&b, "1:b:x", 1, Some(1000), None)
        );
        assert_ne!(global_ip_key("1.2.3.4"), anonymous_ip_key("1.2.3.4"));
        assert_ne!(custom_key("ip"), global_ip_key("ip"));
    }

    #[test]
    fn rl_17_route_keys_differ_by_every_field() {
        let route = RouteKey::new("svc", "GET", "/v1/x");
        let base = route_key(&route, "id", 2, Some(60_000), None);
        for other in [
            route_key(
                &RouteKey::new("other", "GET", "/v1/x"),
                "id",
                2,
                Some(60_000),
                None,
            ),
            route_key(
                &RouteKey::new("svc", "POST", "/v1/x"),
                "id",
                2,
                Some(60_000),
                None,
            ),
            route_key(
                &RouteKey::new("svc", "GET", "/v1/y"),
                "id",
                2,
                Some(60_000),
                None,
            ),
            route_key(&route, "id2", 2, Some(60_000), None),
            route_key(&route, "id", 3, Some(60_000), None),
            route_key(&route, "id", 2, Some(1_000), None),
            route_key(&route, "id", 2, Some(60_000), Some(201)),
        ] {
            assert_ne!(base, other);
        }
    }
}
