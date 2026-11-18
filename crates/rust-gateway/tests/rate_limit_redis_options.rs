//! Redis connection options (CFG-09): database, ACL authentication and TLS. TLS is tested
//! through an in-process TLS terminator in front of the plain test Redis, using the
//! certificates in `tests/data/rate_limit_tls/` (server certificate for `redis.test` only).

mod rate_limit_common;

use std::sync::Arc;

use api_gateway::rate_limit::{
    RateLimitError, RateLimiter, RateLimiterConfigError, RedisConfigError, RedisOptions, RedisTls,
    RedisTlsOptions, StoreError,
};
use rate_limit_common::{limiter_on, nonce, options, redis_host, redis_port, state};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

const CA: &[u8] = include_bytes!("data/rate_limit_tls/ca.pem");
const SERVER_CERT: &[u8] = include_bytes!("data/rate_limit_tls/server.pem");
const SERVER_KEY: &[u8] = include_bytes!("data/rate_limit_tls/server.key");
const CLIENT_CERT: &[u8] = include_bytes!("data/rate_limit_tls/client.pem");
const CLIENT_KEY: &[u8] = include_bytes!("data/rate_limit_tls/client.key");

async fn works(limiter: &RateLimiter) -> Result<(), RateLimitError> {
    let mut s = state(limiter, &format!("client-{}", nonce()));
    limiter.count_global_ip(&mut s).await
}

fn certificates(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .expect("certificates")
}

/// Starts a TLS terminator forwarding to the test Redis; returns its port.
async fn tls_terminator(require_client_certificate: bool) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("protocol versions");
    let builder = if require_client_certificate {
        let mut roots = RootCertStore::empty();
        for certificate in certificates(CA) {
            roots.add(certificate).expect("CA");
        }
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .expect("client verifier");
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let config = builder
        .with_single_cert(
            certificates(SERVER_CERT),
            PrivateKeyDer::from_pem_slice(SERVER_KEY).expect("key"),
        )
        .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(socket).await else {
                    return;
                };
                if let Ok(mut redis) =
                    TcpStream::connect((redis_host().as_str(), redis_port())).await
                {
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut redis).await;
                }
            });
        }
    });
    port
}

fn tls_options(port: u16, tls: RedisTls) -> RedisOptions {
    let mut redis = RedisOptions::new("127.0.0.1", port);
    redis.tls = tls;
    redis
}

fn custom(f: impl FnOnce(&mut RedisTlsOptions)) -> RedisTls {
    let mut tls = RedisTlsOptions::default();
    f(&mut tls);
    RedisTls::Custom(tls)
}

fn assert_connect_error(result: Result<(), RateLimitError>) {
    assert!(
        matches!(result, Err(RateLimitError::Store(StoreError::Connect(_)))),
        "{result:?}"
    );
}

/// CFG-09: TLS with a custom CA and a server name, or without verification.
#[tokio::test]
async fn cfg_09_tls_server_verification() {
    require_redis!();
    let port = tls_terminator(false).await;
    let connect = |tls: RedisTls| limiter_on(&options(), &tls_options(port, tls));

    // Custom CA, server name matching the certificate.
    let ok = connect(custom(|t| {
        t.ca = Some(CA.to_vec());
        t.server_name = Some("redis.test".into());
    }));
    works(&ok).await.expect("verified");

    // Custom CA, but the certificate does not name the host (127.0.0.1).
    assert_connect_error(works(&connect(custom(|t| t.ca = Some(CA.to_vec())))).await);
    // Custom CA, wrong server name.
    assert_connect_error(
        works(&connect(custom(|t| {
            t.ca = Some(CA.to_vec());
            t.server_name = Some("other.test".into());
        })))
        .await,
    );
    // Default settings: the test CA is not a public root.
    assert_connect_error(works(&connect(RedisTls::Default)).await);
    // Verification off.
    works(&connect(custom(|t| t.reject_unauthorized = false)))
        .await
        .expect("unverified");
    // Plain TCP to a TLS port fails too.
    assert!(works(&connect(RedisTls::Disabled)).await.is_err());
}

/// CFG-09: a client certificate for servers that require one.
#[tokio::test]
async fn cfg_09_tls_client_certificate() {
    require_redis!();
    let port = tls_terminator(true).await;
    let connect = |tls: RedisTls| limiter_on(&options(), &tls_options(port, tls));
    works(&connect(custom(|t| {
        t.ca = Some(CA.to_vec());
        t.server_name = Some("redis.test".into());
        t.cert = Some(CLIENT_CERT.to_vec());
        t.key = Some(CLIENT_KEY.to_vec());
    })))
    .await
    .expect("client certificate accepted");
    // TLS 1.3 reports a missing client certificate after the handshake, on first use.
    assert!(
        works(&connect(custom(|t| {
            t.ca = Some(CA.to_vec());
            t.server_name = Some("redis.test".into());
        })))
        .await
        .is_err()
    );
}

/// CFG-09: invalid TLS settings fail construction.
#[test]
fn cfg_09_invalid_tls_settings() {
    let build = |tls: RedisTls| {
        RateLimiter::new(&options(), &tls_options(6379, tls))
            .map(|_| ())
            .map_err(|e| match e {
                RateLimiterConfigError::Redis(e) => e,
                other => panic!("unexpected {other}"),
            })
    };
    assert_eq!(
        build(custom(|t| t.cert = Some(CLIENT_CERT.to_vec()))),
        Err(RedisConfigError::ClientAuthIncomplete)
    );
    assert!(matches!(
        build(custom(|t| t.ca = Some(b"not a certificate".to_vec()))),
        Err(RedisConfigError::Certificate { field: "ca", .. })
    ));
    assert!(matches!(
        build(custom(|t| {
            t.cert = Some(CLIENT_CERT.to_vec());
            t.key = Some(b"nope".to_vec());
        })),
        Err(RedisConfigError::PrivateKey(_))
    ));
    assert!(matches!(
        build(custom(|t| t.server_name = Some("not a name!".into()))),
        Err(RedisConfigError::ServerName(_))
    ));
    assert_eq!(build(RedisTls::Default), Ok(()));
}

/// CFG-09: the database number selects where Buckets live.
#[tokio::test]
async fn cfg_09_database() {
    require_redis!();
    let mut redis = RedisOptions::new(redis_host(), redis_port());
    redis.db = Some(127);
    let l = limiter_on(&options(), &redis);
    let client = format!("client-{}", nonce());
    let mut s = state(&l, &client);
    l.count_global_ip(&mut s).await.expect("counted");

    let raw = redis::Client::open(format!("redis://{}:{}/127", redis_host(), redis_port()))
        .expect("client");
    let mut raw = raw
        .get_multiplexed_async_connection()
        .await
        .expect("connect");
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("*{client}*"))
        .query_async(&mut raw)
        .await
        .expect("KEYS");
    assert_eq!(keys.len(), 1, "the Bucket is in database 127");
    let _: () = redis::cmd("DEL")
        .arg(&keys)
        .query_async(&mut raw)
        .await
        .expect("DEL");
}

/// CFG-09: ACL user name and password.
#[tokio::test]
async fn cfg_09_username_and_password() {
    require_redis!();
    let user = format!("rgw-test-{}", nonce());
    let mut raw = rate_limit_common::raw_redis().await;
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user)
        .arg("on")
        .arg(">secret")
        .arg("~*")
        .arg("+@all")
        .query_async(&mut raw)
        .await
        .expect("ACL SETUSER");

    let with = |password: &str| {
        let mut redis = RedisOptions::new(redis_host(), redis_port());
        redis.username = Some(user.clone());
        redis.password = Some(password.to_owned());
        limiter_on(&options(), &redis)
    };
    let good = works(&with("secret")).await;
    let bad = works(&with("wrong")).await;

    let _: () = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(&user)
        .query_async(&mut raw)
        .await
        .expect("ACL DELUSER");
    good.expect("authenticated");
    assert_connect_error(bad);
}
