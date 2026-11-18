//! Loading documents at startup and refreshing them: the reference's
//! `ProxyService.onModuleInit` (document part) and `UpdateApiDocumentTask`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::{JoinHandle, JoinSet};

use super::fetch::DocumentFetcher;
use super::store::DocumentStore;
use crate::upstream::UpstreamService;

/// How often every document is fetched again (OAS-02). The reference runs a cron job
/// every 10 seconds, at wall-clock seconds divisible by 10.
pub const DOCUMENT_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Starts the initial load of every document and the periodic refresh.
pub struct DocumentLoader {
    store: Arc<DocumentStore>,
    fetcher: Arc<dyn DocumentFetcher>,
    refresh_interval: Duration,
}

impl DocumentLoader {
    /// A loader for the services of `store`, refreshing every
    /// [`DOCUMENT_REFRESH_INTERVAL`].
    pub fn new(store: Arc<DocumentStore>, fetcher: Arc<dyn DocumentFetcher>) -> Self {
        Self {
            store,
            fetcher,
            refresh_interval: DOCUMENT_REFRESH_INTERVAL,
        }
    }

    /// Overrides the refresh interval (tests use a short one). A zero interval is raised to
    /// one millisecond.
    pub fn refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval.max(Duration::from_millis(1));
        self
    }

    /// Spawns the loads on the current Tokio runtime and returns at once (CFG-16):
    ///
    /// - every service's document is fetched concurrently; when all of these first loads
    ///   have settled, successfully or not, [`DocumentStore::ready`] resolves (OAS-01);
    /// - at every multiple of the refresh interval since the Unix epoch, every document is
    ///   fetched again, each in its own task, without waiting for earlier fetches
    ///   (OAS-02, OAS-03).
    ///
    /// Failures are logged with `tracing` and leave the previous document in place.
    /// Dropping the returned handle stops the loader.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn start(self) -> LoaderHandle {
        let store = Arc::clone(&self.store);
        let fetcher = Arc::clone(&self.fetcher);
        let initial_task = tokio::spawn(async move {
            // Dropped with this task, which aborts loads still in flight.
            let mut loads = JoinSet::new();
            for service in store.services().iter().cloned() {
                let store = Arc::clone(&store);
                let fetcher = Arc::clone(&fetcher);
                loads.spawn(async move {
                    match store.load(&service, fetcher.as_ref()).await {
                        Ok(_) => tracing::info!(prefix = %service.prefix, "Start {} successfully.", service.prefix),
                        Err(error) => tracing::error!(
                            prefix = %service.prefix,
                            "Start {} failed. {error}",
                            service.prefix
                        ),
                    }
                });
            }
            // A load that panicked has settled too.
            while loads.join_next().await.is_some() {}
            store.mark_ready();
        });
        let refresh_task = tokio::spawn(refresh(self.store, self.fetcher, self.refresh_interval));
        LoaderHandle {
            tasks: vec![initial_task, refresh_task],
        }
    }
}

impl std::fmt::Debug for DocumentLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentLoader")
            .field("store", &self.store)
            .field("refresh_interval", &self.refresh_interval)
            .finish_non_exhaustive()
    }
}

/// The time until the next multiple of `interval` since the Unix epoch.
fn until_next_tick(interval: Duration) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let period = interval.as_nanos().max(1);
    let remaining = period - now % period;
    Duration::from_nanos(u64::try_from(remaining).unwrap_or(u64::MAX))
}

async fn refresh(store: Arc<DocumentStore>, fetcher: Arc<dyn DocumentFetcher>, interval: Duration) {
    // Dropped with this task, which aborts fetches still in flight.
    let mut in_flight = JoinSet::new();
    loop {
        tokio::time::sleep(until_next_tick(interval)).await;
        while in_flight.try_join_next().is_some() {}
        for service in store.services().iter().cloned() {
            let store = Arc::clone(&store);
            let fetcher = Arc::clone(&fetcher);
            in_flight.spawn(refresh_one(store, fetcher, service));
        }
    }
}

async fn refresh_one(
    store: Arc<DocumentStore>,
    fetcher: Arc<dyn DocumentFetcher>,
    service: UpstreamService,
) {
    if let Err(error) = store.load(&service, fetcher.as_ref()).await {
        tracing::error!(
            prefix = %service.prefix,
            "Health check {} failed. {error}",
            service.prefix
        );
    }
}

/// The running loader. Dropping it (or calling [`abort`](Self::abort)) stops the initial
/// loads and the refresh.
#[derive(Debug)]
pub struct LoaderHandle {
    tasks: Vec<JoinHandle<()>>,
}

impl LoaderHandle {
    /// Stops the loader.
    pub fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for LoaderHandle {
    fn drop(&mut self) {
        self.abort();
    }
}
