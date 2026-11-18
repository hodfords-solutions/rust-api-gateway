//! The MCP server: Parity Checklist area 08 (`docs/parity/08-mcp.md`), the port of the
//! Reference Gateway's `mcp/`. Compiled with the cargo feature `mcp`.
//!
//! Every Route of an Upstream Service can be exposed as an MCP tool that calls the service.
//!
//! - [`ToolRegistry`] builds the tools from the [`DocumentStore`](crate::openapi::DocumentStore)
//!   and rebuilds them every [`TOOL_REFRESH_INTERVAL`] (MCP-05 to MCP-13).
//! - [`ToolExecutor`] runs a call: rate limiting and middleware through a
//!   [`ToolCallBackend`], then the upstream request built by [`build_upstream_call`]
//!   (MCP-14 to MCP-20).
//! - [`McpEndpoint`] serves `POST /mcp` with `rmcp`'s Streamable HTTP transport in stateless
//!   mode, after the [`McpAuthentication`](crate::handler::McpAuthentication) handlers
//!   (MCP-01 to MCP-04).
//!
//! # Wiring
//!
//! The gateway does all of it when [`GatewayOptions::mcp`](crate::GatewayOptions) holds
//! enabled [`McpOptions`]: it starts the registry with the gateway, implements
//! [`ToolCallBackend`] with its rate limiter, proxy middleware and a connection pool per
//! Upstream Service, and mounts the endpoint. [`Gateway::mcp_tools`](crate::Gateway::mcp_tools)
//! reads the current tool list. The pieces are public for tests and other servers:
//!
//! ```ignore
//! let options = Arc::new(McpOptions::enabled());
//! let registry = Arc::new(ToolRegistry::new(store.clone(), options));
//! let _refresh = registry.start(); // keep alive
//! let executor = Arc::new(ToolExecutor::new(registry, backend, exclude_headers));
//! let endpoint = McpEndpoint::new(executor, authenticators);
//! ```
//!
//! # Deviations
//!
//! - D7 (MCP-17): path arguments are encoded and checked; see [`build_upstream_call`].
//! - D8 (MCP-12): a schema referenced from several places is resolved every time; only a
//!   reference to one of its own ancestors resolves to `{}`.

pub(crate) mod backend;
mod call;
mod executor;
mod js;
mod options;
mod registry;
mod schema;
mod server;
pub mod testing;

#[cfg(test)]
mod tests;

pub use call::{
    PathArgumentError, UpstreamCall, build_upstream_call, encode_path_argument, form_urlencode,
};
pub use executor::{
    BoxFuture, FORBIDDEN_TEXT, ToolCallBackend, ToolCallContext, ToolCallRejection, ToolExecutor,
    ToolResult, UpstreamError, UpstreamResponse,
};
pub use options::{
    DEFAULT_SERVER_NAME, DEFAULT_SERVER_VERSION, McpOptions, ParameterFilter,
    ParameterFilterContext, ServerInfo, ToolFilter, ToolNameContext, ToolNamer,
};
pub use registry::{
    MAX_TOOL_NAME_LENGTH, RegistryHandle, TOOL_REFRESH_INTERVAL, ToolBuild, ToolDefinition,
    ToolRegistry, build_tools,
};
pub use schema::{default_tool_name, normalize_schema_type};
pub use server::{INTERNAL_ERROR_BODY, McpBody, McpEndpoint, UNAUTHORIZED_BODY};
