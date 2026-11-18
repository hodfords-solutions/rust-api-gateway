//! OpenAPI ingestion: fetching each Upstream Service's document, building its Route table,
//! and keeping both current.
//!
//! - [`DocumentStore`] holds the loaded documents and Routes and answers lookups (the
//!   reference's `OpenApiService`, API-12).
//! - [`DocumentLoader`] fetches every document at startup without blocking it, signals
//!   readiness, and refreshes every [`DOCUMENT_REFRESH_INTERVAL`] (OAS-01 to OAS-05,
//!   CFG-16).
//! - [`build_service_document`] turns one document into a [`RouteTable`](crate::route::RouteTable)
//!   (OAS-06 to OAS-17, OAS-23).
//! - [`DocumentFetcher`] is the HTTP seam; [`HttpDocumentFetcher`] is the default.

pub mod build;
pub mod camelcase;
pub mod fetch;
pub mod loader;
pub mod store;

pub use build::{
    BuildError, SecurityKeys, ServiceDocument, build_service_document, convert_to_express_path,
};
pub use fetch::{DocumentFetcher, FetchError, FetchFuture, FetchedDocument, HttpDocumentFetcher};
pub use loader::{DOCUMENT_REFRESH_INTERVAL, DocumentLoader, LoaderHandle};
pub use store::{DocumentStore, LoadError, LoadOutcome};

#[cfg(test)]
mod tests;
