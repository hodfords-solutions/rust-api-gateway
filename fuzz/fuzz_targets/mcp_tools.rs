//! MCP tool generation (MCP-06 to MCP-12) from an untrusted upstream document: the Route
//! table, tool names and input schemas, including `$ref` resolution.
#![no_main]

use api_gateway::mcp::{McpOptions, build_tools, normalize_schema_type};
use api_gateway::openapi::{DocumentStore, SecurityKeys};
use api_gateway::upstream::UpstreamService;
use api_gateway_fuzz::skip_open_finding;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    // FUZZ-04: shared `$ref`s expand exponentially (Deviation D8).
    if skip_open_finding("FUZZ-04") && data.windows(4).filter(|w| w == b"$ref").count() > 12 {
        return;
    }
    let Ok(document) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let _ = normalize_schema_type(&document);
    let store = DocumentStore::new(
        vec![UpstreamService::new(
            "svc",
            "http://127.0.0.1:1/doc",
            "http://127.0.0.1:1",
        )],
        SecurityKeys::default(),
    );
    if store
        .apply("svc", "http://127.0.0.1:1/doc", document)
        .is_err()
    {
        return;
    }
    let options = McpOptions {
        enabled: true,
        ..McpOptions::default()
    };
    let build = build_tools(&store, &options);
    for tool in &build.tools {
        let _ = format!("{tool:?}");
    }
});
