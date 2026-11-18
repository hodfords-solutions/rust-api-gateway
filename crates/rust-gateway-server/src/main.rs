//! Example binary for `rust-gateway`, mirroring `src/` of the Reference Gateway repository.
//! It is also the `rust` target of the Conformance Tests.
//!
//! - With `GATEWAY_CONFIG` set, it reads that conformance configuration file as
//!   `conformance/reference/src/config.ts` does, registers the test middleware protocol's
//!   handlers (`conformance/README.md`) and serves `GET /__conformance/ready`.
//! - Without it, it runs the reference sample (EX-01 to EX-04, EX-06): configured from the
//!   environment (see [`config::sample_settings`]), with the sample's handlers and routes.
//!
//! Logs go to standard error; `RUST_LOG` filters them (default `info`).

mod config;
mod protocol;
mod sample;

use std::process::ExitCode;

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{HeaderValue, Response, header};
use rust_gateway::Gateway;
use rust_gateway::error::JSON_UTF8;
use serde_json::json;

use crate::config::{ConfigError, ConformanceConfig, Settings};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::env::var_os("NO_COLOR").is_none())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rust-gateway-server: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rust-gateway-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), ConfigError> {
    let conformance = std::env::var("GATEWAY_CONFIG")
        .ok()
        .filter(|p| !p.is_empty());
    let (settings, gateway) = match &conformance {
        Some(path) => {
            let settings = ConformanceConfig::load(path)?.into_settings()?;
            let gateway = conformance_gateway(&settings)?;
            (settings, gateway)
        }
        None => {
            let settings = config::sample_settings()?;
            let gateway = sample_gateway(&settings)?;
            (settings, gateway)
        }
    };
    let listener = rust_gateway::server::bind_dual_stack(settings.port)
        .map_err(|e| format!("listening on port {}: {e}", settings.port))?;
    if conformance.is_some() {
        println!("conformance rust gateway listening on {}", settings.port);
    } else {
        println!("Server is running on {}", settings.port);
    }
    gateway
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// The gateway of the conformance target: one handler of each kind (CFG-19), all at the
/// default priority, implementing the test middleware protocol.
fn conformance_gateway(settings: &Settings) -> Result<Gateway, ConfigError> {
    let mut builder = Gateway::builder(settings.options.clone())
        .proxy_middleware(protocol::TestProxyMiddleware)
        .websocket_middleware(protocol::TestWsMiddleware)
        .static_request(protocol::TestStaticRequest)
        .mcp_authentication(protocol::TestMcpAuthentication);
    if settings.protocol_resolver {
        builder = builder.identity_resolver(protocol::TestIdentityResolver);
    }
    let gateway = builder.build()?;
    let documents = std::sync::Arc::clone(gateway.documents());
    let routes = Router::new().route(
        "/__conformance/ready",
        get(move || {
            let documents = std::sync::Arc::clone(&documents);
            async move {
                documents.ready().await;
                let body = json!({ "services": documents.loaded_services() }).to_string();
                let mut response = Response::new(Body::from(body));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_UTF8));
                response
            }
        }),
    );
    Ok(gateway.with_routes(routes))
}

/// The reference sample's gateway (EX-03, EX-04, EX-06).
fn sample_gateway(settings: &Settings) -> Result<Gateway, ConfigError> {
    Ok(Gateway::builder(settings.options.clone())
        .proxy_middleware(sample::SampleAuthentication)
        .websocket_middleware(sample::SampleWsAuthentication)
        .static_request(sample::SampleStaticRequest)
        .mcp_authentication(sample::SampleMcpAuthentication)
        .identity_resolver(sample::SampleIdentityResolver)
        .routes(sample::routes())
        .build()?)
}
