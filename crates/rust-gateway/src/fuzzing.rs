//! Entry points for the fuzz targets in `fuzz/` (see `docs/security/fuzzing.md`).
//!
//! Compiled only with `--cfg fuzzing`, which `cargo fuzz` sets. These wrap crate-private
//! parsers that see untrusted bytes; they are not part of the public API and carry no
//! stability promise.

use bytes::BytesMut;

use crate::server::{Inbound, UpgradeHead};

/// What [`inbound`] handed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundOutput {
    /// The bytes handed to hyper, including what `finish` flushed at the end.
    pub to_hyper: Vec<u8>,
    /// The upgrade head held back (as [`upgrade_head`] describes it) and the bytes received
    /// after it, when one was held.
    pub held: Option<(UpgradeHeadView, Vec<u8>)>,
}

/// Feeds `chunks` to the connection's inbound rewriter, one `feed` call per chunk, then
/// ends the connection.
pub fn inbound<'a>(
    chunks: impl IntoIterator<Item = &'a [u8]>,
    hold_upgrades: bool,
) -> InboundOutput {
    let mut inbound = if hold_upgrades {
        Inbound::holding_upgrades()
    } else {
        Inbound::default()
    };
    let mut out = BytesMut::new();
    for chunk in chunks {
        inbound.feed(chunk, &mut out);
    }
    let held = if inbound.is_holding() {
        inbound
            .take_held()
            .map(|held| (UpgradeHeadView::from(&held.head), held.early.to_vec()))
    } else {
        None
    };
    inbound.finish(&mut out);
    InboundOutput {
        to_hyper: out.to_vec(),
        held,
    }
}

/// An upgrade request head as the server keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeHeadView {
    /// The method.
    pub method: String,
    /// The request target, as sent.
    pub target: String,
    /// `0` for HTTP/1.0, `1` for HTTP/1.1.
    pub minor_version: u8,
    /// Header names (lower-case) and values, in arrival order.
    pub headers: Vec<(String, Vec<u8>)>,
}

impl From<&UpgradeHead> for UpgradeHeadView {
    fn from(head: &UpgradeHead) -> Self {
        Self {
            method: head.method.as_str().to_string(),
            target: head.target.clone(),
            minor_version: u8::from(head.version == http::Version::HTTP_11),
            headers: head
                .headers
                .iter()
                .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
                .collect(),
        }
    }
}

/// `UpgradeHead::parse`: `None` unless the head is a complete upgrade request Node accepts.
pub fn upgrade_head(head: &[u8]) -> Option<UpgradeHeadView> {
    UpgradeHead::parse(head).map(|head| UpgradeHeadView::from(&head))
}

/// The request-target rewrite applied before hyper parses a head.
pub fn rewrite_target(target: &[u8]) -> Vec<u8> {
    crate::server::rewrite_target(target)
}

pub use crate::proxy::websocket::fuzzing::{UpstreamResponse, upstream_response};

/// `JSON.parse` then `JSON.stringify` as the document endpoint performs them; `None` where
/// `JSON.parse` fails.
pub fn js_json_round_trip(text: &str) -> Option<String> {
    crate::documents::json_round_trip(text)
}
