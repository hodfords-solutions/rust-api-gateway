//! Example binary for `rust-gateway`, mirroring `src/` of the Reference Gateway.
//!
//! It will read the conformance configuration file named by `GATEWAY_CONFIG` and
//! implement the test middleware protocol described in `conformance/README.md`.
//! Until the gateway exists it only reports that it is not implemented.

use std::process::ExitCode;

use rust_gateway as _;

fn main() -> ExitCode {
    eprintln!("rust-gateway-server: the gateway is not implemented yet (pre-parity)");
    ExitCode::FAILURE
}
