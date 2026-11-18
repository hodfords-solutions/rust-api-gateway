//! The default TLS client settings, shared by every outgoing `https` connection the gateway
//! makes: forwarded requests and MCP tool calls (FWD-19), document fetches (OAS-01) and,
//! with the `otel` feature, span exports (OTEL-01).
//!
//! They trust Mozilla's root certificates, built into the binary (`webpki-roots`), as Node
//! trusts its bundled store. The host's trust store is never read, so a host without CA
//! certificates (a minimal container image) starts the gateway like any other: plain `http`
//! works, and an `https` peer whose certificate those roots do not cover fails on that
//! connection, with the TLS error, instead of at startup. An application that needs other
//! roots passes its own settings with
//! [`GatewayBuilder::upstream_tls`](crate::GatewayBuilder::upstream_tls).

use std::sync::Arc;

/// TLS client settings trusting Mozilla's root certificates.
pub(crate) fn default_client_config() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map(|builder| {
            builder
                .with_root_certificates(roots.clone())
                .with_no_client_auth()
        })
        .unwrap_or_else(|_| {
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        })
}
