//! `rust-gateway` is a Rust port of the `@hodfords/nestjs-api-gateway` library
//! (the Reference Gateway, version 12.4.2).
//!
//! The gateway sits in front of unchanged NestJS Upstream Services. It reads each
//! service's OpenAPI document, builds a Route table from it, and forwards requests,
//! applying middleware and Redis-backed rate limits on the way. Its observable
//! behaviour (the External Contract) must match the Reference Gateway; see
//! `docs/parity/` for the Parity Checklist and `CONTEXT.md` for vocabulary.
//!
//! This crate is pre-parity: no gateway behaviour is implemented yet.
