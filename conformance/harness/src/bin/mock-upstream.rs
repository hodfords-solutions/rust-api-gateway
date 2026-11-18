//! Runs a mock Upstream Service from the command line, for manual testing.
//!
//!     mock-upstream [--port N] [--fixture PATH]
//!
//! Serves the fixture at `/openapi.json`, echoes every other request as JSON, and prints
//! each recorded request to stdout as one JSON line.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use conformance::mock::{Document, MockUpstream};

#[tokio::main]
async fn main() -> Result<()> {
    let mut port = 0u16;
    let mut fixture = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().context("--port needs a value")?.parse()?,
            "--fixture" => fixture = Some(args.next().context("--fixture needs a value")?),
            "-h" | "--help" => {
                println!("usage: mock-upstream [--port N] [--fixture PATH]");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}"),
        }
    }

    let mock = MockUpstream::start_on(([127, 0, 0, 1], port).into()).await?;
    if let Some(path) = fixture {
        let json = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
        mock.set_document(Document::Json(json));
    }
    eprintln!(
        "mock upstream on {} (document at {})",
        mock.origin(),
        mock.doc_url()
    );

    let mut printed = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let requests = mock.requests();
        for request in &requests[printed..] {
            println!("{}", serde_json::to_string(request)?);
        }
        printed = requests.len();
    }
}
