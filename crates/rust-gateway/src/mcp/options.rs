//! The MCP options: the reference's `McpOption` (CFG-13).

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::route::Route;

/// The default server name (MCP-03).
pub const DEFAULT_SERVER_NAME: &str = "nestjs-api-gateway";
/// The default server version (MCP-03).
pub const DEFAULT_SERVER_VERSION: &str = "1.0.0";

/// Decides whether a Route becomes a tool. Receives the Service Prefix and the Route
/// (MCP-06).
pub type ToolFilter = Arc<dyn Fn(&str, &Route) -> bool + Send + Sync>;
/// Decides whether an operation parameter becomes a property of the input schema. Receives
/// the parameter exactly as the document has it (MCP-10).
pub type ParameterFilter = Arc<dyn Fn(&Value, &ParameterFilterContext<'_>) -> bool + Send + Sync>;
/// Names a tool (MCP-07).
pub type ToolNamer = Arc<dyn Fn(&ToolNameContext<'_>) -> String + Send + Sync>;

/// What a [`ParameterFilter`] is told about the operation: the reference's
/// `McpParameterFilterContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterFilterContext<'a> {
    /// The Service Prefix.
    pub service_name: &'a str,
    /// The OpenAPI path, for example `/users/{id}`.
    pub path: &'a str,
    /// The lower-case method, as the path-item key (`get`).
    pub method: &'a str,
}

/// What a [`ToolNamer`] is told about the tool: the reference's `McpToolNameContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolNameContext<'a> {
    /// The Service Prefix.
    pub service_name: &'a str,
    /// The lower-case method, as the path-item key (`get`).
    pub method: &'a str,
    /// The operation id, if the operation has one.
    pub operation_id: Option<&'a str>,
    /// The OpenAPI path, for example `/users/{id}`.
    pub path: &'a str,
    /// The name the default strategy gives the tool.
    pub default_name: &'a str,
}

/// The server identity sent in the `initialize` result (MCP-03).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// Replaces [`DEFAULT_SERVER_NAME`] unless absent or empty.
    pub name: Option<String>,
    /// Replaces [`DEFAULT_SERVER_VERSION`] unless absent or empty.
    pub version: Option<String>,
}

impl ServerInfo {
    /// The name sent: the option, or the default when it is absent or empty (the
    /// reference uses `||`).
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(DEFAULT_SERVER_NAME)
    }

    /// The version sent: the option, or the default when it is absent or empty.
    pub fn version(&self) -> &str {
        self.version
            .as_deref()
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_SERVER_VERSION)
    }
}

/// The MCP options: the reference's `McpOption`. `McpOption.path` is not ported (the
/// endpoint is fixed at `POST /mcp`, MCP-01).
#[derive(Clone, Default)]
pub struct McpOptions {
    /// Whether the MCP server is mounted at all (CFG-13).
    pub enabled: bool,
    /// Only these services' Routes become tools; `None` allows every service (MCP-06).
    pub allowed_services: Option<Vec<String>>,
    /// Only Routes with one of these operation ids become tools; Routes without an
    /// operation id always pass; `None` allows every Route (MCP-06).
    pub allowed_operations: Option<Vec<String>>,
    /// The `filter` callback (MCP-06).
    pub filter: Option<ToolFilter>,
    /// The `parameterFilter` callback (MCP-10).
    pub parameter_filter: Option<ParameterFilter>,
    /// The `toolName` callback (MCP-07).
    pub tool_name: Option<ToolNamer>,
    /// The server identity (MCP-03).
    pub server_info: ServerInfo,
}

impl McpOptions {
    /// Enabled options with every default.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Sets the `filter` callback.
    pub fn with_filter(
        mut self,
        filter: impl Fn(&str, &Route) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filter = Some(Arc::new(filter));
        self
    }

    /// Sets the `parameterFilter` callback.
    pub fn with_parameter_filter(
        mut self,
        filter: impl Fn(&Value, &ParameterFilterContext<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.parameter_filter = Some(Arc::new(filter));
        self
    }

    /// Sets the `toolName` callback.
    pub fn with_tool_name(
        mut self,
        namer: impl Fn(&ToolNameContext<'_>) -> String + Send + Sync + 'static,
    ) -> Self {
        self.tool_name = Some(Arc::new(namer));
        self
    }
}

impl fmt::Debug for McpOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpOptions")
            .field("enabled", &self.enabled)
            .field("allowed_services", &self.allowed_services)
            .field("allowed_operations", &self.allowed_operations)
            .field("filter", &self.filter.is_some())
            .field("parameter_filter", &self.parameter_filter.is_some())
            .field("tool_name", &self.tool_name.is_some())
            .field("server_info", &self.server_info)
            .finish()
    }
}
