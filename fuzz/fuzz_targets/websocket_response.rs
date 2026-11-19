//! The upstream's answer to a proxied WebSocket upgrade (WS-10 to WS-15): its head as Node's
//! client parses it. Property: the refusal status line written to the client is a single
//! line. (Since Deviation D15 a refused upgrade relays no body, so there is none to decode.)
#![no_main]

use api_gateway::fuzzing::upstream_response;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The head ends at the first empty line, as the proxy reads it.
    let head = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => &data[..at + 4],
        None => data,
    };
    let Some(response) = upstream_response(head) else {
        return;
    };
    assert!(
        !response.refusal_line.contains(['\r', '\n']),
        "refusal status line spans several lines: {:?}",
        response.refusal_line
    );
});
