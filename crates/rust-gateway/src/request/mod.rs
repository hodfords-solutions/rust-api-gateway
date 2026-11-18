//! The stack-independent core of the request pipeline (Parity Checklist areas 03 and 04).
//!
//! Everything here is a pure function or a plain value type: the server wiring calls these
//! in the pipeline order of REQ-16 and performs all I/O itself. The rules reproduce what the
//! Reference Gateway does, including the behaviour of the Node libraries it relies on (the
//! WHATWG URL parser, Express's router, `node:querystring`, Node's HTTP parser, undici and
//! es-toolkit), checked against golden vectors generated from those libraries.

mod class;
#[cfg(test)]
mod golden;
mod headers;
mod js_object;
mod kebab;
mod path;
mod prefix;
mod proxy_request;
mod query;
mod selection;
mod unicode_tables;
mod url;

pub use class::{
    RequestClass, SUPPORTED_METHODS, classify, is_bypass_request, is_supported_method,
};
pub use headers::{
    BodyLength, ClientHeaders, Framing, HOP_BY_HOP_HEADERS, NodeHeaderValue, X_POWERED_BY,
    node_incoming_headers, node_remote_address, response_headers, strip_excluded_headers,
    upstream_framing, upstream_request_headers,
};
pub(crate) use headers::{is_valid_header_value, latin1};
pub(crate) use js_object::JsObject;
pub use kebab::kebab_case;
pub use path::{upstream_path, upstream_target};
pub use prefix::{matches_prefix_segment, strip_service_prefix};
pub use proxy_request::ProxyRequest;
pub use query::{
    escape as query_escape, forwarded_query, parse as parse_query, stringify as stringify_query,
    unescape as query_unescape,
};
pub use selection::{
    DEFAULT_SERVICE_PREFIX, SelectedService, SelectedVia, Selection, ServiceSelector,
};
pub use url::{
    InvalidUrl, NORMALISATION_BASE, decodes_as_uri_component, malformed_path_segment,
    normalize_request_url,
};
