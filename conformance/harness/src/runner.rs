//! The scenario runner: starts a target gateway with a scenario's configuration, runs the
//! scenario against it, and in `both` mode compares the two targets' observations.
//!
//! Target selection: `CONFORMANCE_TARGET` = `reference` (default), `rust` or `both`.
//! The Rust target only runs scenarios whose checklist IDs are all listed in
//! `conformance/rust-implemented.txt`; the others are skipped for it, not failed.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::client::{self, Request, Response};
use crate::config::GatewayConfig;
use crate::redis::{allocate_db, flush_db, redis_addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// The Node app in `conformance/reference/` embedding the Reference Gateway.
    Reference,
    /// `rust-gateway-server`.
    Rust,
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TargetKind::Reference => "reference",
            TargetKind::Rust => "rust",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Reference,
    Rust,
    Both,
}

impl Selection {
    pub fn from_env() -> Result<Self> {
        match std::env::var("CONFORMANCE_TARGET").as_deref() {
            Err(_) | Ok("") | Ok("reference") => Ok(Selection::Reference),
            Ok("rust") => Ok(Selection::Rust),
            Ok("both") => Ok(Selection::Both),
            Ok(other) => bail!("CONFORMANCE_TARGET must be reference, rust or both, not {other:?}"),
        }
    }

    pub fn kinds(self) -> &'static [TargetKind] {
        match self {
            Selection::Reference => &[TargetKind::Reference],
            Selection::Rust => &[TargetKind::Rust],
            Selection::Both => &[TargetKind::Reference, TargetKind::Rust],
        }
    }
}

/// The repository root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root exists")
}

/// Parses an implemented-IDs list: one checklist ID per line, `#` starts a comment.
pub fn parse_implemented(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// Checklist IDs the Rust target implements, from `conformance/rust-implemented.txt`.
pub fn rust_implemented() -> Result<BTreeSet<String>> {
    let path = workspace_root().join("conformance/rust-implemented.txt");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_implemented(&text))
}

/// IDs in `covers` that the Rust target does not implement yet.
pub fn missing_for_rust(covers: &[&str], implemented: &BTreeSet<String>) -> Vec<String> {
    covers
        .iter()
        .filter(|id| !implemented.contains(**id))
        .map(|id| id.to_string())
        .collect()
}

/// Normalised results a scenario wants compared across targets in `both` mode.
/// Record only values that must be identical: no ports, dates or other run-specific data.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Observations {
    entries: Vec<(String, Value)>,
}

impl Observations {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, label: impl Into<String>, value: impl Serialize) {
        let value = serde_json::to_value(value).expect("observation serialises");
        self.entries.push((label.into(), value));
    }

    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }
}

/// Differences between two targets' observations, one line per differing label.
pub fn compare(reference: &Observations, rust: &Observations) -> Vec<String> {
    let mut diffs = Vec::new();
    let max = reference.entries.len().max(rust.entries.len());
    for i in 0..max {
        match (reference.entries.get(i), rust.entries.get(i)) {
            (Some(a), Some(b)) if a == b => {}
            (a, b) => diffs.push(format!(
                "#{i}: reference {} / rust {}",
                a.map(|(l, v)| format!("{l} = {v}"))
                    .unwrap_or_else(|| "<none>".into()),
                b.map(|(l, v)| format!("{l} = {v}"))
                    .unwrap_or_else(|| "<none>".into()),
            )),
        }
    }
    diffs
}

/// One target a scenario runs against.
pub struct Target {
    pub kind: TargetKind,
    scenario: &'static str,
    starts: AtomicU32,
}

impl Target {
    pub fn new(kind: TargetKind, scenario: &'static str) -> Self {
        Self {
            kind,
            scenario,
            starts: AtomicU32::new(0),
        }
    }

    /// Starts the gateway with `config`, filling in the port and, unless the scenario set
    /// one, the Redis address. A fresh, flushed Redis database is always assigned.
    pub async fn start(&self, mut config: GatewayConfig) -> Result<Gateway> {
        let n = self.starts.fetch_add(1, Ordering::SeqCst);
        config.port = free_port()?;
        if config.redis.host.is_empty() {
            let (host, port) = redis_addr();
            config.redis.host = host;
            config.redis.port = port;
        }
        config.redis.db = allocate_db();
        flush_db(config.redis.db).await?;

        let dir = workspace_root().join("target/conformance");
        std::fs::create_dir_all(&dir)?;
        let stem = format!("{}-{}-{n}", self.scenario, self.kind);
        let config_path = dir.join(format!("{stem}.json"));
        let log_path = dir.join(format!("{stem}.log"));
        std::fs::write(&config_path, config.to_json())?;
        let log = std::fs::File::create(&log_path)?;

        let mut command = match self.kind {
            TargetKind::Reference => {
                let main = workspace_root().join("conformance/reference/dist/main.js");
                if !main.exists() {
                    bail!(
                        "{} is missing: run `pnpm install && pnpm build` in conformance/reference",
                        main.display()
                    );
                }
                let mut c = Command::new(std::env::var("NODE").unwrap_or_else(|_| "node".into()));
                c.arg(main);
                c
            }
            TargetKind::Rust => {
                let bin = std::env::var("RUST_GATEWAY_SERVER_BIN")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| workspace_root().join("target/debug/rust-gateway-server"));
                if !bin.exists() {
                    bail!(
                        "{} is missing: run `cargo build -p rust-gateway-server` or set RUST_GATEWAY_SERVER_BIN",
                        bin.display()
                    );
                }
                Command::new(bin)
            }
        };
        command
            .env("GATEWAY_CONFIG", &config_path)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        let child = command
            .spawn()
            .with_context(|| format!("starting the {} target", self.kind))?;

        let mut gateway = Gateway {
            kind: self.kind,
            addr: SocketAddr::from(([127, 0, 0, 1], config.port)),
            child,
            log_path,
        };
        gateway.wait_ready(Duration::from_secs(60)).await?;
        Ok(gateway)
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("0.0.0.0:0")?;
    Ok(listener.local_addr()?.port())
}

/// A running target. Dropping it kills the process.
pub struct Gateway {
    pub kind: TargetKind,
    addr: SocketAddr,
    child: Child,
    log_path: PathBuf,
}

impl Gateway {
    /// The IPv4 loopback address of the gateway.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub async fn send(&self, request: &Request) -> Result<Response> {
        client::send(self.addr, request).await
    }

    pub async fn get(&self, target: &str) -> Result<Response> {
        self.send(&Request::get(target)).await
    }

    /// `GET /__conformance/ready`: the services whose document is loaded.
    pub async fn loaded_services(&self) -> Result<Vec<String>> {
        let response = self.get("/__conformance/ready").await?;
        let json = response.json()?;
        Ok(serde_json::from_value(json["services"].clone())?)
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The last lines of the target's output.
    pub fn log_tail(&self, lines: usize) -> String {
        let text = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    async fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "{} target exited with {status} before it was ready; log {}:\n{}",
                    self.kind,
                    self.log_path.display(),
                    self.log_tail(40)
                );
            }
            let probe = client::send_with_timeout(
                self.addr,
                &Request::get("/__conformance/ready"),
                Duration::from_secs(2),
            )
            .await;
            if matches!(probe, Ok(ref r) if r.status == 200) {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "{} target not ready after {timeout:?}; log {}:\n{}",
                    self.kind,
                    self.log_path.display(),
                    self.log_tail(40)
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Runs a Conformance Test. `covers` lists the checklist IDs it proves.
///
/// Panics (failing the test) when the scenario fails on a target, or when in `both` mode
/// the targets' observations differ.
pub async fn run<F, Fut>(name: &'static str, covers: &'static [&'static str], scenario: F)
where
    F: Fn(Target) -> Fut,
    Fut: Future<Output = Result<Observations>>,
{
    if let Err(error) = run_inner(name, covers, false, scenario).await {
        panic!("{name}: {error:#}");
    }
}

/// Runs a scenario that checks reference behaviour behind an approved Deviation. It never
/// runs against the Rust target, whose behaviour for these rows is tested Rust-only.
pub async fn run_reference_only<F, Fut>(
    name: &'static str,
    covers: &'static [&'static str],
    scenario: F,
) where
    F: Fn(Target) -> Fut,
    Fut: Future<Output = Result<Observations>>,
{
    if let Err(error) = run_inner(name, covers, true, scenario).await {
        panic!("{name}: {error:#}");
    }
}

async fn run_inner<F, Fut>(
    name: &'static str,
    covers: &'static [&'static str],
    reference_only: bool,
    scenario: F,
) -> Result<()>
where
    F: Fn(Target) -> Fut,
    Fut: Future<Output = Result<Observations>>,
{
    let selection = Selection::from_env()?;
    let mut results = Vec::new();
    for &kind in selection.kinds() {
        if kind == TargetKind::Rust {
            if reference_only {
                eprintln!("SKIP {name} [rust]: reference-only check of a Deviation");
                continue;
            }
            let missing = missing_for_rust(covers, &rust_implemented()?);
            if !missing.is_empty() {
                eprintln!(
                    "SKIP {name} [rust]: not in conformance/rust-implemented.txt: {}",
                    missing.join(", ")
                );
                continue;
            }
        }
        let observations = scenario(Target::new(kind, name))
            .await
            .with_context(|| format!("[{kind}] covering {}", covers.join(", ")))?;
        eprintln!(
            "PASS {name} [{kind}] {}\n{}",
            covers.join(", "),
            serde_json::to_string_pretty(&observations.entries).unwrap_or_default()
        );
        results.push((kind, observations));
    }
    if let [(TargetKind::Reference, reference), (TargetKind::Rust, rust)] = results.as_slice() {
        let diffs = compare(reference, rust);
        if !diffs.is_empty() {
            bail!("targets differ:\n{}", diffs.join("\n"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implemented_list_ignores_comments_and_blank_lines() {
        let ids = parse_implemented("# header\n\nREQ-01\nFWD-04  # query strings\n");
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec!["FWD-04".to_string(), "REQ-01".to_string()]
        );
    }

    #[test]
    fn a_scenario_runs_for_rust_only_when_every_id_is_implemented() {
        let implemented = parse_implemented("REQ-01\nREQ-02\n");
        assert!(missing_for_rust(&["REQ-01", "REQ-02"], &implemented).is_empty());
        assert_eq!(
            missing_for_rust(&["REQ-01", "REQ-03"], &implemented),
            vec!["REQ-03".to_string()]
        );
    }

    #[test]
    fn the_committed_implemented_list_parses() {
        rust_implemented().unwrap();
    }

    #[test]
    fn observations_compare_by_position() {
        let mut a = Observations::new();
        a.record("status", 200);
        let mut b = Observations::new();
        b.record("status", 200);
        assert!(compare(&a, &b).is_empty());
        b.record("extra", true);
        assert_eq!(compare(&a, &b).len(), 1);
    }
}
