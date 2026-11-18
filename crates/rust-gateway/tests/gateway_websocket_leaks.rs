//! WS-14 under load (Rust-only): many concurrent tunnels, closed gracefully, reset by the
//! client, and reset while the gateway is writing to the client, leave no task and no
//! socket behind on either side. Alone in its test binary so that the process's file
//! descriptor count is not disturbed by other tests.

mod gateway_common;
mod websocket_common;

use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::upstream::UpstreamService;
use gateway_common::{options, serve};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use websocket_common::{EchoUpstream, read_head, upgrade_request};

const TUNNELS: usize = 300;

/// Open file descriptors of this process.
fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd")
        .map(|dir| dir.count())
        .unwrap_or(0)
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// Waits until `measure` is at most `limit`; returns its last value.
async fn settles(limit: usize, measure: impl Fn() -> usize) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let value = measure();
        if value <= limit || tokio::time::Instant::now() > deadline {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How a client leaves its tunnel.
#[derive(Clone, Copy, Debug)]
enum Leave {
    /// Closes its connection (FIN).
    Close,
    /// Resets its connection (RST) while idle.
    Reset,
    /// Resets its connection while the upstream is sending it data.
    ResetWhileReceiving,
}

async fn tunnel(addr: std::net::SocketAddr, n: usize, leave: Leave) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    stream
        .write_all(&upgrade_request(&format!("/svc/socket/{n}"), ""))
        .await
        .expect("upgrade request");
    let head = read_head(&mut stream).await;
    assert!(
        head.starts_with(b"HTTP/1.1 101 "),
        "{}",
        String::from_utf8_lossy(&head)
    );
    let message = format!("message {n}");
    stream.write_all(message.as_bytes()).await.expect("send");
    let mut echoed = vec![0u8; message.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut echoed))
        .await
        .expect("echo in time")
        .expect("echo");
    assert_eq!(echoed, message.as_bytes());
    match leave {
        Leave::Close => drop(stream),
        Leave::Reset => {
            socket2::SockRef::from(&stream)
                .set_linger(Some(Duration::ZERO))
                .expect("linger");
            drop(stream);
        }
        Leave::ResetWhileReceiving => {
            // The echo keeps data flowing towards the client while it resets.
            stream
                .write_all(&vec![b'x'; 256 * 1024])
                .await
                .expect("send");
            socket2::SockRef::from(&stream)
                .set_linger(Some(Duration::ZERO))
                .expect("linger");
            drop(stream);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_tunnels_and_abrupt_disconnects_leave_nothing_behind() {
    let upstream = EchoUpstream::start().await;
    let service = UpstreamService::new(
        "svc",
        format!("http://{}/openapi.json", upstream.addr),
        format!("http://{}", upstream.addr),
    );
    let served = serve(Gateway::builder(options(vec![service])), true).await;
    // Let startup tasks (the first document load) finish before measuring.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let fds_before = open_fds();
    let tasks_before = alive_tasks();

    let mut clients = tokio::task::JoinSet::new();
    for n in 0..TUNNELS {
        let leave = match n % 3 {
            0 => Leave::Close,
            1 => Leave::Reset,
            _ => Leave::ResetWhileReceiving,
        };
        clients.spawn(tunnel(served.addr, n, leave));
    }
    while let Some(result) = clients.join_next().await {
        result.expect("client task");
    }
    assert_eq!(upstream.accepted(), TUNNELS);

    let open = upstream.wait_open_at_most(0, Duration::from_secs(10)).await;
    assert_eq!(open, 0, "upstream connections left open");
    let tasks = settles(tasks_before, alive_tasks).await;
    assert!(
        tasks <= tasks_before,
        "tasks before {tasks_before}, after {tasks}"
    );
    let fds = settles(fds_before, open_fds).await;
    assert!(
        fds <= fds_before,
        "file descriptors before {fds_before}, after {fds}"
    );
    served.shutdown().await.expect("serve");
}
