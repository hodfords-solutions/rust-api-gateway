//! Helpers shared by scenarios.

use anyhow::Result;
use conformance::client::Response;
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::MockUpstream;
use conformance::runner::{Gateway, Target, TargetKind};
use serde_json::{Value, json};

/// A mock serving `users-service.json`, and a gateway with it as service `svc`.
pub async fn users_gateway(
    target: &Target,
    configure: impl FnOnce(GatewayConfig) -> GatewayConfig,
) -> Result<(MockUpstream, Gateway)> {
    let mock = MockUpstream::start().await?;
    mock.set_document_json(fixtures::USERS_SERVICE);
    let gateway = target
        .start(configure(GatewayConfig::new().service("svc", &mock)))
        .await?;
    Ok((mock, gateway))
}

/// Deviation D11 (FWD-15, REQ-23): the reference puts `x-powered-by: Express` first in every
/// response the upstream did not give its own `x-powered-by`; the Rust target sends none (it
/// is off by default, `GatewayBuilder::x_powered_by`, tested Rust-only). Checks that for the
/// target and returns the other header lines, which both targets must still send alike.
/// `lines` are `name: value` with names lower-cased.
pub fn without_x_powered_by(kind: TargetKind, label: &str, mut lines: Vec<String>) -> Vec<String> {
    const EXPRESS: &str = "x-powered-by: Express";
    match kind {
        TargetKind::Reference => {
            assert_eq!(
                lines.first().map(String::as_str),
                Some(EXPRESS),
                "{label}: the reference sends {EXPRESS} first"
            );
            lines.remove(0);
        }
        TargetKind::Rust => {
            assert!(
                !lines.iter().any(|l| l.starts_with("x-powered-by:")),
                "{label}: no x-powered-by under Deviation D11: {lines:?}"
            );
        }
    }
    lines
}

/// The parts of an error response both targets must reproduce.
pub fn error_view(response: &Response) -> Value {
    json!({
        "status": response.status,
        "contentType": response.header("content-type"),
        "body": response.text(),
    })
}

/// The `x-test-route` header the protocol middleware forwarded, parsed.
pub fn matched_route(request: &conformance::mock::RecordedRequest) -> Value {
    request
        .header("x-test-route")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(Value::Null)
}

/// Compares an observed table with the expected one, printing the observed table as a Rust
/// literal on mismatch so it can be reviewed and pasted.
pub fn assert_table(label: &str, observed: &[(String, String)], expected: &[(&str, &str)]) {
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    if observed != expected.as_slice() {
        let literal: Vec<String> = observed
            .iter()
            .map(|(a, b)| format!("        ({a:?}, {b:?}),"))
            .collect();
        panic!(
            "{label}: observed table differs from the expected one. Observed:\n{}",
            literal.join("\n")
        );
    }
}
