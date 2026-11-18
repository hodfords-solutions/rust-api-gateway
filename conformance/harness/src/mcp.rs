//! Helpers for the MCP endpoint (`POST /mcp`, Streamable HTTP, stateless).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client::{self, Request, Response};

/// A JSON-RPC exchange with the MCP endpoint.
#[derive(Debug, Clone)]
pub struct McpReply {
    pub response: Response,
    /// The JSON-RPC message, taken from the SSE stream or the JSON body.
    pub message: Option<Value>,
}

/// Builds a JSON-RPC request to `POST /mcp`.
pub fn request(method: &str, params: Value) -> Request {
    Request::post("/mcp")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
}

/// Extracts the first JSON-RPC message from an SSE or JSON response body.
pub fn parse_message(response: &Response) -> Option<Value> {
    let text = response.text();
    let is_sse = response
        .header("content-type")
        .is_some_and(|v| v.contains("text/event-stream"));
    if is_sse {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .find_map(|data| serde_json::from_str(data.trim()).ok())
    } else {
        serde_json::from_str(&text).ok()
    }
}

pub async fn send(addr: SocketAddr, request: &Request) -> Result<McpReply> {
    let response = client::send(addr, request).await?;
    let message = parse_message(&response);
    Ok(McpReply { response, message })
}

/// Calls a tool and returns the `result` object (`content`, `isError`).
pub async fn call_tool(addr: SocketAddr, name: &str, arguments: Value) -> Result<Value> {
    let reply = send(
        addr,
        &request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        ),
    )
    .await?;
    let message = reply
        .message
        .with_context(|| format!("no JSON-RPC message in {:?}", reply.response.text()))?;
    Ok(message["result"].clone())
}

/// The text of a tool result's first content item.
pub fn result_text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
