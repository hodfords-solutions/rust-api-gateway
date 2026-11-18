//! The document endpoints (Parity Checklist area 07): `GET /documents`, an HTML page that
//! loads the Scalar API reference, and `GET /document-json?type=<prefix>`, one Upstream
//! Service's OpenAPI document as the gateway publishes it.
//!
//! Both are plain functions over the [`DocumentStore`](crate::openapi::DocumentStore) and a
//! [`DocumentFetcher`](crate::openapi::DocumentFetcher), so a server mounts them with a few
//! lines and tests run them without one:
//!
//! ```ignore
//! let endpoints = DocumentEndpoints::new(store, fetcher, options, scalar_options);
//! // before the proxy, and only when documents are enabled (CFG-14):
//! match DocumentEndpoint::match_request(request.method(), request.uri().path()) {
//!     Some(DocumentEndpoint::Page) => endpoints.page(),
//!     Some(DocumentEndpoint::Json) => endpoints
//!         .document_json(request.uri().query())
//!         .await
//!         .unwrap_or_else(|error| error_renderer.render(&error)),
//!     None => proxy(request).await,
//! }
//! ```
//!
//! Like every Express response, the server adds `X-Powered-By: Express`, a weak `ETag` and
//! `Content-Length`, and answers `HEAD` without the body. The reference applies neither
//! middleware nor rate limits to these endpoints.

mod json;
mod page;
mod transform;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use http::{HeaderValue, Method, Response, StatusCode, header};

pub use page::{HTML_UTF8, render_page};

use crate::error::{GatewayError, JSON_UTF8};
use crate::openapi::{DocumentFetcher, DocumentStore};
use crate::request::parse_query;

/// The path of the page.
pub const DOCUMENTS_PATH: &str = "/documents";

/// The path of the JSON document.
pub const DOCUMENT_JSON_PATH: &str = "/document-json";

/// The options that shape `/document-json` (the reference's `excludeHeaders`,
/// `openApiSecurityKeys` and `restful.hideDocumentIds`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentOptions {
    /// Header parameters with one of these names are removed from every operation (DOC-08).
    /// Parameter names are lower-cased before the comparison and the entries are not, so
    /// only lower-case entries take effect.
    pub exclude_headers: Vec<String>,
    /// Security requirements naming one of these schemes name `bearer` instead (DOC-09).
    pub open_api_security_keys: Vec<String>,
    /// A path item with an operation whose `operationId` is listed is removed (DOC-10).
    pub hide_document_ids: Vec<String>,
}

/// The fetched body is not a JSON object, so there is no document to serve. The endpoint
/// answers 500, as the reference does (DOC-13).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the upstream document is not a JSON object")]
pub struct NotADocument;

/// The reference's `getDocument` after the fetch (DOC-05 to DOC-12, DOC-15): the document
/// served for service `prefix`, given the body its document URL answered, as the JSON text
/// Express's `res.json` sends.
///
/// The body is read as axios reads it: UTF-8 (invalid bytes replaced), a leading byte-order
/// mark dropped, then `JSON.parse`. Everything the rows do not change is written back the
/// way `JSON.stringify` writes it, which is not always the upstream's bytes: see the
/// conformance scenario `documents::doc_05_serialisation`.
///
/// Deviation D5: no shape of document makes this fail; parts the reference would throw on
/// are left as they are. A body that is not a JSON object is [`NotADocument`].
pub fn transform_document(
    prefix: &str,
    body: &[u8],
    options: &DocumentOptions,
) -> Result<String, NotADocument> {
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Ok(json::JsValue::Object(mut document)) = json::parse(text) else {
        return Err(NotADocument);
    };
    transform::transform(&mut document, prefix, options);
    Ok(json::stringify(&json::JsValue::Object(document)))
}

/// The value of the `type` query parameter, as Express 5 reads it (`node:querystring`):
/// decoded, and a repeated parameter joined with `,` (an array used as a property key).
/// `None` when the query has no `type`.
pub fn document_type(query: Option<&str>) -> Option<String> {
    let query = query?;
    parse_query(query)
        .into_iter()
        .find(|(key, _)| key == "type")
        .map(|(_, values)| values.join(","))
}

/// `services` in the order the reference lists them: the key order of a plain JavaScript
/// object, so prefixes that are array indices (`0`, `7`, `42`) come first in numeric
/// order, then the rest in the order given (first successful load).
pub fn page_service_order(services: &[String]) -> Vec<String> {
    let index = |prefix: &str| json::JsString::from_str(prefix).array_index();
    let mut indices: Vec<(u32, &String)> = services
        .iter()
        .filter_map(|prefix| index(prefix).map(|i| (i, prefix)))
        .collect();
    indices.sort_unstable_by_key(|&(i, _)| i);
    indices
        .into_iter()
        .map(|(_, prefix)| prefix.clone())
        .chain(
            services
                .iter()
                .filter(|prefix| index(prefix).is_none())
                .cloned(),
        )
        .collect()
}

/// Which document endpoint a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentEndpoint {
    /// `GET /documents`.
    Page,
    /// `GET /document-json`.
    Json,
}

impl DocumentEndpoint {
    /// The endpoint a request is routed to, as Express routes it: `GET` or `HEAD`, the raw
    /// (still percent-encoded) path compared without regard to ASCII case, one trailing
    /// slash allowed. `path` excludes the query. Anything else is not a document request
    /// and goes to the proxy (conformance scenario `documents::doc_01_05_endpoint_paths`).
    pub fn match_request(method: &Method, path: &str) -> Option<Self> {
        if method != Method::GET && method != Method::HEAD {
            return None;
        }
        let path = path.strip_suffix('/').unwrap_or(path);
        if path.eq_ignore_ascii_case(DOCUMENTS_PATH) {
            Some(DocumentEndpoint::Page)
        } else if path.eq_ignore_ascii_case(DOCUMENT_JSON_PATH) {
            Some(DocumentEndpoint::Json)
        } else {
            None
        }
    }
}

/// The two document endpoints over a store and a fetcher.
pub struct DocumentEndpoints {
    store: Arc<DocumentStore>,
    fetcher: Arc<dyn DocumentFetcher>,
    options: DocumentOptions,
    scalar_options: Option<serde_json::Value>,
}

impl std::fmt::Debug for DocumentEndpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentEndpoints")
            .field("store", &self.store)
            .field("options", &self.options)
            .field("scalar_options", &self.scalar_options)
            .finish_non_exhaustive()
    }
}

impl DocumentEndpoints {
    /// Endpoints serving the documents of `store`, fetching them with `fetcher`, shaped by
    /// `options`, with the application's `scalarOptions` spread over the page's own.
    pub fn new(
        store: Arc<DocumentStore>,
        fetcher: Arc<dyn DocumentFetcher>,
        options: DocumentOptions,
        scalar_options: Option<serde_json::Value>,
    ) -> Self {
        Self {
            store,
            fetcher,
            options,
            scalar_options,
        }
    }

    /// `GET /documents` (DOC-01 to DOC-04): 200 `text/html; charset=utf-8` listing every
    /// service with a loaded document.
    pub fn page(&self) -> Response<Vec<u8>> {
        let services = page_service_order(&self.store.loaded_services());
        let html = render_page(&services, self.scalar_options.as_ref());
        respond(HTML_UTF8, html)
    }

    /// `GET /document-json` with the request's query string (DOC-05 to DOC-15): 200
    /// `application/json; charset=utf-8` with the service's document fetched now.
    ///
    /// Errors: [`GatewayError::DocumentNotFound`] when `type` is missing or names no service with a
    /// loaded document (DOC-14, Deviation D5); [`GatewayError::Internal`] when the fetch
    /// fails, answers outside 2xx, or the body is not a JSON object (DOC-13).
    pub async fn document_json(
        &self,
        query: Option<&str>,
    ) -> Result<Response<Vec<u8>>, GatewayError> {
        let prefix = document_type(query).ok_or(GatewayError::DocumentNotFound)?;
        let service = self
            .store
            .document(&prefix)
            .ok_or(GatewayError::DocumentNotFound)?;
        let fetched = self
            .fetcher
            .fetch(&service.doc_url)
            .await
            .map_err(GatewayError::internal)?;
        if !(200..300).contains(&fetched.status) {
            return Err(GatewayError::internal(format!(
                "the document of {prefix:?} answered {}",
                fetched.status
            )));
        }
        let document = transform_document(&prefix, &fetched.body, &self.options)
            .map_err(GatewayError::internal)?;
        Ok(respond(JSON_UTF8, document))
    }
}

fn respond(content_type: &'static str, body: String) -> Response<Vec<u8>> {
    let mut response = Response::new(body.into_bytes());
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
