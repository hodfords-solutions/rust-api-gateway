//! Loading, refreshing and readiness (OAS-01 to OAS-05, CFG-16), with a scripted fetcher.
//! Includes the `getServiceDetail` cases of `open-api.service.spec.ts` and the Rust-only
//! tests of Deviation D3 (OAS-17, OAS-23).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use super::*;
use crate::upstream::UpstreamService;

#[derive(Clone)]
enum Script {
    Ok(u16, Vec<u8>),
    Fail,
    /// Waits for the notify, then answers like the inner script.
    Gated(Arc<Notify>, Box<Script>),
}

#[derive(Default)]
struct FakeFetcher {
    scripts: Mutex<HashMap<String, Script>>,
    fetches: Mutex<HashMap<String, usize>>,
}

impl FakeFetcher {
    fn set(&self, url: &str, script: Script) {
        self.scripts.lock().unwrap().insert(url.to_string(), script);
    }

    fn json(&self, url: &str, document: &Value) {
        self.set(url, Script::Ok(200, serde_json::to_vec(document).unwrap()));
    }

    fn fetches(&self, url: &str) -> usize {
        self.fetches.lock().unwrap().get(url).copied().unwrap_or(0)
    }
}

impl DocumentFetcher for FakeFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        *self
            .fetches
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_default() += 1;
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or(Script::Fail);
        Box::pin(async move {
            let mut script = script;
            loop {
                match script {
                    Script::Ok(status, body) => return Ok(FetchedDocument { status, body }),
                    Script::Fail => return Err(FetchError::new("connection refused")),
                    Script::Gated(gate, inner) => {
                        gate.notified().await;
                        script = *inner;
                    }
                }
            }
        })
    }
}

fn service(prefix: &str) -> UpstreamService {
    UpstreamService::new(
        prefix,
        format!("http://{prefix}/doc"),
        format!("http://{prefix}"),
    )
}

fn doc(operation_id: &str) -> Value {
    json!({
        "info": { "title": "User Service", "version": "1.0.0" },
        "paths": { "/users/{id}": { "get": { "operationId": operation_id } } }
    })
}

fn store(prefixes: &[&str]) -> Arc<DocumentStore> {
    Arc::new(DocumentStore::new(
        prefixes.iter().map(|p| service(p)).collect(),
        SecurityKeys::default(),
    ))
}

fn routed(store: &DocumentStore, prefix: &str) -> Option<String> {
    store
        .route(prefix, "GET", "/users/1")
        .unwrap()
        .and_then(|route| route.operation_id.clone())
}

// OpenApiService.getServiceDetail

#[tokio::test]
async fn oas_01_loads_and_parses_the_document_on_success() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    fetcher.json("http://users/doc", &doc("show"));
    let outcome = store.load(&service("users"), &fetcher).await;
    assert_eq!(outcome, Ok(LoadOutcome::Loaded));
    assert_eq!(*store.original_document("users").unwrap(), doc("show"));
    assert_eq!(
        store.document("users").unwrap().title.as_deref(),
        Some("User Service")
    );
    assert_eq!(routed(&store, "users").as_deref(), Some("show"));
}

#[tokio::test]
async fn oas_04_skips_re_parsing_when_the_document_is_unchanged() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    fetcher.json("http://users/doc", &doc("show"));
    store.load(&service("users"), &fetcher).await.unwrap();
    let first = store.document("users").unwrap();
    // The same JSON with different formatting is the same document.
    fetcher.set(
        "http://users/doc",
        Script::Ok(200, serde_json::to_vec_pretty(&doc("show")).unwrap()),
    );
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Ok(LoadOutcome::Unchanged)
    );
    assert!(Arc::ptr_eq(&first, &store.document("users").unwrap()));
}

#[tokio::test]
async fn oas_04_re_parses_when_the_document_changes() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    fetcher.json("http://users/doc", &doc("show"));
    store.load(&service("users"), &fetcher).await.unwrap();
    let mut changed = doc("show");
    changed["info"]["version"] = json!("2.0.0");
    fetcher.json("http://users/doc", &changed);
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Ok(LoadOutcome::Loaded)
    );
    assert_eq!(
        store.document("users").unwrap().version.as_deref(),
        Some("2.0.0")
    );
}

#[tokio::test]
async fn oas_01_keeps_existing_docs_when_the_fetch_returns_a_non_200_status() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    fetcher.set("http://users/doc", Script::Ok(503, b"null".to_vec()));
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Err(LoadError::Status(503))
    );
    assert!(store.document("users").is_none());
    // Any status but 200 counts as a failure, 2xx included.
    fetcher.json("http://users/doc", &doc("show"));
    fetcher.set(
        "http://users/doc",
        Script::Ok(201, serde_json::to_vec(&doc("show")).unwrap()),
    );
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Err(LoadError::Status(201))
    );
    assert!(!store.has_document("users"));
}

#[tokio::test]
async fn oas_05_a_service_has_a_document_from_its_first_successful_load_onward() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    assert!(!store.has_document("users"));
    assert!(store.load(&service("users"), &fetcher).await.is_err());
    assert!(!store.has_document("users"));
    fetcher.json("http://users/doc", &doc("show"));
    store.load(&service("users"), &fetcher).await.unwrap();
    assert!(store.has_document("users"));
    fetcher.set("http://users/doc", Script::Fail);
    assert!(store.load(&service("users"), &fetcher).await.is_err());
    assert!(store.has_document("users"));
    assert!(!store.has_document("unknown"));
}

#[tokio::test]
async fn oas_03_a_failed_refresh_keeps_the_previous_document() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    fetcher.json("http://users/doc", &doc("v1"));
    store.load(&service("users"), &fetcher).await.unwrap();

    for failure in [
        Script::Fail,
        Script::Ok(500, b"{}".to_vec()),
        Script::Ok(404, Vec::new()),
    ] {
        fetcher.set("http://users/doc", failure);
        assert!(store.load(&service("users"), &fetcher).await.is_err());
        assert_eq!(routed(&store, "users").as_deref(), Some("v1"));
        assert_eq!(*store.original_document("users").unwrap(), doc("v1"));
    }

    // A body that is not JSON: the original document becomes the text, the Routes stay.
    fetcher.set("http://users/doc", Script::Ok(200, b"<html>".to_vec()));
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Err(LoadError::Build(BuildError::NotAnObject))
    );
    assert_eq!(routed(&store, "users").as_deref(), Some("v1"));
    assert_eq!(*store.original_document("users").unwrap(), json!("<html>"));

    // A document path-to-regexp rejects: same, and it is not retried until it changes.
    let mut invalid = doc("v2");
    invalid["paths"]["/users/{id}"]["get"]["x-router-path"] = json!("/users/(");
    fetcher.json("http://users/doc", &invalid);
    assert!(matches!(
        store.load(&service("users"), &fetcher).await,
        Err(LoadError::Build(BuildError::Pattern { .. }))
    ));
    assert_eq!(routed(&store, "users").as_deref(), Some("v1"));
    assert_eq!(*store.original_document("users").unwrap(), invalid);
    assert_eq!(
        store.load(&service("users"), &fetcher).await,
        Ok(LoadOutcome::Unchanged)
    );

    fetcher.json("http://users/doc", &doc("v3"));
    store.load(&service("users"), &fetcher).await.unwrap();
    assert_eq!(routed(&store, "users").as_deref(), Some("v3"));
    assert_eq!(store.loaded_services(), ["users"]);
}

#[tokio::test]
async fn oas_01_a_document_that_fails_to_build_on_first_load_leaves_no_document() {
    let store = store(&["users"]);
    let fetcher = FakeFetcher::default();
    let mut invalid = doc("v1");
    invalid["paths"]["/users/{id}"]["get"]["x-router-path"] = json!("/:a:b");
    fetcher.json("http://users/doc", &invalid);
    assert!(store.load(&service("users"), &fetcher).await.is_err());
    assert!(!store.has_document("users"));
    assert_eq!(*store.original_document("users").unwrap(), invalid);
    assert!(store.loaded_services().is_empty());
}

// The loader.

#[tokio::test]
async fn cfg_16_startup_does_not_wait_and_readiness_follows_every_first_load() {
    let store = store(&["a", "b", "c"]);
    let fetcher = Arc::new(FakeFetcher::default());
    let gate_a = Arc::new(Notify::new());
    let gate_b = Arc::new(Notify::new());
    let body = serde_json::to_vec(&doc("x")).unwrap();
    fetcher.set(
        "http://a/doc",
        Script::Gated(gate_a.clone(), Box::new(Script::Ok(200, body.clone()))),
    );
    fetcher.set(
        "http://b/doc",
        Script::Gated(gate_b.clone(), Box::new(Script::Ok(200, body))),
    );
    fetcher.set("http://c/doc", Script::Fail);

    let _loader = DocumentLoader::new(store.clone(), fetcher.clone()).start();
    assert!(!store.is_ready());

    // All three are fetched concurrently: b and c are requested while a is still waiting.
    wait_until(|| fetcher.fetches("http://b/doc") == 1 && fetcher.fetches("http://c/doc") == 1)
        .await;
    gate_b.notify_one();
    wait_until(|| store.has_document("b")).await;
    assert!(!store.is_ready(), "a has not settled yet");

    gate_a.notify_one();
    tokio::time::timeout(Duration::from_secs(5), store.ready())
        .await
        .unwrap();
    assert!(store.is_ready());
    // First-load order, not configuration order; c failed and has no document.
    assert_eq!(store.loaded_services(), ["b", "a"]);
}

#[tokio::test]
async fn cfg_16_readiness_resolves_when_every_first_load_fails() {
    let store = store(&["a", "b"]);
    let fetcher = Arc::new(FakeFetcher::default());
    let _loader = DocumentLoader::new(store.clone(), fetcher).start();
    tokio::time::timeout(Duration::from_secs(5), store.ready())
        .await
        .unwrap();
    assert!(store.loaded_services().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oas_02_oas_03_refresh_changes_routing_and_failures_keep_the_previous_document() {
    let store = store(&["users", "down"]);
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.json("http://users/doc", &doc("v1"));
    let _loader = DocumentLoader::new(store.clone(), fetcher.clone())
        .refresh_interval(Duration::from_millis(20))
        .start();
    tokio::time::timeout(Duration::from_secs(5), store.ready())
        .await
        .unwrap();
    assert_eq!(routed(&store, "users").as_deref(), Some("v1"));

    fetcher.json("http://users/doc", &doc("v2"));
    wait_until(|| routed(&store, "users").as_deref() == Some("v2")).await;

    fetcher.set("http://users/doc", Script::Fail);
    let before = fetcher.fetches("http://users/doc");
    wait_until(|| fetcher.fetches("http://users/doc") >= before + 3).await;
    assert_eq!(routed(&store, "users").as_deref(), Some("v2"));

    // A service that was down at startup loads on a later refresh.
    fetcher.json("http://down/doc", &doc("late"));
    wait_until(|| store.has_document("down")).await;
    assert_eq!(store.loaded_services(), ["users", "down"]);
}

#[test]
fn oas_02_the_refresh_interval_is_ten_seconds_on_the_wall_clock() {
    assert_eq!(DOCUMENT_REFRESH_INTERVAL, Duration::from_secs(10));
}

#[tokio::test]
async fn dropping_the_handle_stops_the_refresh() {
    let store = store(&["users"]);
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.json("http://users/doc", &doc("v1"));
    let loader = DocumentLoader::new(store.clone(), fetcher.clone())
        .refresh_interval(Duration::from_millis(5))
        .start();
    store.ready().await;
    wait_until(|| fetcher.fetches("http://users/doc") >= 3).await;
    drop(loader);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stopped = fetcher.fetches("http://users/doc");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fetcher.fetches("http://users/doc"), stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_run_while_a_refresh_swaps_the_table() {
    let store = store(&["users"]);
    store.apply("users", "http://users/doc", doc("v0")).unwrap();
    let lookups = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for _ in 0..3 {
        let store = store.clone();
        let lookups = lookups.clone();
        readers.push(tokio::spawn(async move {
            for _ in 0..2000 {
                let id = routed(&store, "users").expect("a Route always matches");
                assert!(id.starts_with('v'));
                lookups.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }));
    }
    for i in 1..200 {
        store
            .apply("users", "http://users/doc", doc(&format!("v{i}")))
            .unwrap();
        tokio::task::yield_now().await;
    }
    for reader in readers {
        reader.await.unwrap();
    }
    assert_eq!(lookups.load(Ordering::Relaxed), 6000);
}

// Deviation D3: Rust-only tests. The reference does not load these documents.

#[tokio::test]
async fn oas_17_a_document_with_non_method_path_item_keys_loads() {
    let store = store(&["svc"]);
    let fetcher = FakeFetcher::default();
    fetcher.set(
        "http://svc/doc",
        Script::Ok(
            200,
            include_bytes!("../../../../conformance/fixtures/invalid-path-item.json").to_vec(),
        ),
    );
    assert_eq!(
        store.load(&service("svc"), &fetcher).await,
        Ok(LoadOutcome::Loaded)
    );
    assert!(store.has_document("svc"));
    assert_eq!(store.loaded_services(), ["svc"]);
}

#[tokio::test]
async fn oas_23_a_document_without_info_loads() {
    let store = store(&["svc"]);
    let fetcher = FakeFetcher::default();
    fetcher.json(
        "http://svc/doc",
        &json!({ "paths": { "/users/{id}": { "get": { "operationId": "show" } } } }),
    );
    assert_eq!(
        store.load(&service("svc"), &fetcher).await,
        Ok(LoadOutcome::Loaded)
    );
    assert_eq!(routed(&store, "svc").as_deref(), Some("show"));
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("condition reached within 5 seconds");
}
