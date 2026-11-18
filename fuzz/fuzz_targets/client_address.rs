//! Client address derivation for rate limiting (RL-06, RL-07): a `trust proxy` setting
//! compiled with the `ipaddr.js` port, then walked through `X-Forwarded-For`. Properties: the
//! result is the peer or one of the forwarded entries, trusting nothing gives the peer, and
//! `normalize_ip` is idempotent.
//!
//! Input: `trust \0 peer \0 x-forwarded-for \0 address \0 ipv6 subnet` as (lossy) UTF-8.
//! `trust` is `false`, `true`, a hop count, or comma-separated entries; a `peer` that is not
//! an IP address means there is none; a missing `x-forwarded-for` field means no header.
#![no_main]

use std::net::IpAddr;

use api_gateway::rate_limit::{TrustProxy, TrustedProxies, normalize_ip};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut fields = text.split('\0');
    let trust = match fields.next().unwrap_or("") {
        "false" => TrustProxy::None,
        "true" => TrustProxy::All,
        other => match other.parse::<u32>() {
            Ok(hops) => TrustProxy::Hops(hops),
            Err(_) => TrustProxy::from_comma_separated(other),
        },
    };
    let peer: Option<IpAddr> = fields.next().and_then(|p| p.parse().ok());
    let forwarded_for = fields.next();
    let address = fields.next().unwrap_or("");
    let subnet = fields
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(64)
        % 129;

    let normalized = normalize_ip(address, subnet);
    // A masked IPv6 result carries `/<bits>` and is no longer an address: it stays as it is.
    assert_eq!(
        normalize_ip(&normalized, subnet),
        normalized,
        "normalize_ip not idempotent"
    );

    let Ok(proxies) = TrustedProxies::new(&trust) else {
        return;
    };
    let Some(client) = proxies.client_address(peer, forwarded_for) else {
        return;
    };
    let _ = normalize_ip(&client, subnet);
    let peer = peer.map(|ip| ip.to_string());
    if matches!(trust, TrustProxy::None) {
        assert_eq!(
            Some(&client),
            peer.as_ref(),
            "trusting nothing must give the peer"
        );
    }
    let from_header =
        forwarded_for.is_some_and(|xff| xff.split(',').any(|e| e.trim_matches(' ') == client));
    assert!(
        Some(&client) == peer.as_ref() || from_header,
        "client address {client:?} is neither the peer nor a forwarded entry"
    );
});
