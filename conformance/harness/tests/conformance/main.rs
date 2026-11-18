//! Conformance Tests: black-box scenarios run against the `reference` target, the `rust`
//! target, or `both` (`CONFORMANCE_TARGET`). Each scenario names the Parity Checklist IDs
//! it covers. Run with `cargo test -p conformance --features conformance`; see
//! `docs/development.md`.

mod common;
mod deviations;
mod documents;
mod errors;
mod forwarding;
mod mcp;
mod openapi;
mod pipeline;
mod pool;
mod rate_limit;
mod routes;
