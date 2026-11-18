//! Conformance harness for `rust-gateway`.
//!
//! - [`mock`]: a controllable Upstream Service that serves OpenAPI fixtures and records requests.
//! - [`client`]: a raw HTTP/1.1 and WebSocket client.
//! - [`runner`]: starts the `reference` or `rust` target and runs scenarios against it.
//! - [`config`]: the configuration file both targets read.
//! - [`mcp`]: helpers for calling the MCP endpoint.
//!
//! See `conformance/README.md` for the test middleware protocol and `docs/development.md`
//! for how to run the suite.

pub mod client;
pub mod config;
pub mod fixtures;
pub mod mcp;
pub mod mock;
pub mod redis;
pub mod runner;
pub mod wire;
