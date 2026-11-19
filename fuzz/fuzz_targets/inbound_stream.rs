//! The connection-level rewriter that runs on raw client bytes before hyper (REQ-02,
//! WS-01): it finds request heads by following the framing, rewrites their targets, and holds
//! upgrade heads back. Properties:
//!
//! - chunk boundaries do not matter: feeding the bytes in pieces gives the same output as
//!   feeding them at once;
//! - every request head hyper would act on in the output has a target the rewriter produced,
//!   so no byte above `0x7e` reaches hyper in a target (a framing disagreement between the
//!   rewriter and hyper would let an unrewritten head through: request smuggling past the
//!   rewrite).
#![no_main]

use api_gateway::fuzzing::inbound;
use api_gateway_fuzz::hyper_heads;
use libfuzzer_sys::fuzz_target;

// Input: one control byte, then the bytes the client sends. The control byte's low bit holds
// upgrade heads back; the rest seeds the chunk sizes of the split feed.
fuzz_target!(|data: &[u8]| {
    let Some((&control, stream)) = data.split_first() else {
        return;
    };
    let hold_upgrades = control & 1 == 1;
    let whole = inbound([stream], hold_upgrades);

    let mut chunks = Vec::new();
    let mut rest = stream;
    let mut state = u32::from(control) | 1;
    while !rest.is_empty() {
        // xorshift: chunk sizes from 1 to 64.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let size = (state as usize % 64 + 1).min(rest.len());
        let (chunk, tail) = rest.split_at(size);
        chunks.push(chunk);
        rest = tail;
    }
    let split = inbound(chunks.iter().copied(), hold_upgrades);
    assert_eq!(whole, split, "output depends on how the bytes were split");

    for head in hyper_heads(&whole.to_hyper) {
        assert!(
            head.target.iter().all(|&b| b <= 0x7e),
            "hyper would accept {} {:?} at byte {} with a non-ASCII target",
            head.method,
            String::from_utf8_lossy(&head.target),
            head.offset
        );
    }
});
