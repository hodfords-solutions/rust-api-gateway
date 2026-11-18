//! The Redis connection behind the Buckets (CFG-09, CFG-17, Deviation D10).
//!
//! One multiplexed connection is shared by all requests. It is opened on first use (never
//! when rate limiting is disabled, RL-01), replaced as soon as it closes or fails, and every
//! operation, including any reconnection it triggers, is bounded by
//! [`REDIS_OPERATION_TIMEOUT`].

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, RedisConnectionInfo, RedisError, Script};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;

use super::bucket::CHECK_AND_COUNT;

/// The longest a single Redis operation may take, including connecting or reconnecting to
/// Redis, before it fails with [`StoreError::Timeout`] (Deviation D10).
///
/// When Redis cannot be reached, the first Bucket operation of a request fails within this
/// bound and the request is answered with 500 (REQ-24); later operations of that request do
/// not run. A refused connection fails at once. The bound leaves the pipeline headroom under
/// the 2 seconds D10 allows for the whole request.
pub const REDIS_OPERATION_TIMEOUT: Duration = Duration::from_millis(1500);

/// How to reach Redis: the port of the reference's `RedisOptionType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisOptions {
    /// Host name or IP address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Database number. `None` means 0.
    pub db: Option<i64>,
    /// ACL user name. Only sent together with `password` (`AUTH <username> <password>`).
    pub username: Option<String>,
    /// Password. When set, the connection authenticates with `AUTH`.
    pub password: Option<String>,
    /// Whether and how to use TLS.
    pub tls: RedisTls,
}

impl RedisOptions {
    /// Plain TCP to `host:port`, database 0, no authentication.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            db: None,
            username: None,
            password: None,
            tls: RedisTls::Disabled,
        }
    }
}

/// TLS for the Redis connection (the reference's `tls?: boolean | ConnectionOptions`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RedisTls {
    /// `tls` absent or `false`: plain TCP.
    #[default]
    Disabled,
    /// `tls: true`: TLS with default settings, verifying the server certificate against
    /// Mozilla's root certificates (as Node's bundled store does) and the host name.
    Default,
    /// `tls: { … }`: TLS with the given settings.
    Custom(RedisTlsOptions),
}

/// TLS settings (the subset of Node's `tls.connect` options the reference documents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisTlsOptions {
    /// `ca`: PEM certificates to trust instead of the default roots.
    pub ca: Option<Vec<u8>>,
    /// `cert`: PEM client certificate chain, for servers that require client
    /// authentication. Requires `key`.
    pub cert: Option<Vec<u8>>,
    /// `key`: PEM private key of `cert`.
    pub key: Option<Vec<u8>>,
    /// `servername`: the name sent with SNI and checked against the server certificate,
    /// instead of `host`.
    pub server_name: Option<String>,
    /// `rejectUnauthorized`: when false, the server certificate is not verified at all.
    pub reject_unauthorized: bool,
}

impl Default for RedisTlsOptions {
    fn default() -> Self {
        Self {
            ca: None,
            cert: None,
            key: None,
            server_name: None,
            reject_unauthorized: true,
        }
    }
}

/// Invalid [`RedisOptions`], found when the rate limiter is built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RedisConfigError {
    /// A certificate (`ca` or `cert`) is not valid PEM, or contains no certificate.
    #[error("invalid Redis TLS certificate ({field}): {reason}")]
    Certificate {
        /// `ca` or `cert`.
        field: &'static str,
        /// What is wrong.
        reason: String,
    },
    /// `key` is not a valid PEM private key.
    #[error("invalid Redis TLS private key: {0}")]
    PrivateKey(String),
    /// Only one of `cert` and `key` is set.
    #[error("Redis TLS client authentication needs both cert and key")]
    ClientAuthIncomplete,
    /// The server name (`server_name`, or `host` when it is unset) cannot be used for TLS.
    #[error("invalid Redis TLS server name: {0}")]
    ServerName(String),
    /// rustls rejected the TLS configuration.
    #[error("invalid Redis TLS configuration: {0}")]
    Tls(String),
}

/// A failed Redis operation. The pipeline answers 500 (REQ-24).
#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    /// The operation did not finish within [`REDIS_OPERATION_TIMEOUT`].
    #[error("Redis did not answer within {0:?}")]
    Timeout(Duration),
    /// Connecting to Redis failed.
    #[error("cannot connect to Redis: {0}")]
    Connect(String),
    /// Redis answered with an error, or the connection failed mid-command.
    #[error("Redis command failed: {0}")]
    Command(String),
}

struct Target {
    host: String,
    port: u16,
    info: RedisConnectionInfo,
    tls: Option<(TlsConnector, ServerName<'static>)>,
}

struct Live {
    id: u64,
    connection: MultiplexedConnection,
    alive: Arc<AtomicBool>,
}

pub(crate) struct Store {
    target: Target,
    slot: Mutex<Option<Live>>,
    next_id: AtomicU64,
    check_and_count: Script,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("host", &self.target.host)
            .field("port", &self.target.port)
            .field("tls", &self.target.tls.is_some())
            .finish_non_exhaustive()
    }
}

impl Store {
    pub(crate) fn new(options: &RedisOptions) -> Result<Self, RedisConfigError> {
        let mut info = RedisConnectionInfo::default()
            .set_db(options.db.unwrap_or(0))
            .set_skip_set_lib_name();
        if let Some(username) = &options.username {
            info = info.set_username(username);
        }
        if let Some(password) = &options.password {
            info = info.set_password(password);
        }
        let tls = match &options.tls {
            RedisTls::Disabled => None,
            RedisTls::Default => Some(tls_connector(&options.host, &RedisTlsOptions::default())?),
            RedisTls::Custom(tls) => Some(tls_connector(&options.host, tls)?),
        };
        Ok(Self {
            target: Target {
                host: options.host.clone(),
                port: options.port,
                info,
                tls,
            },
            slot: Mutex::new(None),
            next_id: AtomicU64::new(1),
            check_and_count: Script::new(CHECK_AND_COUNT),
        })
    }

    /// Runs the check-and-count script. `true` when the request is admitted.
    pub(crate) async fn check_and_count(
        &self,
        key: &str,
        limit: u64,
        window_ms: u64,
    ) -> Result<bool, StoreError> {
        let rejected: i64 = self
            .run(|mut connection| async move {
                self.check_and_count
                    .key(key)
                    .arg(limit)
                    .arg(window_ms)
                    .invoke_async(&mut connection)
                    .await
            })
            .await?;
        Ok(rejected == 0)
    }

    /// The current count of a Bucket, 0 when it has no open window.
    pub(crate) async fn count(&self, key: &str) -> Result<u64, StoreError> {
        let count: Option<i64> = self
            .run(|mut connection| async move {
                redis::cmd("GET")
                    .arg(key)
                    .query_async(&mut connection)
                    .await
            })
            .await?;
        Ok(count.map_or(0, |c| c.max(0) as u64))
    }

    /// Runs `operation` on the shared connection within [`REDIS_OPERATION_TIMEOUT`]. A
    /// connection that failed is dropped, and the operation is tried once more on a fresh
    /// one, so the first request after Redis restarts succeeds (CFG-17).
    async fn run<T, F, Fut>(&self, operation: F) -> Result<T, StoreError>
    where
        F: Fn(MultiplexedConnection) -> Fut,
        Fut: Future<Output = Result<T, RedisError>>,
    {
        let used = AtomicU64::new(0);
        let attempt = async {
            let mut retried = false;
            loop {
                let (id, connection) = self.connection().await?;
                used.store(id, Ordering::Relaxed);
                match operation(connection).await {
                    Ok(value) => return Ok(value),
                    Err(error) if is_connection_failure(&error) => {
                        self.invalidate(id).await;
                        if retried {
                            return Err(StoreError::Command(error.to_string()));
                        }
                        tracing::debug!(%error, "Redis connection failed; retrying on a new one");
                        retried = true;
                    }
                    Err(error) => return Err(StoreError::Command(error.to_string())),
                }
            }
        };
        match tokio::time::timeout(REDIS_OPERATION_TIMEOUT, attempt).await {
            Ok(result) => {
                if let Err(error) = &result {
                    tracing::warn!(%error, "rate-limit Redis operation failed");
                }
                result
            }
            Err(_) => {
                // The connection may be half-open (a peer that vanished without closing):
                // drop it so the next operation starts afresh.
                self.invalidate(used.load(Ordering::Relaxed)).await;
                let error = StoreError::Timeout(REDIS_OPERATION_TIMEOUT);
                tracing::warn!(%error, "rate-limit Redis operation failed");
                Err(error)
            }
        }
    }

    async fn connection(&self) -> Result<(u64, MultiplexedConnection), StoreError> {
        let mut slot = self.slot.lock().await;
        if let Some(live) = slot.as_ref()
            && live.alive.load(Ordering::Acquire)
        {
            return Ok((live.id, live.connection.clone()));
        }
        *slot = None;
        let (connection, alive) = self.connect().await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Live {
            id,
            connection: connection.clone(),
            alive,
        });
        Ok((id, connection))
    }

    async fn invalidate(&self, id: u64) {
        let mut slot = self.slot.lock().await;
        if slot.as_ref().is_some_and(|live| live.id == id) {
            *slot = None;
        }
    }

    async fn connect(&self) -> Result<(MultiplexedConnection, Arc<AtomicBool>), StoreError> {
        let target = &self.target;
        let tcp = TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| StoreError::Connect(e.to_string()))?;
        // Small request/response exchanges: do not wait to coalesce writes.
        let _ = tcp.set_nodelay(true);
        match &target.tls {
            None => self.establish(tcp).await,
            Some((connector, server_name)) => {
                let tls = connector
                    .connect(server_name.clone(), tcp)
                    .await
                    .map_err(|e| StoreError::Connect(format!("TLS: {e}")))?;
                self.establish(tls).await
            }
        }
    }

    async fn establish<C>(
        &self,
        stream: C,
    ) -> Result<(MultiplexedConnection, Arc<AtomicBool>), StoreError>
    where
        C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(None)
            .set_response_timeout(None);
        let (connection, driver) =
            MultiplexedConnection::new_with_config(&self.target.info, stream, config)
                .await
                .map_err(|e| StoreError::Connect(e.to_string()))?;
        let alive = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&alive);
        tokio::spawn(async move {
            driver.await;
            flag.store(false, Ordering::Release);
        });
        Ok((connection, alive))
    }
}

fn is_connection_failure(error: &RedisError) -> bool {
    error.is_io_error()
        || error.is_connection_dropped()
        || error.is_connection_refusal()
        || error.is_unrecoverable_error()
}

fn tls_connector(
    host: &str,
    options: &RedisTlsOptions,
) -> Result<(TlsConnector, ServerName<'static>), RedisConfigError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| RedisConfigError::Tls(e.to_string()))?;
    let builder = if options.reject_unauthorized {
        let mut roots = RootCertStore::empty();
        match &options.ca {
            Some(pem) => {
                for certificate in parse_certificates("ca", pem)? {
                    roots
                        .add(certificate)
                        .map_err(|e| RedisConfigError::Certificate {
                            field: "ca",
                            reason: e.to_string(),
                        })?;
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        builder.with_root_certificates(roots)
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCertificate(provider)))
    };
    let config = match (&options.cert, &options.key) {
        (None, None) => builder.with_no_client_auth(),
        (Some(cert), Some(key)) => {
            let chain = parse_certificates("cert", cert)?;
            let key = PrivateKeyDer::from_pem_slice(key)
                .map_err(|e| RedisConfigError::PrivateKey(e.to_string()))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| RedisConfigError::Tls(e.to_string()))?
        }
        _ => return Err(RedisConfigError::ClientAuthIncomplete),
    };
    let name = options.server_name.as_deref().unwrap_or(host).to_owned();
    let server_name = ServerName::try_from(name.clone())
        .map_err(|e| RedisConfigError::ServerName(format!("{name}: {e}")))?;
    Ok((TlsConnector::from(Arc::new(config)), server_name))
}

fn parse_certificates(
    field: &'static str,
    pem: &[u8],
) -> Result<Vec<CertificateDer<'static>>, RedisConfigError> {
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RedisConfigError::Certificate {
            field,
            reason: e.to_string(),
        })?;
    if certificates.is_empty() {
        return Err(RedisConfigError::Certificate {
            field,
            reason: "no PEM certificate found".to_owned(),
        });
    }
    Ok(certificates)
}

/// `rejectUnauthorized: false`: any server certificate is accepted. Handshake signatures are
/// still checked, as Node does.
#[derive(Debug)]
struct AcceptAnyServerCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
