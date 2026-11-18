//! The tool registry (MCP-05 to MCP-13): the reference's `McpToolRegistryService`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;

use super::js;
use super::options::{McpOptions, ToolNameContext};
use super::schema::{SchemaBuilder, default_tool_name};
use crate::openapi::DocumentStore;
use crate::route::{Route, RouteMethod};

/// How often the registry is rebuilt (MCP-05). The reference uses the cron expression
/// "every 5 minutes", so rebuilds happen on wall-clock multiples of this interval.
pub const TOOL_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Tool names longer than this are logged as a warning and used as they are (MCP-08).
pub const MAX_TOOL_NAME_LENGTH: usize = 64;

/// One MCP tool: a Route of an Upstream Service (the reference's `McpToolDefinition`).
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// The tool name (MCP-07).
    pub name: String,
    /// The Route's description, or `<METHOD> <path>` (MCP-09).
    pub description: String,
    /// The JSON Schema of the arguments (MCP-10 to MCP-12).
    pub input_schema: Map<String, Value>,
    /// The Service Prefix of the Upstream Service.
    pub service: String,
    /// The OpenAPI path, for example `/users/{id}`.
    pub path: String,
    /// The Route the tool calls.
    pub route: Arc<Route>,
}

impl ToolDefinition {
    /// The HTTP method of the upstream request.
    pub fn method(&self) -> RouteMethod {
        self.route.method
    }
}

/// The result of one rebuild: the tools, and the warnings the reference logs while building
/// them (long names, unmappable types).
#[derive(Debug, Clone, Default)]
pub struct ToolBuild {
    /// The tools, in the reference's order: services in first-load order, then methods in
    /// Route-table order, then Routes in document order.
    pub tools: Vec<ToolDefinition>,
    /// Warning messages, in the order the reference logs them.
    pub warnings: Vec<String>,
}

/// Builds every tool from the loaded documents (MCP-06 to MCP-12).
///
/// The Routes come from each service's last successfully built document, and the schemas
/// from its last fetched original document, which can be newer: as in the reference, an
/// operation missing from the original document gets the schema
/// `{"type":"object","properties":{},"required":[]}`.
pub fn build_tools(store: &DocumentStore, options: &McpOptions) -> ToolBuild {
    build_tools_with(store, options, true)
}

pub(crate) fn build_tools_with(
    store: &DocumentStore,
    options: &McpOptions,
    resolve_repeated_refs: bool,
) -> ToolBuild {
    let mut build = ToolBuild::default();
    // `for (serviceName in apiDocs)`: JavaScript key order of the load order.
    let mut services = js::JsMap::new();
    for prefix in store.loaded_services() {
        services.insert(prefix, ());
    }
    for (service, ()) in services.into_entries() {
        if let Some(allowed) = &options.allowed_services
            && !allowed.contains(&service)
        {
            continue;
        }
        let Some(document) = store.document(&service) else {
            continue;
        };
        let original = store
            .original_document(&service)
            .unwrap_or_else(|| Arc::new(Value::Null));
        let mut schemas = SchemaBuilder::new(&original);
        schemas.resolve_repeated_refs = resolve_repeated_refs;
        for method in RouteMethod::ALL {
            for route in document.routes.routes(method) {
                let operation_id = route.operation_id.as_deref();
                if let (Some(allowed), Some(id)) = (&options.allowed_operations, operation_id)
                    && !id.is_empty()
                    && !allowed.iter().any(|a| a == id)
                {
                    continue;
                }
                if let Some(filter) = &options.filter
                    && !filter(&service, route)
                {
                    continue;
                }
                let name = tool_name(
                    &service,
                    method.as_str(),
                    operation_id,
                    &route.path,
                    options,
                );
                // `toolName.length` counts UTF-16 code units.
                let length = name.encode_utf16().count();
                if length > MAX_TOOL_NAME_LENGTH {
                    build.warnings.push(format!(
                        "MCP tool name \"{name}\" is {length} characters, which exceeds the recommended maximum of {MAX_TOOL_NAME_LENGTH}."
                    ));
                }
                let input_schema =
                    schemas.input_schema(&route.path, method.as_str(), &service, options);
                let description = if route.description.is_empty() {
                    format!("{} {}", method.as_str().to_uppercase(), route.path)
                } else {
                    route.description.clone()
                };
                build.tools.push(ToolDefinition {
                    name,
                    description,
                    input_schema,
                    service: service.clone(),
                    path: route.path.clone(),
                    route: Arc::clone(route),
                });
            }
        }
        build.warnings.append(&mut schemas.warnings);
    }
    build
}

fn tool_name(
    service: &str,
    method: &str,
    operation_id: Option<&str>,
    path: &str,
    options: &McpOptions,
) -> String {
    let default_name = default_tool_name(service, method, operation_id, path);
    match &options.tool_name {
        Some(namer) => namer(&ToolNameContext {
            service_name: service,
            method,
            operation_id,
            path,
            default_name: &default_name,
        }),
        None => default_name,
    }
}

/// The current tools, rebuilt from a [`DocumentStore`] (MCP-05).
pub struct ToolRegistry {
    store: Arc<DocumentStore>,
    options: Arc<McpOptions>,
    tools: ArcSwap<Vec<Arc<ToolDefinition>>>,
}

impl ToolRegistry {
    /// An empty registry over `store`. Call [`refresh`](Self::refresh) or
    /// [`start`](Self::start) to fill it.
    pub fn new(store: Arc<DocumentStore>, options: Arc<McpOptions>) -> Self {
        Self {
            store,
            options,
            tools: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// The options the registry builds with.
    pub fn options(&self) -> &McpOptions {
        &self.options
    }

    /// Every tool, in order (MCP-13).
    pub fn tools(&self) -> Arc<Vec<Arc<ToolDefinition>>> {
        self.tools.load_full()
    }

    /// The first tool with this exact name (MCP-08, MCP-14).
    pub fn tool(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        self.tools.load().iter().find(|t| t.name == name).cloned()
    }

    /// Rebuilds the tools now, logs the warnings and the count, and returns the count.
    pub fn refresh(&self) -> usize {
        let build = build_tools(&self.store, &self.options);
        for warning in &build.warnings {
            tracing::warn!(target: "rust_gateway::mcp", "{warning}");
        }
        let count = build.tools.len();
        self.tools
            .store(Arc::new(build.tools.into_iter().map(Arc::new).collect()));
        tracing::info!(target: "rust_gateway::mcp", "Refreshed MCP tool registry: {count} tools available");
        count
    }

    /// Starts rebuilding in the background (MCP-05): once when the first document load has
    /// settled ([`DocumentStore::ready`]) and on every wall-clock multiple of
    /// [`TOOL_REFRESH_INTERVAL`], independently of readiness, as the reference's cron job
    /// does. Must run inside a Tokio runtime; dropping the handle stops it.
    pub fn start(self: &Arc<Self>) -> RegistryHandle {
        self.start_with_interval(TOOL_REFRESH_INTERVAL)
    }

    /// [`start`](Self::start) with another interval, for tests.
    pub fn start_with_interval(self: &Arc<Self>, interval: Duration) -> RegistryHandle {
        let registry = Arc::clone(self);
        let first = tokio::spawn(async move {
            registry.store.ready().await;
            registry.refresh();
        });
        let registry = Arc::clone(self);
        let periodic = tokio::spawn(async move {
            loop {
                tokio::time::sleep(until_next_multiple(interval)).await;
                registry.refresh();
            }
        });
        RegistryHandle {
            tasks: vec![first, periodic],
        }
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.load().len())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// The time until the next wall-clock multiple of `interval`.
fn until_next_multiple(interval: Duration) -> Duration {
    let interval_ms = interval.as_millis().max(1);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wait = interval_ms - now_ms % interval_ms;
    Duration::from_millis(u64::try_from(wait).unwrap_or(u64::MAX))
}

/// Keeps the background rebuilds running; dropping it stops them.
#[derive(Debug)]
pub struct RegistryHandle {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for RegistryHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
