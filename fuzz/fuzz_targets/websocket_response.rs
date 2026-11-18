//! The upstream's answer to a proxied WebSocket upgrade (WS-10 to WS-15): its head as Node's
//! client parses it, and a refusal's body decoded from its framing. Properties: the refusal
//! status line written to the client is a single line, and body decoding terminates without
//! panicking.
#![no_main]

use api_gateway::fuzzing::upstream_response;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The head ends at the first empty line; the rest is what followed on the connection.
    let (head, rest) = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => data.split_at(at + 4),
        None => (data, &[][..]),
    };
    let Some(response) = upstream_response(head, rest) else {
        return;
    };
    assert!(
        !response.refusal_line.contains(['\r', '\n']),
        "refusal status line spans several lines: {:?}",
        response.refusal_line
    );
    if let Ok(body) = &response.body {
        assert!(
            body.len() <= rest.len(),
            "decoded body longer than its encoding"
        );
    }
});
