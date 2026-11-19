//! Upgrade request heads (WS-01) and the request-target rewrite. Properties: a recognised
//! upgrade head has a printable-ASCII target and both `Upgrade` and `Connection: upgrade`;
//! a rewritten target never contains a byte above `0x7e`, and is accepted by the `http` crate
//! whenever the original had no such byte or control character.
#![no_main]

use api_gateway::fuzzing::{rewrite_target, upgrade_head};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(head) = upgrade_head(data) {
        assert!(!head.target.is_empty());
        assert!(head.target.bytes().all(|b| (0x21..=0x7e).contains(&b)));
        assert!(head.headers.iter().any(|(n, _)| n == "upgrade"));
        assert!(head.headers.iter().any(|(n, v)| {
            n == "connection"
                && v.split(|&b| b == b',')
                    .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"upgrade"))
        }));
        assert_ne!(head.method, "CONNECT");
    }

    let rewritten = rewrite_target(data);
    assert!(
        rewritten.iter().all(|&b| b <= 0x7e),
        "byte above 0x7e left in the target"
    );
    let printable = !data.is_empty() && data.iter().all(|&b| (0x21..=0x7e).contains(&b));
    if printable && data.starts_with(b"/") && !data.contains(&b'#') {
        assert!(
            http::uri::PathAndQuery::try_from(rewritten.as_slice()).is_ok(),
            "rewritten target {:?} still rejected by the http crate",
            String::from_utf8_lossy(&rewritten)
        );
    }
});
