//! The conformance configuration file (`conformance/README.md`), read exactly as
//! `conformance/reference/src/config.ts` reads it, and the reference sample's environment
//! configuration (EX-01, EX-02, EX-04).

use rust_gateway::gateway::{DocumentOptions, GatewayOptions, McpOptions, PoolOptions};
use rust_gateway::rate_limit::{
    BucketLimit, RateLimitOptions, RedisOptions, RedisTls, RedisTlsOptions, TrustProxy,
};
use rust_gateway::upstream::UpstreamService;
use serde::Deserialize;
use serde_json::{Value, json};

/// A configuration error, reported before the server starts.
pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;

/// The file's contents (field names as in the Reference Gateway's `ApiGatewayOption`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConformanceConfig {
    pub port: f64,
    #[serde(default)]
    pub trust_proxy: Option<Value>,
    pub api_services: Vec<UpstreamService>,
    pub exclude_headers: Vec<String>,
    pub open_api_security_keys: Vec<String>,
    #[serde(default)]
    pub open_api_security_api_keys: Option<Vec<String>>,
    pub throttler: Throttler,
    pub restful: Restful,
    #[serde(default)]
    pub scalar_options: Option<Value>,
    pub redis: Redis,
    #[serde(default)]
    pub pool: Value,
    #[serde(default)]
    pub bypass_route_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub mcp: Option<Mcp>,
}

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(rename = "anonymousIpRateLimit", default)]
    pub anonymous_ip_rate_limit: Option<f64>,
    #[serde(rename = "anonymousIpRateLimitTTL", default)]
    pub anonymous_ip_rate_limit_ttl: Option<f64>,
    #[serde(rename = "ipv6Subnet", default)]
    pub ipv6_subnet: Option<u32>,
    /// `"protocol"` installs the test Identity resolver.
    #[serde(rename = "keyResolver", default)]
    pub key_resolver: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Restful {
    pub is_enable_document: bool,
    #[serde(default)]
    pub hide_document_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Redis {
    pub host: String,
    pub port: f64,
    #[serde(default)]
    pub db: Option<f64>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub tls: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mcp {
    pub enabled: bool,
    #[serde(default)]
    pub allowed_services: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_operations: Option<Vec<String>>,
    #[serde(default)]
    pub server_info: Option<Value>,
}

/// What the binary runs.
pub struct Settings {
    /// The listen port.
    pub port: u16,
    /// The gateway options.
    pub options: GatewayOptions,
    /// Whether the protocol Identity resolver is installed.
    pub protocol_resolver: bool,
}

/// Reads an environment variable, treating an empty value as unset (JavaScript truthiness).
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// JavaScript's `Number(text)` for the integers used here.
fn number(name: &str, text: &str) -> Result<f64, ConfigError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(0.0);
    }
    trimmed
        .parse::<f64>()
        .map_err(|_| format!("{name} must be a number, not {text:?}").into())
}

fn port(name: &str, value: f64) -> Result<u16, ConfigError> {
    if value.fract() == 0.0 && (0.0..=65535.0).contains(&value) {
        Ok(value as u16)
    } else {
        Err(format!("{name} must be a port number, not {value}").into())
    }
}

/// A limit as the reference's Lua script compares it with integer counters.
fn limit(value: f64) -> u64 {
    if value.is_finite() {
        value.ceil().max(0.0) as u64
    } else {
        0
    }
}

/// Express's `trust proxy` setting (RL-06). Absent means the reference sample's presets.
pub fn trust_proxy(value: Option<&Value>) -> Result<TrustProxy, ConfigError> {
    Ok(match value {
        None | Some(Value::Null) => TrustProxy::List(vec![
            "loopback".into(),
            "linklocal".into(),
            "uniquelocal".into(),
        ]),
        Some(Value::Bool(false)) => TrustProxy::None,
        Some(Value::Bool(true)) => TrustProxy::All,
        Some(Value::Number(n)) => TrustProxy::Hops(
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("trustProxy hop count must be a non-negative integer")?,
        ),
        Some(Value::String(s)) => TrustProxy::from_comma_separated(s),
        Some(Value::Array(items)) => TrustProxy::List(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or("trustProxy entries must be strings")
                })
                .collect::<Result<_, _>>()?,
        ),
        Some(other) => return Err(format!("unsupported trustProxy value {other}").into()),
    })
}

fn read_pem(path: Option<&Value>) -> Result<Option<Vec<u8>>, ConfigError> {
    match path.and_then(Value::as_str).filter(|p| !p.is_empty()) {
        Some(path) => Ok(Some(
            std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?,
        )),
        None => Ok(None),
    }
}

/// The file's `redis.tls`: `true`, `false`, or `{ caFile, certFile, keyFile, servername,
/// rejectUnauthorized }` naming PEM files, as `redisTls()` in `config.ts` reads it.
pub fn redis_tls(value: Option<&Value>) -> Result<RedisTls, ConfigError> {
    Ok(match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => RedisTls::Disabled,
        Some(Value::Bool(true)) => RedisTls::Default,
        Some(Value::Object(tls)) => RedisTls::Custom(RedisTlsOptions {
            ca: read_pem(tls.get("caFile"))?,
            cert: read_pem(tls.get("certFile"))?,
            key: read_pem(tls.get("keyFile"))?,
            server_name: tls
                .get("servername")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            reject_unauthorized: tls.get("rejectUnauthorized") != Some(&Value::Bool(false)),
        }),
        Some(other) => return Err(format!("unsupported redis.tls value {other}").into()),
    })
}

impl ConformanceConfig {
    /// Reads the file named by `GATEWAY_CONFIG` and applies the environment overrides
    /// `APP_PORT`, `REDIS_HOST`, `REDIS_PORT` and `REDIS_DB`, as `config.ts` does.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        let mut config: ConformanceConfig =
            serde_json::from_str(&text).map_err(|e| format!("parsing {path}: {e}"))?;
        if let Some(value) = env("APP_PORT") {
            config.port = number("APP_PORT", &value)?;
        }
        if let Some(value) = env("REDIS_HOST") {
            config.redis.host = value;
        }
        if let Some(value) = env("REDIS_PORT") {
            config.redis.port = number("REDIS_PORT", &value)?;
        }
        if let Some(value) = env("REDIS_DB") {
            config.redis.db = Some(number("REDIS_DB", &value)?);
        }
        Ok(config)
    }

    /// The gateway options the file describes.
    pub fn into_settings(self) -> Result<Settings, ConfigError> {
        let throttler = &self.throttler;
        let mut rate_limit = RateLimitOptions::new(
            throttler.is_enable,
            BucketLimit::new(
                limit(throttler.global_ip_rate_limit),
                throttler.global_ip_rate_limit_ttl,
            ),
            BucketLimit::new(
                limit(throttler.global_custom_rate_limit),
                throttler.global_custom_rate_limit_ttl,
            ),
        );
        rate_limit.anonymous_ip_limit = throttler.anonymous_ip_rate_limit.map(limit);
        rate_limit.anonymous_ip_ttl = throttler.anonymous_ip_rate_limit_ttl;
        rate_limit.ipv6_subnet = throttler.ipv6_subnet;
        rate_limit.trust_proxy = trust_proxy(self.trust_proxy.as_ref())?;

        let mut redis = RedisOptions::new(
            self.redis.host.clone(),
            port("redis.port", self.redis.port)?,
        );
        redis.db = self.redis.db.map(|db| db as i64);
        redis.username = self.redis.username.clone();
        redis.password = self.redis.password.clone();
        redis.tls = redis_tls(self.redis.tls.as_ref())?;

        let mut options = GatewayOptions::new(self.api_services, rate_limit, redis);
        options.exclude_headers = self.exclude_headers;
        options.open_api_security_keys = self.open_api_security_keys;
        options.open_api_security_api_keys = self.open_api_security_api_keys.unwrap_or_default();
        options.pool = PoolOptions::from_undici(&self.pool);
        options.bypass_route_prefixes = self.bypass_route_prefixes.unwrap_or_default();
        options.documents = DocumentOptions {
            enabled: self.restful.is_enable_document,
            hide_document_ids: self.restful.hide_document_ids.unwrap_or_default(),
        };
        options.scalar_options = self.scalar_options;
        options.mcp = self.mcp.map(|mcp| McpOptions {
            enabled: mcp.enabled,
            allowed_services: mcp.allowed_services,
            allowed_operations: mcp.allowed_operations,
            server_info: mcp.server_info,
        });
        Ok(Settings {
            port: port("port", self.port)?,
            options,
            protocol_resolver: self.throttler.key_resolver.as_deref() == Some("protocol"),
        })
    }
}

/// The reference sample's configuration (EX-01, EX-02, EX-04), used when `GATEWAY_CONFIG` is
/// not set: `src/config/env.config.ts` and `src/app.module.ts`.
///
/// Environment: `APP_PORT` (default 3000), `REDIS_HOST` (`localhost`), `REDIS_PORT` (6379),
/// `REDIS_DB` (0), `REDIS_USERNAME`, `REDIS_PASSWORD`, `REDIS_TLS` (`true` enables TLS),
/// `REDIS_TLS_CA_FILE`, `REDIS_TLS_CERT_FILE`, `REDIS_TLS_KEY_FILE`, `REDIS_TLS_SERVERNAME`,
/// `REDIS_TLS_REJECT_UNAUTHORIZED` (`false` disables verification), and `API_SERVICES`: a
/// JSON list of Upstream Services replacing the sample's `user-service` (EX-02).
pub fn sample_settings() -> Result<Settings, ConfigError> {
    let app_port = env("APP_PORT")
        .map(|v| number("APP_PORT", &v))
        .transpose()?
        .filter(|p| *p != 0.0 && p.is_finite())
        .unwrap_or(3000.0);
    let services: Vec<UpstreamService> = match env("API_SERVICES") {
        Some(json) => serde_json::from_str(&json).map_err(|e| format!("API_SERVICES: {e}"))?,
        None => vec![
            UpstreamService::new(
                "user-service",
                "http://localhost:4020/documents-json",
                "http://localhost:4020",
            )
            .with_direct_prefixes(["oauth", "oidc"]),
        ],
    };
    let mut rate_limit =
        RateLimitOptions::new(true, BucketLimit::new(60, 60.0), BucketLimit::new(60, 60.0));
    rate_limit.trust_proxy = trust_proxy(None)?;

    let redis_port = number(
        "REDIS_PORT",
        &env("REDIS_PORT").unwrap_or_else(|| "6379".into()),
    )?;
    let mut redis = RedisOptions::new(
        env("REDIS_HOST").unwrap_or_else(|| "localhost".into()),
        port("REDIS_PORT", redis_port)?,
    );
    redis.db = Some(number("REDIS_DB", &env("REDIS_DB").unwrap_or_else(|| "0".into()))? as i64);
    redis.username = env("REDIS_USERNAME");
    redis.password = env("REDIS_PASSWORD");
    redis.tls = if std::env::var("REDIS_TLS").as_deref() == Ok("true") {
        let file = |name: &str| env(name).map(Value::String);
        redis_tls(Some(&json!({
            "caFile": file("REDIS_TLS_CA_FILE"),
            "certFile": file("REDIS_TLS_CERT_FILE"),
            "keyFile": file("REDIS_TLS_KEY_FILE"),
            "servername": file("REDIS_TLS_SERVERNAME"),
            "rejectUnauthorized": std::env::var("REDIS_TLS_REJECT_UNAUTHORIZED").as_deref() != Ok("false"),
        })))?
    } else {
        RedisTls::Disabled
    };

    let mut options = GatewayOptions::new(services, rate_limit, redis);
    options.open_api_security_keys = vec!["auth-user-id".into()];
    options.open_api_security_api_keys = vec!["x-api-key".into()];
    options.exclude_headers = vec!["auth-user-id".into(), "permission-in-any-guard".into()];
    options.scalar_options = Some(json!({ "showExtensions": true }));
    options.documents = DocumentOptions {
        enabled: true,
        hide_document_ids: vec![
            "AppController_getHello".into(),
            "AppController_getHealth".into(),
        ],
    };
    options.pool = PoolOptions {
        connections: Some(100),
        ..PoolOptions::default()
    };
    options.mcp = Some(McpOptions {
        enabled: true,
        ..McpOptions::default()
    });
    Ok(Settings {
        port: port("APP_PORT", app_port)?,
        options,
        protocol_resolver: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_documented_example_file() {
        let text = r#"{
          "port": 41234,
          "trustProxy": ["loopback", "linklocal", "uniquelocal"],
          "apiServices": [
            { "prefix": "svc", "docUrl": "http://127.0.0.1:5000/openapi.json", "host": "http://127.0.0.1:5000", "directPrefixes": ["oauth"] }
          ],
          "excludeHeaders": ["auth-user-id", "permission-in-any-guard"],
          "openApiSecurityKeys": ["auth-user-id"],
          "openApiSecurityApiKeys": ["x-api-key"],
          "throttler": {
            "globalIpRateLimit": 60, "globalIpRateLimitTTL": 60,
            "globalCustomRateLimit": 60, "globalCustomRateLimitTTL": 60,
            "isEnable": true,
            "anonymousIpRateLimit": 10, "anonymousIpRateLimitTTL": 60,
            "ipv6Subnet": 64,
            "keyResolver": "protocol"
          },
          "restful": { "isEnableDocument": true, "hideDocumentIds": [] },
          "scalarOptions": { "showExtensions": true },
          "redis": { "host": "127.0.0.1", "port": 6399, "db": 7, "username": null, "password": null, "tls": false },
          "pool": { "connections": 100, "pipelining": 1 },
          "bypassRoutePrefixes": ["svc/public"],
          "mcp": { "enabled": true, "allowedServices": ["svc"], "allowedOperations": [], "serverInfo": { "name": "x", "version": "1" } }
        }"#;
        let config: ConformanceConfig = serde_json::from_str(text).unwrap();
        let settings = config.into_settings().unwrap();
        assert_eq!(settings.port, 41234);
        assert!(settings.protocol_resolver);
        let options = settings.options;
        assert_eq!(options.api_services[0].direct_prefixes, ["oauth"]);
        assert_eq!(options.rate_limit.anonymous_ip_limit, Some(10));
        assert_eq!(options.redis.db, Some(7));
        assert_eq!(options.redis.tls, RedisTls::Disabled);
        assert_eq!(options.pool.connections, Some(100));
        assert_eq!(options.bypass_route_prefixes, ["svc/public"]);
        assert!(options.mcp.unwrap().enabled);
    }

    #[test]
    fn trust_proxy_values_map_like_express() {
        assert_eq!(trust_proxy(Some(&json!(false))).unwrap(), TrustProxy::None);
        assert_eq!(trust_proxy(Some(&json!(true))).unwrap(), TrustProxy::All);
        assert_eq!(trust_proxy(Some(&json!(2))).unwrap(), TrustProxy::Hops(2));
        assert_eq!(
            trust_proxy(Some(&json!("loopback, 10.0.0.0/8"))).unwrap(),
            TrustProxy::List(vec!["loopback".into(), "10.0.0.0/8".into()])
        );
        assert_eq!(
            trust_proxy(None).unwrap(),
            TrustProxy::List(vec![
                "loopback".into(),
                "linklocal".into(),
                "uniquelocal".into()
            ])
        );
    }
}
