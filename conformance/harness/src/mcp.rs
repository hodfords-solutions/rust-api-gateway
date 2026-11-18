//! Helpers for the MCP endpoint (`POST /mcp`, Streamable HTTP, stateless).

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{self, Request, Response};

/// The `Accept` value a conforming Streamable HTTP client sends.
pub const ACCEPT: &str = "application/json, text/event-stream";

/// A JSON-RPC exchange with the MCP endpoint.
#[derive(Debug, Clone)]
pub struct McpReply {
    pub response: Response,
    /// The JSON-RPC message, taken from the SSE stream or the JSON body.
    pub message: Option<Value>,
}

/// A JSON-RPC request object with id 1.
pub fn rpc(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// Builds a JSON-RPC request to `POST /mcp`.
pub fn request(method: &str, params: Value) -> Request {
    post(&rpc(method, params))
}

/// `POST /mcp` with a JSON body and the standard `Accept` header.
pub fn post(body: &Value) -> Request {
    Request::post("/mcp").header("Accept", ACCEPT).json(body)
}

/// `POST /mcp` with a raw body, the standard `Accept` header and `content-type:
/// application/json`.
pub fn post_raw(body: &str) -> Request {
    Request::post("/mcp")
        .header("Accept", ACCEPT)
        .header("content-type", "application/json")
        .body(body.as_bytes().to_vec())
}

/// Every JSON-RPC message of an SSE or JSON response body, in order.
pub fn parse_messages(response: &Response) -> Vec<Value> {
    let text = response.text();
    if is_sse(response) {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect()
    } else {
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(items)) => items,
            Ok(value) => vec![value],
            Err(_) => Vec::new(),
        }
    }
}

/// Extracts the first JSON-RPC message from an SSE or JSON response body.
pub fn parse_message(response: &Response) -> Option<Value> {
    parse_messages(response).into_iter().next()
}

fn is_sse(response: &Response) -> bool {
    response
        .header("content-type")
        .is_some_and(|v| v.contains("text/event-stream"))
}

/// The media type of a response without parameters, or `null`.
pub fn media_type(response: &Response) -> Value {
    response
        .header("content-type")
        .map(|v| Value::String(v.split(';').next().unwrap_or("").trim().to_string()))
        .unwrap_or(Value::Null)
}

/// What a client observes of an MCP response at the JSON-RPC level: status, media type,
/// whether a session id was issued, and the messages (or the raw body when it holds none).
pub fn exchange_view(response: &Response) -> Value {
    let messages = parse_messages(response);
    json!({
        "status": response.status,
        "mediaType": media_type(response),
        "sessionId": response.header("mcp-session-id").is_some(),
        "messages": messages,
        "rawBody": if messages.is_empty() { Value::String(response.text()) } else { Value::Null },
    })
}

pub async fn send(addr: SocketAddr, request: &Request) -> Result<McpReply> {
    let response = client::send(addr, request).await?;
    let message = parse_message(&response);
    Ok(McpReply { response, message })
}

/// Calls a tool and returns the `result` object (`content`, `isError`).
pub async fn call_tool(addr: SocketAddr, name: &str, arguments: Value) -> Result<Value> {
    call_tool_with(addr, name, arguments, &[]).await
}

/// Calls a tool with extra headers on the `/mcp` request and returns the `result` object.
pub async fn call_tool_with(
    addr: SocketAddr,
    name: &str,
    arguments: Value,
    headers: &[(&str, &str)],
) -> Result<Value> {
    let mut request = self::request(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    );
    for (n, v) in headers {
        request = request.header(n, v);
    }
    let reply = send(addr, &request).await?;
    let message = reply
        .message
        .with_context(|| format!("no JSON-RPC message in {:?}", reply.response.text()))?;
    Ok(message["result"].clone())
}

/// The tools `tools/list` returns.
pub async fn list_tools(addr: SocketAddr) -> Result<Vec<Value>> {
    let reply = send(addr, &request("tools/list", json!({}))).await?;
    let message = reply
        .message
        .with_context(|| format!("no JSON-RPC message in {:?}", reply.response.text()))?;
    match &message["result"]["tools"] {
        Value::Array(tools) => Ok(tools.clone()),
        other => bail!("tools/list returned {other}"),
    }
}

/// The names of the tools `tools/list` returns, in order.
pub async fn tool_names(addr: SocketAddr) -> Result<Vec<String>> {
    Ok(list_tools(addr)
        .await?
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect())
}

/// The text of a tool result's first content item.
pub fn result_text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
