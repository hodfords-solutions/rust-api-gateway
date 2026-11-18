//! Redis plumbing for scenarios: database allocation, flushing, and a severable link.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

/// Databases available on the conformance Redis (`--databases 128`). Index 0 is never used.
const DATABASES: u32 = 128;

static NEXT_DB: AtomicU32 = AtomicU32::new(0);

/// Where the conformance Redis listens: `CONFORMANCE_REDIS_HOST` (default `127.0.0.1`) and
/// `CONFORMANCE_REDIS_PORT` (default `6399`, the port in `conformance/docker-compose.yml`).
pub fn redis_addr() -> (String, u16) {
    let host = std::env::var("CONFORMANCE_REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("CONFORMANCE_REDIS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6399);
    (host, port)
}

/// Picks the next database index for a scenario run (1 to 127, cycling).
pub fn allocate_db() -> u32 {
    1 + NEXT_DB.fetch_add(1, Ordering::SeqCst) % (DATABASES - 1)
}

/// Sends one command and returns the raw reply line.
async fn command(stream: &mut TcpStream, args: &[&str]) -> Result<String> {
    let mut out = format!("*{}\r\n", args.len());
    for arg in args {
        out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
    }
    stream.write_all(out.as_bytes()).await?;
    let mut buf = vec![0u8; 512];
    let n = stream.read(&mut buf).await?;
    let reply = String::from_utf8_lossy(&buf[..n]).into_owned();
    if reply.starts_with('-') {
        bail!("redis error: {}", reply.trim());
    }
    Ok(reply)
}

/// Empties database `db` so a scenario starts with no rate-limit state.
pub async fn flush_db(db: u32) -> Result<()> {
    let (host, port) = redis_addr();
    let mut stream = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| {
            format!("connecting to the conformance Redis at {host}:{port} (is `docker compose -f conformance/docker-compose.yml up -d` running?)")
        })?;
    command(&mut stream, &["SELECT", &db.to_string()]).await?;
    command(&mut stream, &["FLUSHDB"]).await?;
    Ok(())
}

/// A TCP relay between a gateway and Redis that a scenario can cut.
pub struct RedisLink {
    addr: SocketAddr,
    tasks: Arc<Mutex<Vec<AbortHandle>>>,
}

impl RedisLink {
    pub async fn start() -> Result<Self> {
        let (host, port) = redis_addr();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let tasks: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
        let relay_tasks = tasks.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let host = host.clone();
                let relay = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect((host.as_str(), port)).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                relay_tasks.lock().unwrap().push(relay.abort_handle());
            }
        });
        tasks.lock().unwrap().push(accept.abort_handle());
        Ok(Self { addr, tasks })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drops every relayed connection and stops listening, so reconnects are refused.
    pub fn sever(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

impl Drop for RedisLink {
    fn drop(&mut self) {
        self.sever();
    }
}
