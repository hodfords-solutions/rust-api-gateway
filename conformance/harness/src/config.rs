//! The conformance configuration file shared by both targets.
//!
//! The runner serialises a [`GatewayConfig`] to JSON and passes its path in `GATEWAY_CONFIG`.
//! The reference app (`conformance/reference/src/config.ts`) and `rust-gateway-server` read
//! the same file. Field names follow the Reference Gateway's `ApiGatewayOption`.

use serde::Serialize;
use serde_json::Value;

use crate::mock::MockUpstream;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    /// Filled in by the runner.
    pub port: u16,
    /// Express `trust proxy`. `None` means the reference sample's
    /// `["loopback", "linklocal", "uniquelocal"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_proxy: Option<Value>,
    pub api_services: Vec<UpstreamService>,
    pub exclude_headers: Vec<String>,
    pub open_api_security_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_api_security_api_keys: Option<Vec<String>>,
    pub throttler: Throttler,
    pub restful: Restful,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scalar_options: Option<Value>,
    /// Filled in by the runner unless set.
    pub redis: Redis,
    /// Passed to undici's `Pool` unchanged.
    pub pool: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bypass_route_prefixes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Mcp>,
    /// Harness field: the host application's `http.Server` timeouts in milliseconds
    /// (`headersTimeout`, `requestTimeout`, `keepAliveTimeout`, `keepAliveTimeoutBuffer`,
    /// `connectionsCheckingInterval`). Absent means Node's defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamService {
    pub prefix: String,
    pub doc_url: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_prefixes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Throttler {
    #[serde(rename = "globalIpRateLimit")]
    pub global_ip_rate_limit: f64,
    #[serde(rename = "globalIpRateLimitTTL")]
    pub global_ip_rate_limit_ttl: f64,
    #[serde(rename = "globalCustomRateLimit")]
    pub global_custom_rate_limit: f64,
    #[serde(rename = "globalCustomRateLimitTTL")]
    pub global_custom_rate_limit_ttl: f64,
    #[serde(rename = "isEnable")]
    pub is_enable: bool,
    #[serde(
        rename = "anonymousIpRateLimit",
        skip_serializing_if = "Option::is_none"
    )]
    pub anonymous_ip_rate_limit: Option<f64>,
    #[serde(
        rename = "anonymousIpRateLimitTTL",
        skip_serializing_if = "Option::is_none"
    )]
    pub anonymous_ip_rate_limit_ttl: Option<f64>,
    #[serde(rename = "ipv6Subnet", skip_serializing_if = "Option::is_none")]
    pub ipv6_subnet: Option<u32>,
    /// `Some("protocol")` installs the test Identity resolver.
    #[serde(rename = "keyResolver", skip_serializing_if = "Option::is_none")]
    pub key_resolver: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Restful {
    pub is_enable_document: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_document_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Redis {
    pub host: String,
    pub port: u16,
    pub db: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mcp {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_services: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_operations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<Value>,
    /// `{ "rejectServices": [...], "rejectPathPrefixes": [...] }`: the `filter` callback of
    /// the MCP callback protocol (`conformance/README.md`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// `{ "rejectNames": [...], "rejectIn": [...] }`: the `parameterFilter` callback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_filter: Option<Value>,
    /// A template such as `"{serviceName}.{operationId}"`: the `toolName` callback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl GatewayConfig {
    /// The reference sample's settings (EX-04) with limits high enough not to interfere,
    /// the test Identity resolver, documents and MCP enabled, and no services.
    pub fn new() -> Self {
        Self {
            port: 0,
            trust_proxy: None,
            api_services: Vec::new(),
            exclude_headers: vec!["auth-user-id".into(), "permission-in-any-guard".into()],
            open_api_security_keys: vec!["auth-user-id".into()],
            open_api_security_api_keys: Some(vec!["x-api-key".into()]),
            throttler: Throttler {
                global_ip_rate_limit: 10_000.0,
                global_ip_rate_limit_ttl: 60.0,
                global_custom_rate_limit: 10_000.0,
                global_custom_rate_limit_ttl: 60.0,
                is_enable: true,
                anonymous_ip_rate_limit: None,
                anonymous_ip_rate_limit_ttl: None,
                ipv6_subnet: None,
                key_resolver: Some("protocol".into()),
            },
            restful: Restful {
                is_enable_document: true,
                hide_document_ids: None,
            },
            scalar_options: None,
            redis: Redis::default(),
            pool: serde_json::json!({ "connections": 100, "pipelining": 1 }),
            bypass_route_prefixes: None,
            mcp: Some(Mcp {
                enabled: true,
                allowed_services: None,
                allowed_operations: None,
                server_info: None,
                filter: None,
                parameter_filter: None,
                tool_name: None,
            }),
            server: None,
        }
    }

    /// Adds an Upstream Service served by `mock`.
    pub fn service(self, prefix: &str, mock: &MockUpstream) -> Self {
        self.service_with_direct(prefix, mock, &[])
    }

    /// Adds an Upstream Service served by `mock` that owns `direct_prefixes`.
    pub fn service_with_direct(
        mut self,
        prefix: &str,
        mock: &MockUpstream,
        direct_prefixes: &[&str],
    ) -> Self {
        self.api_services.push(UpstreamService {
            prefix: prefix.to_string(),
            doc_url: mock.doc_url(),
            host: mock.origin(),
            direct_prefixes: if direct_prefixes.is_empty() {
                None
            } else {
                Some(direct_prefixes.iter().map(|p| p.to_string()).collect())
            },
        });
        self
    }

    pub fn throttler(mut self, f: impl FnOnce(&mut Throttler)) -> Self {
        f(&mut self.throttler);
        self
    }

    /// Sets the `http.Server` timeouts (see [`GatewayConfig::server`]).
    pub fn server(mut self, server: Value) -> Self {
        self.server = Some(server);
        self
    }

    pub fn pool(mut self, pool: Value) -> Self {
        self.pool = pool;
        self
    }

    /// Changes the `mcp` section (created enabled with defaults when absent).
    pub fn mcp(mut self, f: impl FnOnce(&mut Mcp)) -> Self {
        let mcp = self.mcp.get_or_insert_with(|| Mcp {
            enabled: true,
            allowed_services: None,
            allowed_operations: None,
            server_info: None,
            filter: None,
            parameter_filter: None,
            tool_name: None,
        });
        f(mcp);
        self
    }

    pub fn scalar_options(mut self, options: Value) -> Self {
        self.scalar_options = Some(options);
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("config serialises")
    }
}
