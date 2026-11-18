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
#[derive(Debug, Clone)]
pub struct HttpDocumentFetcher {
    client: reqwest::Client,
}

impl HttpDocumentFetcher {
    /// A fetcher without a timeout, as the reference has none.
    pub fn new() -> Result<Self, FetchError> {
        Self::build(None)
    }

    /// A fetcher whose requests fail after `timeout`.
    pub fn with_timeout(timeout: Duration) -> Result<Self, FetchError> {
        Self::build(Some(timeout))
    }

    fn build(timeout: Option<Duration>) -> Result<Self, FetchError> {
        let mut builder =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(21));
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder
            .build()
            .map_err(|error| FetchError::new(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self { client })
    }
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
                .map_err(|error| FetchError::new(error.to_string()))?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|error| FetchError::new(error.to_string()))?;
            Ok(FetchedDocument {
                status,
                body: body.to_vec(),
            })
        })
    }
}
