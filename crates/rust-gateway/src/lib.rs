//! `rust-gateway` is a Rust port of the `@hodfords/nestjs-api-gateway` library
//! (the Reference Gateway, version 12.4.2).
//!
//! The gateway sits in front of unchanged NestJS Upstream Services. It reads each
//! service's OpenAPI document, builds a Route table from it, and forwards requests,
//! applying middleware and Redis-backed rate limits on the way. Its observable
//! behaviour (the External Contract) must match the Reference Gateway; see
//! `docs/parity/` for the Parity Checklist and `CONTEXT.md` for vocabulary.
//!
//! Start with [`Gateway::builder`]: give it the options ([`GatewayOptions`]) and the
//! handlers ([`handler`]), build it inside a Tokio runtime, then either serve it with
//! [`Gateway::serve`] or mount [`Gateway::router`] in an axum application.

pub mod documents;
mod endpoints;
pub mod error;
pub mod gateway;
pub mod handler;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod openapi;
mod proxy;
pub mod rate_limit;
pub mod request;
pub mod route;
pub mod server;
pub mod upstream;

pub use gateway::{Gateway, GatewayBuilder, GatewayOptions};
