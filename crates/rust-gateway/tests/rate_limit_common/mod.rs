//! Helpers shared by the `rate_limit_*` integration tests, which need a real Redis.
//!
//! Redis is found the way the conformance harness finds it: `CONFORMANCE_REDIS_HOST`
//! (default `127.0.0.1`) and `CONFORMANCE_REDIS_PORT` (default `6399`). When it cannot be
//! reached the tests print `SKIP` and pass; set `RATE_LIMIT_REQUIRE_REDIS=1` to make them
//! fail instead. They use database 0, which the conformance runner never assigns, and
//! unique keys, so they can run next to the conformance suite.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{HeaderMap, HeaderValue};
use rust_gateway::rate_limit::{
    BucketLimit, RateLimitOptions, RateLimitState, RateLimiter, RedisOptions, TrustProxy,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub fn redis_host() -> String {
    std::env::var("CONFORMANCE_REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

pub fn redis_port() -> u16 {
    std::env::var("CONFORMANCE_REDIS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6399)
}

pub fn redis_options() -> RedisOptions {
    RedisOptions::new(redis_host(), redis_port())
}

/// Whether Redis answers; panics instead when `RATE_LIMIT_REQUIRE_REDIS=1`.
pub async fn redis_up() -> bool {
    let up = tokio::time::timeout(
        Duration::from_millis(500),
        TcpStream::connect((redis_host().as_str(), redis_port())),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    if !up {
        assert!(
            std::env::var("RATE_LIMIT_REQUIRE_REDIS").as_deref() != Ok("1"),
            "Redis is not reachable at {}:{}",
            redis_host(),
            redis_port()
        );
        eprintln!(
            "SKIP: Redis is not reachable at {}:{} (set CONFORMANCE_REDIS_PORT)",
            redis_host(),
            redis_port()
        );
    }
    up
}

#[macro_export]
macro_rules! require_redis {
    () => {
        if !rate_limit_common::redis_up().await {
            return;
        }
    };
}

/// A string unique to this test run and call.
pub fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Options with limits too high to matter, trusting every proxy so that a test picks the
/// client address with `X-Forwarded-For` (see [`state`]).
pub fn options() -> RateLimitOptions {
    let mut options = RateLimitOptions::new(
        true,
        BucketLimit::new(1_000_000, 60.0),
        BucketLimit::new(1_000_000, 60.0),
    );
    options.trust_proxy = TrustProxy::All;
    options
}

pub fn limiter(options: &RateLimitOptions) -> RateLimiter {
    limiter_on(options, &redis_options())
}

pub fn limiter_on(options: &RateLimitOptions, redis: &RedisOptions) -> RateLimiter {
    RateLimiter::new(options, redis).expect("valid options")
}

/// A request state whose client address is `client` (any string: non-addresses are used as
/// they are), given [`options`]'s `trust proxy` setting.
pub fn state(limiter: &RateLimiter, client: &str) -> RateLimitState {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_str(client).expect("header value"),
    );
    limiter.request_state(Some("127.0.0.1".parse().expect("ip")), &headers)
}

/// A direct connection to the test Redis, for setting up and inspecting state.
pub async fn raw_redis() -> redis::aio::MultiplexedConnection {
    let client = redis::Client::open(format!("redis://{}:{}/0", redis_host(), redis_port()))
        .expect("client");
    client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to Redis")
}

/// A TCP forwarder in front of Redis. Stopping it closes every connection and the listening
/// socket, which a client sees exactly like a Redis restart; it can then be started again on
/// the same port.
pub struct Proxy {
    addr: SocketAddr,
    target: (String, u16),
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Proxy {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let proxy = Self {
            addr,
            target: (redis_host(), redis_port()),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        proxy.serve(listener);
        proxy
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn redis_options(&self) -> RedisOptions {
        RedisOptions::new("127.0.0.1", self.addr.port())
    }

    fn serve(&self, listener: TcpListener) {
        let tasks = Arc::clone(&self.tasks);
        let target = self.target.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let target = target.clone();
                let task = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect((target.0.as_str(), target.1)).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                tasks.lock().unwrap_or_else(|e| e.into_inner()).push(task);
            }
        });
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(accept);
    }

    /// Closes the listener and every forwarded connection.
    pub async fn stop(&self) {
        let tasks: Vec<_> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    /// Listens again on the same port.
    pub async fn restart(&self) {
        let listener = TcpListener::bind(self.addr).await.expect("rebind");
        self.serve(listener);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            task.abort();
        }
    }
}

/// A listener that accepts connections and never answers: Redis behind a dead network path.
/// It counts the connections it accepted.
pub struct Blackhole {
    addr: SocketAddr,
    accepted: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Blackhole {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accepted = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(socket);
            }
        });
        Self {
            addr,
            accepted,
            task,
        }
    }

    pub fn redis_options(&self) -> RedisOptions {
        RedisOptions::new("127.0.0.1", self.addr.port())
    }

    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Blackhole {
    fn drop(&mut self) {
        self.task.abort();
    }
}
