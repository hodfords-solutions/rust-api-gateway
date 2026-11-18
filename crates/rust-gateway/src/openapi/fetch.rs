//! Fetching documents over HTTP, behind a trait so tests can substitute it.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A boxed future, as returned by [`DocumentFetcher::fetch`].
pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchError>> + Send + 'a>>;

/// The response to a document request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedDocument {
    /// The HTTP status.
    pub status: u16,
    /// The body bytes.
    pub body: Vec<u8>,
}

/// A document request that did not produce a response (connection refused or reset,
/// invalid URL, too many redirects, ...).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FetchError {
    message: String,
}

impl FetchError {
    /// An error with a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Performs the `GET` of an Upstream Service's document URL (OAS-01).
pub trait DocumentFetcher: Send + Sync + 'static {
    /// Fetches `url`. Any HTTP status is a response; only transport failures are errors.
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a>;
}

/// The default [`DocumentFetcher`]: a plain `GET` like the reference's axios request,
/// following up to 21 redirects, with no timeout unless one is set.
///
/// `https` URLs are verified with the gateway's TLS settings: by default Mozilla's root
/// certificates, built in (as Node uses its bundled store), never the host's trust store, so
/// building a fetcher does not depend on the host having CA certificates. A certificate the
/// settings do not trust fails that fetch, with the TLS error in the [`FetchError`].
#[derive(Debug, Clone)]
pub struct HttpDocumentFetcher {
    client: reqwest::Client,
}

impl HttpDocumentFetcher {
    /// A fetcher without a timeout, as the reference has none.
    pub fn new() -> Result<Self, FetchError> {
        Self::with_tls(crate::tls::default_client_config(), None)
    }

    /// A fetcher whose requests fail after `timeout`.
    pub fn with_timeout(timeout: Duration) -> Result<Self, FetchError> {
        Self::with_tls(crate::tls::default_client_config(), Some(timeout))
    }

    /// A fetcher that verifies `https` URLs with `tls` (the gateway passes the settings of
    /// [`GatewayBuilder::upstream_tls`](crate::GatewayBuilder::upstream_tls)), and whose
    /// requests fail after `timeout` when one is given.
    pub fn with_tls(
        tls: rustls::ClientConfig,
        timeout: Option<Duration>,
    ) -> Result<Self, FetchError> {
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::limited(21));
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder.build().map_err(|error| {
            FetchError::new(format!(
                "cannot build the HTTP client: {}",
                describe(&error)
            ))
        })?;
        Ok(Self { client })
    }
}

/// An error and its sources, so a TLS failure reads as such ("error sending request for
/// url (…): client error (Connect): invalid peer certificate: UnknownIssuer") rather than
/// as reqwest's outermost message alone.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

impl DocumentFetcher for HttpDocumentFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(url)
                .header("accept", "application/json, text/plain, */*")
                .send()
                .await
                .map_err(|error| FetchError::new(describe(&error)))?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|error| FetchError::new(describe(&error)))?;
            Ok(FetchedDocument {
                status,
                body: body.to_vec(),
            })
        })
    }
}
