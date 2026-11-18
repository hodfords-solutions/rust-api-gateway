//! The loaded documents: the read side of the reference's `OpenApiService` (API-12).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arc_swap::ArcSwapOption;
use serde_json::Value;
use tokio::sync::watch;

use super::build::{BuildError, SecurityKeys, ServiceDocument, build_service_document};
use super::fetch::{DocumentFetcher, FetchError};
use crate::route::{Route, RouteLookupError};
use crate::upstream::UpstreamService;

/// What one load of a document did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The document differed from the last one and its Routes replaced the previous ones.
    Loaded,
    /// The document was identical to the last one, so nothing was rebuilt (OAS-04).
    Unchanged,
}

/// Why a load did not replace the service's Routes. The previous Routes, if any, stay in
/// place (OAS-03).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The request failed.
    #[error("cannot fetch the document: {0}")]
    Fetch(#[from] FetchError),
    /// The response status was not 200.
    #[error("the document request answered {0}")]
    Status(u16),
    /// The Route table could not be built. As in the reference, the original document is
    /// still replaced ([`DocumentStore::original_document`]) and the same document is not
    /// tried again until it changes.
    #[error("cannot build the Routes: {0}")]
    Build(#[from] BuildError),
}

#[derive(Default)]
struct Slot {
    document: ArcSwapOption<ServiceDocument>,
    original: ArcSwapOption<Value>,
    /// The serialised form of the last document applied. Holding the lock also serialises
    /// concurrent applies for the same prefix.
    snapshot: Mutex<Option<String>>,
}

/// The documents and Routes of every Upstream Service, with the readiness signal of
/// CFG-16.
///
/// Reads are lock-free: each service's document is an atomically swapped `Arc`, so a
/// lookup on the request path never waits for a refresh that is rebuilding the table.
///
/// Services are keyed by Service Prefix, as in the reference: two configured services with
/// the same prefix share one slot.
pub struct DocumentStore {
    services: Vec<UpstreamService>,
    slots: HashMap<String, Slot>,
    security: SecurityKeys,
    loaded_order: Mutex<Vec<String>>,
    ready: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The guarded data stays consistent even if a holder panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DocumentStore {
    /// A store for `services`, with no documents yet.
    pub fn new(services: Vec<UpstreamService>, security: SecurityKeys) -> Self {
        let slots = services
            .iter()
            .map(|service| (service.prefix.clone(), Slot::default()))
            .collect();
        Self {
            services,
            slots,
            security,
            loaded_order: Mutex::new(Vec::new()),
            ready: watch::Sender::new(false),
        }
    }

    /// The configured services, in configuration order.
    pub fn services(&self) -> &[UpstreamService] {
        &self.services
    }

    /// The security scheme names Routes are built with.
    pub fn security_keys(&self) -> &SecurityKeys {
        &self.security
    }

    /// The Routes of a service, from its first successful load onward (OAS-05).
    pub fn document(&self, prefix: &str) -> Option<Arc<ServiceDocument>> {
        self.slots.get(prefix)?.document.load_full()
    }

    /// Whether a service has a document. Service selection depends on this (REQ-07).
    pub fn has_document(&self, prefix: &str) -> bool {
        self.slots
            .get(prefix)
            .is_some_and(|slot| slot.document.load().is_some())
    }

    /// The last document fetched for a service, as JSON (the reference's `originDocs`). It
    /// is replaced by every changed document, even one whose Routes could not be built,
    /// and a body that is not JSON is kept as a string.
    pub fn original_document(&self, prefix: &str) -> Option<Arc<Value>> {
        self.slots.get(prefix)?.original.load_full()
    }

    /// Prefixes of the services that have a document, in the order of their first
    /// successful load. The reference's `apiDocs` is a plain object, so its key order puts
    /// prefixes that are array indices (`0`, `7`) first; [`page_service_order`] turns this
    /// list into that order.
    ///
    /// [`page_service_order`]: crate::documents::page_service_order
    pub fn loaded_services(&self) -> Vec<String> {
        lock(&self.loaded_order).clone()
    }

    /// The reference's `getRouterDetail`: the Route for a request to service `prefix`,
    /// where `url` is the request URL with the Service Prefix already removed.
    ///
    /// `Ok(None)` when no Route matches, and also when the service has no document; the
    /// request pipeline checks [`has_document`](Self::has_document) first (in the reference
    /// that case is an unhandled error).
    pub fn route(
        &self,
        prefix: &str,
        method: &str,
        url: &str,
    ) -> Result<Option<Arc<Route>>, RouteLookupError> {
        match self.document(prefix) {
            Some(document) => document.routes.lookup(method, url),
            None => Ok(None),
        }
    }

    /// Whether every service's first load has settled (CFG-16).
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Resolves once every service's first load has settled, successfully or not
    /// (CFG-16). Startup does not wait for it.
    pub async fn ready(&self) {
        let mut receiver = self.ready.subscribe();
        // The sender lives in `self`, so the channel cannot close while this borrow lasts.
        let _ = receiver.wait_for(|ready| *ready).await;
    }

    /// Signals that every service's first load has settled.
    pub fn mark_ready(&self) {
        self.ready.send_replace(true);
    }

    /// The reference's `getServiceDetail`: fetches the service's document and, when it
    /// changed, rebuilds its Routes (OAS-01 to OAS-05).
    pub async fn load(
        &self,
        service: &UpstreamService,
        fetcher: &dyn DocumentFetcher,
    ) -> Result<LoadOutcome, LoadError> {
        let response = fetcher.fetch(&service.doc_url).await?;
        if response.status != 200 {
            return Err(LoadError::Status(response.status));
        }
        // axios parses any body that is JSON and otherwise keeps the text.
        let document = serde_json::from_slice(&response.body).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&response.body).into_owned())
        });
        self.apply(&service.prefix, &service.doc_url, document)
    }

    /// Applies a fetched document to a service: skipped when identical to the last one
    /// (OAS-04); otherwise the original document is replaced and the Routes rebuilt.
    pub fn apply(
        &self,
        prefix: &str,
        doc_url: &str,
        document: Value,
    ) -> Result<LoadOutcome, LoadError> {
        let Some(slot) = self.slots.get(prefix) else {
            return Ok(LoadOutcome::Unchanged);
        };
        // Compare the serialised JSON, as the reference compares `JSON.stringify` output:
        // formatting differences in the body do not count as a change.
        let snapshot = serde_json::to_string(&document).unwrap_or_default();
        let mut last = lock(&slot.snapshot);
        if last.as_deref() == Some(snapshot.as_str()) {
            return Ok(LoadOutcome::Unchanged);
        }
        *last = Some(snapshot);
        let built = build_service_document(doc_url, &document, &self.security);
        slot.original.store(Some(Arc::new(document)));
        let built = built?;
        let first = slot.document.swap(Some(Arc::new(built))).is_none();
        if first {
            lock(&self.loaded_order).push(prefix.to_string());
        }
        Ok(LoadOutcome::Loaded)
    }
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("services", &self.services)
            .field("loaded", &self.loaded_services())
            .field("ready", &self.is_ready())
            .finish_non_exhaustive()
    }
}
