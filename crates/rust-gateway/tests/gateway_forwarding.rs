//! Rust-only tests of forwarding: `https` upstreams (FWD-19), startup without a system trust
//! store, and connection pooling (FWD-20, CFG-10).

mod gateway_common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gateway_common::{Upstream, client, options, serve};
use rust_gateway::gateway::{Gateway, PoolOptions};
use rust_gateway::openapi::{DocumentFetcher as _, HttpDocumentFetcher};
use rust_gateway::upstream::UpstreamService;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

const UPSTREAM_CERT: &[u8] = include_bytes!("data/gateway_tls/upstream.pem");
const UPSTREAM_KEY: &[u8] = include_bytes!("data/gateway_tls/upstream.key");

fn certificates() -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(UPSTREAM_CERT)
        .collect::<Result<_, _>>()
        .expect("certificate")
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_tls() -> Arc<rustls::ServerConfig> {
    Arc::new(
        rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(
                certificates(),
                PrivateKeyDer::from_pem_slice(UPSTREAM_KEY).expect("key"),
            )
            .expect("server config"),
    )
}

fn client_tls_trusting_the_upstream() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in certificates() {
        roots.add(certificate).expect("root");
    }
    rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// FWD-19: an Upstream Service may be `https`. Its certificate is verified: with the default
/// roots, a self-signed upstream is unavailable (FWD-22).
#[tokio::test]
async fn fwd_19_https_upstreams() {
    // The document is served over plain HTTP by a second upstream; requests go over TLS.
    let documents = Upstream::start().await;
    let secure = Upstream::start_with(Some(server_tls())).await;
    let mut service = documents.service("svc");
    service.host = secure.origin();
    assert!(service.host.starts_with("https://localhost:"));

    let served = serve(
        Gateway::builder(options(vec![service.clone()]))
            .upstream_tls(client_tls_trusting_the_upstream()),
        true,
    )
    .await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "upstream /v1/users/");
    assert_eq!(secure.seen().len(), 1);
    assert!(documents.seen().is_empty());

    let served = serve(Gateway::builder(options(vec![service])), true).await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"message":"Service unavailable."}"#
    );
    assert_eq!(secure.seen().len(), 1, "nothing reached the upstream");
}

/// Set in the child process of [`startup_needs_no_system_trust_store`].
const WITHOUT_TRUST_STORE: &str = "RUST_GATEWAY_TEST_WITHOUT_TRUST_STORE";

/// FWD-19, OAS-01: a host without CA certificates (a container image without
/// `ca-certificates`) starts the gateway. Runs [`trust_store_absent`] in a child process
/// whose `SSL_CERT_FILE` and `SSL_CERT_DIR` name an empty file and an empty directory: on
/// Linux that is a host with no trust store (what `rustls-native-certs` loads, and what a
/// client verifying with the platform's roots fails to build without); elsewhere the
/// variables change nothing and the same assertions still hold.
#[test]
fn startup_needs_no_system_trust_store() {
    let dir = std::env::temp_dir().join(format!("rust-gateway-no-trust-{}", std::process::id()));
    let certs = dir.join("certs");
    std::fs::create_dir_all(&certs).expect("empty directory");
    let file = dir.join("empty.pem");
    std::fs::write(&file, b"").expect("empty file");
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "trust_store_absent", "--nocapture"])
        .env("SSL_CERT_FILE", &file)
        .env("SSL_CERT_DIR", &certs)
        .env(WITHOUT_TRUST_STORE, "1")
        .output()
        .expect("child test runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
}

/// The child of [`startup_needs_no_system_trust_store`] (does nothing on its own): the
/// gateway builds, plain `http` documents and upstreams work, an `https` upstream or
/// document URL the default roots do not cover fails on its own request or fetch with the
/// TLS error, and the application's TLS settings make both work.
#[tokio::test]
async fn trust_store_absent() {
    if std::env::var_os(WITHOUT_TRUST_STORE).is_none() {
        return;
    }
    let plain = Upstream::start().await;
    let secure = Upstream::start_with(Some(server_tls())).await;
    let mut https_upstream = plain.service("tls");
    https_upstream.host = secure.origin();
    let https_document_url = format!("{}/openapi.json", secure.origin());
    let https_document = UpstreamService::new("doc", https_document_url.as_str(), plain.origin());
    let services = vec![plain.service("svc"), https_upstream, https_document];

    // Default TLS settings: the gateway builds and plain HTTP works.
    let served = serve(Gateway::builder(options(services.clone())), true).await;
    let get = |path: &str| client().get(served.url(path)).send();
    let response = get("/svc/v1/users").await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "upstream /v1/users/");
    // The https upstream fails on its own request (FWD-22).
    let response = get("/tls/v1/users").await.unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"message":"Service unavailable."}"#
    );
    // The https document fails on its own fetch, with the TLS error.
    let mut loaded = served.gateway.documents().loaded_services();
    loaded.sort();
    assert_eq!(loaded, ["svc", "tls"]);
    let error = HttpDocumentFetcher::new()
        .expect("the fetcher builds")
        .fetch(&https_document_url)
        .await
        .expect_err("self-signed");
    assert!(
        error.to_string().contains("invalid peer certificate"),
        "{error}"
    );
    drop(served);

    // The application's TLS settings apply to upstreams and document URLs alike.
    let served = serve(
        Gateway::builder(options(services)).upstream_tls(client_tls_trusting_the_upstream()),
        true,
    )
    .await;
    let mut loaded = served.gateway.documents().loaded_services();
    loaded.sort();
    assert_eq!(loaded, ["doc", "svc", "tls"]);
    for path in ["/tls/v1/users", "/doc/v1/users"] {
        let response = client().get(served.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
    let fetched = HttpDocumentFetcher::with_tls(client_tls_trusting_the_upstream(), None)
        .expect("the fetcher builds")
        .fetch(&https_document_url)
        .await
        .expect("trusted");
    assert_eq!(fetched.status, 200);
}

/// FWD-20 and CFG-10: connections to an Upstream Service are pooled and reused; a
/// connection idle longer than the keep-alive timeout is not reused; the connection cap
/// makes further requests wait.
#[tokio::test]
async fn fwd_20_connections_are_pooled() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.pool = PoolOptions {
        connections: Some(1),
        keep_alive_timeout: Duration::from_millis(300),
        ..PoolOptions::default()
    };
    let served = serve(Gateway::builder(gateway_options), true).await;
    let base = upstream.connections();

    for _ in 0..3 {
        let response = client()
            .get(served.url("/svc/v1/users"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        upstream.connections() - base,
        1,
        "sequential requests share one"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        upstream.connections() - base,
        2,
        "an idle connection expired"
    );

    // `connections: 1`: three concurrent 200 ms requests run one after another.
    let started = Instant::now();
    let url = served.url("/svc/v1/users?delay=200");
    let (a, b, c) = tokio::join!(
        client().get(&url).send(),
        client().get(&url).send(),
        client().get(&url).send()
    );
    for response in [a, b, c] {
        assert_eq!(response.unwrap().status(), 200);
    }
    assert!(
        started.elapsed() >= Duration::from_millis(590),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        upstream.connections() - base,
        2,
        "still one connection at a time"
    );
}

/// CFG-10: `headersTimeout` ends a silent upstream with the FWD-22 503.
#[tokio::test]
async fn cfg_10_headers_timeout_answers_503() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.pool.headers_timeout = Some(Duration::from_millis(200));
    let served = serve(Gateway::builder(gateway_options), true).await;
    let started = Instant::now();
    let response = client()
        .get(served.url("/svc/v1/users?delay=2000"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"message":"Service unavailable."}"#
    );
}
