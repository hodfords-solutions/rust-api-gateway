//! MCP tool calls (MCP-17, MCP-18): tool arguments come from the MCP client and are
//! substituted into the Route's path, query and headers. Properties (Deviation D7): an
//! argument never adds a path segment, a `?`/`#`, or a dot segment, and the result keeps the
//! template's prefix before its first placeholder.
//!
//! Input: a JSON case shaped like `crates/rust-gateway/src/mcp/testdata/call.json`:
//! `{"tool":{"httpMethod","path"},"args":{},"middleware":[[name,value]],"excludeHeaders":[]}`.
#![no_main]

use api_gateway::mcp::build_upstream_call;
use api_gateway::request::ProxyRequest;
use api_gateway::route::RouteMethod;
use api_gateway_fuzz::is_dot_segment;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

fuzz_target!(|data: &[u8]| {
    let Ok(Value::Object(case)) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let tool = case.get("tool");
    let method = tool
        .and_then(|t| t.get("httpMethod"))
        .and_then(Value::as_str)
        .and_then(RouteMethod::from_request_method)
        .unwrap_or(RouteMethod::Get);
    let Some(template) = tool.and_then(|t| t.get("path")).and_then(Value::as_str) else {
        return;
    };
    let arguments = match case.get("args") {
        Some(Value::Object(args)) => args.clone(),
        _ => Map::new(),
    };
    let mut middleware = ProxyRequest::new();
    if let Some(Value::Array(pairs)) = case.get("middleware") {
        for pair in pairs {
            if let (Some(n), Some(v)) = (
                pair.get(0).and_then(Value::as_str),
                pair.get(1).and_then(Value::as_str),
            ) {
                middleware.add_header(n, v);
            }
        }
    }
    let exclude: Vec<String> = match case.get("excludeHeaders") {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(|n| n.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let Ok(call) = build_upstream_call(method, template, &arguments, &middleware, &exclude) else {
        return;
    };
    let substituted = arguments
        .keys()
        .filter_map(|k| k.strip_prefix("path_"))
        .any(|name| template.contains(&format!("{{{name}}}")));
    if !substituted {
        return;
    }
    let path = call.path_and_query.split('?').next().unwrap_or("");
    // Substituted values are percent-encoded: no `/`, `?` or `#` can come from them. The
    // target gains one trailing `/` when it has none.
    let count = |s: &str, c: char| s.matches(c).count();
    assert!(
        count(path, '/') <= count(template, '/') + 1,
        "{template:?} -> {path:?}: an argument added a path segment"
    );
    if !template.contains(['?', '#']) {
        assert!(
            !path.contains('#') && call.path_and_query.matches('?').count() <= 1,
            "{template:?} -> {:?}: an argument added a query or fragment",
            call.path_and_query
        );
    }
    let template_dots = template.split('/').filter(|s| is_dot_segment(s)).count();
    let path_dots = path.split('/').filter(|s| is_dot_segment(s)).count();
    assert!(
        path_dots <= template_dots,
        "{template:?} -> {path:?}: an argument produced a dot segment"
    );
    let first = template.find('{').unwrap_or(template.len());
    assert!(
        call.path_and_query.starts_with(&template[..first]),
        "{template:?} -> {path:?}: the prefix before the first placeholder changed"
    );
});
