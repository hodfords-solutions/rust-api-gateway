//! Security tests: rate-limit bypasses (`docs/security/attack-tests.md`).
//!
//! They need Redis and follow the `rate_limit_common` convention: `SKIP` without it, a
//! failure under `RATE_LIMIT_REQUIRE_REDIS=1`. Every test gets its own Service Prefix (Route
//! Buckets are scoped by service, Deviation D2) or its own client address, so runs and
//! parallel tests never share a Bucket.

mod gateway_common;
mod rate_limit_common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use api_gateway::gateway::{Gateway, GatewayBuilder};
use api_gateway::handler::{BoxError, IdentityContext, IdentityResolver, async_trait};
use api_gateway::rate_limit::{BucketLimit, Identity, RateLimitOptions, TrustProxy};
use gateway_common::{Served, Upstream, options, parse_responses, raw_send, serve};
use rate_limit_common::nonce;

/// A gateway for one service with a fresh prefix, rate limiting on, high IP limits, and the
/// document's `/v1/limited` Route (1 request per 60 s).
async fn limited(
    trust: TrustProxy,
    global_ip: u64,
    customise: impl FnOnce(GatewayBuilder) -> GatewayBuilder,
) -> (Upstream, String, Served) {
    let upstream = Upstream::start().await;
    let prefix = format!("s{}", nonce());
    let mut options = options(vec![upstream.service(&prefix)]);
    options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(global_ip, 60.0),
        BucketLimit::new(1_000_000, 60.0),
    );
    options.rate_limit.trust_proxy = trust;
    options.redis = rate_limit_common::redis_options();
    let served = serve(customise(Gateway::builder(options)), true).await;
    (upstream, prefix, served)
}

async fn status(served: &Served, request: &str) -> u16 {
    let raw = raw_send(served.addr, request.as_bytes(), Duration::from_secs(3)).await;
    parse_responses(&raw).first().map_or(0, |r| r.status)
}

fn get(target: &str, headers: &str) -> String {
    format!("GET {target} HTTP/1.1\r\nHost: gw\r\n{headers}Connection: close\r\n\r\n")
}

/// With the default `trust proxy` (trust nothing), client-address headers cannot give a
/// client a fresh Identity: the Route Bucket keeps counting the connection's peer.
#[tokio::test]
async fn spoofed_client_address_headers_do_not_change_the_identity() {
    require_redis!();
    let (upstream, prefix, served) = limited(TrustProxy::None, 1_000_000, |b| b).await;
    let target = format!("/{prefix}/v1/limited");
    assert_eq!(status(&served, &get(&target, "")).await, 200);
    for (i, header) in [
        "X-Forwarded-For",
        "X-Real-IP",
        "Forwarded",
        "True-Client-IP",
        "CF-Connecting-IP",
        "X-Client-IP",
        "X-Cluster-Client-IP",
    ]
    .iter()
    .enumerate()
    {
        let value = if *header == "Forwarded" {
            format!("for=10.9.8.{i}")
        } else {
            format!("10.9.8.{i}")
        };
        let request = get(&target, &format!("{header}: {value}\r\n"));
        assert_eq!(status(&served, &request).await, 429, "{header}");
    }
    assert_eq!(upstream.seen().len(), 1);
}

/// Trusting one proxy hop, the client address is the entry that proxy appended, so
/// addresses the client prepends to `X-Forwarded-For` change nothing.
#[tokio::test]
async fn trusting_one_hop_ignores_addresses_the_client_prepends() {
    require_redis!();
    let (upstream, prefix, served) = limited(TrustProxy::Hops(1), 1_000_000, |b| b).await;
    let target = format!("/{prefix}/v1/limited");
    let client = format!("client-{}", nonce());
    let mut statuses = Vec::new();
    for spoofed in ["1.1.1.1", "2.2.2.2", "3.3.3.3, 4.4.4.4"] {
        let header = format!("X-Forwarded-For: {spoofed}, {client}\r\n");
        statuses.push(status(&served, &get(&target, &header)).await);
    }
    assert_eq!(statuses, [200, 429, 429]);
    assert_eq!(upstream.seen().len(), 1);
}

/// Spellings of a Route's path (letter case, trailing slash, dot segments, backslashes, a
/// query, a fragment) are the same Route and count against the same Bucket; spellings that
/// are not (percent-encoded letters, doubled slashes, `;` parameters, an encoded slash) match
/// no Route and are never forwarded. No variant reaches the upstream once the Bucket is full.
#[tokio::test]
async fn path_variants_cannot_get_a_fresh_route_bucket() {
    require_redis!();
    let (upstream, prefix, served) = limited(TrustProxy::None, 1_000_000, |b| b).await;
    let p = &prefix;
    assert_eq!(
        status(&served, &get(&format!("/{p}/v1/limited"), "")).await,
        200
    );
    for (variant, expected) in [
        (format!("/{p}/V1/LIMITED"), 429),
        (format!("/{p}/v1/limited/"), 429),
        (format!("/{p}/v1/./limited"), 429),
        (format!("/{p}/v1/x/../limited"), 429),
        (format!("/{p}/v1/%2e/limited"), 429),
        (format!("/{p}\\v1\\limited"), 429),
        (format!("/{p}/v1/limited?cache=bust"), 429),
        (format!("/{p}/v1/limited#x"), 429),
        (format!("/{p}/v1/%6cimited"), 405),
        (format!("/{p}/v1/%6Cimited"), 405),
        (format!("/{p}//v1/limited"), 405),
        (format!("/{p}/v1/limited//"), 405),
        (format!("/{p}/v1/limited;x=1"), 405),
        (format!("/{p}/v1/limited%2f"), 405),
        (format!("/{p}/v1/limited%00"), 405),
    ] {
        assert_eq!(
            status(&served, &get(&variant, "")).await,
            expected,
            "{variant}"
        );
    }
    let method = format!("get /{p}/v1/limited HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n");
    assert_eq!(status(&served, &method).await, 405, "lower-case method");
    assert_eq!(upstream.seen().len(), 1, "{:?}", upstream.seen());
}

/// Identifies a request by `x-user` (standing for a user the middleware verified) as an
/// authenticated Identity, and anyone else by client address, anonymously. Records the
/// client addresses it is given.
struct UserOrAddress(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl IdentityResolver for UserOrAddress {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        self.0.lock().expect("ips").push(context.ip.to_string());
        Ok(Some(match context.request.header("x-user") {
            Some(user) => Identity::authenticated(user),
            None => Identity::anonymous(context.ip),
        }))
    }
}

/// SEC-08: an Identity key is used as it is, whether the Identity is authenticated or not,
/// so an authenticated user whose key equals another client's address (a user name the
/// attacker chose at sign-up, say `127.0.0.1`) spends that client's anonymous Route Buckets,
/// and the reverse.
///
/// Safe behaviour: authenticated and anonymous Identities count against separate Buckets.
#[tokio::test]
#[ignore = "security finding SEC-08: authenticated and anonymous Identities with the same key share Buckets"]
async fn sec_08_authenticated_and_anonymous_identities_do_not_share_buckets() {
    require_redis!();
    let addresses = Arc::new(Mutex::new(Vec::new()));
    let resolver = UserOrAddress(Arc::clone(&addresses));
    let (_upstream, prefix, served) = limited(TrustProxy::None, 1_000_000, |b| {
        b.identity_resolver(resolver)
    })
    .await;
    // Learn the address the anonymous client is keyed by.
    assert_eq!(
        status(&served, &get(&format!("/{prefix}/v1/users"), "")).await,
        200
    );
    let address = addresses.lock().expect("ips")[0].clone();
    let target = format!("/{prefix}/v1/limited");
    let attacker = get(&target, &format!("X-User: {address}\r\n"));
    assert_eq!(status(&served, &attacker).await, 200);
    assert_eq!(
        status(&served, &get(&target, "")).await,
        200,
        "the anonymous client {address} was limited by the user named {address}"
    );
}

/// SEC-01: upgrade requests are not counted against any Bucket (WS-04), so a client whose
/// Global IP Bucket is full keeps reaching the upstream by adding upgrade headers to its
/// requests.
///
/// Safe behaviour: the upgrade is refused with 429 like the plain request.
#[tokio::test]
#[ignore = "security finding SEC-01: upgrade requests are not rate limited"]
async fn sec_01_upgrade_requests_are_rate_limited() {
    require_redis!();
    let (upstream, prefix, served) = limited(TrustProxy::All, 1, |b| b).await;
    let client = format!("X-Forwarded-For: client-{}\r\n", nonce());
    let target = format!("/{prefix}/v1/users");
    assert_eq!(status(&served, &get(&target, &client)).await, 200);
    assert_eq!(status(&served, &get(&target, &client)).await, 429);
    let upgrade = format!(
        "GET {target} HTTP/1.1\r\nHost: gw\r\n{client}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    assert_eq!(status(&served, &upgrade).await, 429);
    assert_eq!(upstream.seen().len(), 1, "{:?}", upstream.seen());
}
