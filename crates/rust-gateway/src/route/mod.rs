//! Routes and the per-method Route table.
//!
//! A Route is one operation of an Upstream Service's OpenAPI document (OAS-07, OAS-08).
//! The table groups Routes by method, in document order (OAS-06); a lookup takes the first
//! Route of the requested method whose pattern matches the request path (OAS-18 to OAS-22).

use std::fmt;
use std::sync::Arc;

use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map, Value};

pub mod pattern;
pub mod uri;

pub use pattern::{ParamDecodeError, PathError, PathMatch, PathPattern};
pub use uri::{InvalidUrl, UriError};

/// The eight methods the Reference Gateway builds Routes for, in its order (OAS-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RouteMethod {
    /// `get`
    Get,
    /// `post`
    Post,
    /// `delete`
    Delete,
    /// `patch`
    Patch,
    /// `put`
    Put,
    /// `options`
    Options,
    /// `head`
    Head,
    /// `search`
    Search,
}

impl RouteMethod {
    /// Every method, in the reference's order.
    pub const ALL: [RouteMethod; 8] = [
        RouteMethod::Get,
        RouteMethod::Post,
        RouteMethod::Delete,
        RouteMethod::Patch,
        RouteMethod::Put,
        RouteMethod::Options,
        RouteMethod::Head,
        RouteMethod::Search,
    ];

    /// The lower-case name, as used for path-item keys in OpenAPI documents.
    pub fn as_str(self) -> &'static str {
        match self {
            RouteMethod::Get => "get",
            RouteMethod::Post => "post",
            RouteMethod::Delete => "delete",
            RouteMethod::Patch => "patch",
            RouteMethod::Put => "put",
            RouteMethod::Options => "options",
            RouteMethod::Head => "head",
            RouteMethod::Search => "search",
        }
    }

    /// The method a path-item key names. Keys are compared exactly: `GET` is not a method
    /// key, and is skipped like any other non-method key (OAS-17).
    pub fn from_path_item_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == key)
    }

    /// The method of a request, compared case-insensitively (OAS-22). Methods outside the
    /// eight have no Routes.
    pub fn from_request_method(method: &str) -> Option<Self> {
        Self::from_path_item_key(&method.to_lowercase())
    }

    fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for RouteMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of an operation's `x-rate-limits` (OAS-11): at most `limit` requests per
/// `ttl` seconds, counting only responses with `status` when it is set.
///
/// The reference keeps the JSON values as they are and compares them in JavaScript and Lua.
/// [`RouteLimit::from_json`] normalises them without changing what the reference does:
///
/// - `limit` is rounded up to an integer and negative values become 0. Counters are
///   integers, so `count >= 2.5` and `count >= 3` agree, and every limit at or below 0
///   behaves like 0 (the first request in a window is always counted, later ones are
///   rejected; on the status path every request is rejected).
/// - `ttl` is kept as given, in seconds, including fractions and negative values (the
///   reference passes `Math.floor(ttl * 1000)` milliseconds to Redis, which rejects an
///   expiry at or below zero).
/// - `status` absent, `null`, `0`, `false` or `""` means no status. An integer from 1 to
///   65535 is kept. Any other truthy value (a fraction, a negative number, a string such
///   as `"201"`, `true`, an array or an object) can never equal a response status in the
///   reference's `===` comparison but still selects the status path; it is kept as
///   `Some(u16::MAX)`, which no response carries.
///
/// Numeric strings are read as numbers, as JavaScript and Lua coerce them. A `limit` or
/// `ttl` that is missing or not numeric becomes `0` or `NaN`; the reference answers 500 or
/// 429 for such limits depending on the path, and they are not supported further.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteLimit {
    /// Requests allowed per window.
    pub limit: u64,
    /// Window length in seconds.
    pub ttl: f64,
    /// Count only responses with this status; `None` counts every request.
    pub status: Option<u16>,
}

impl RouteLimit {
    /// Reads one `x-rate-limits` entry. Entries that are not JSON objects give `None` and
    /// are skipped (the reference fails every request on the Route instead).
    pub fn from_json(value: &Value) -> Option<Self> {
        let Value::Object(entry) = value else {
            return None;
        };
        let limit = entry.get("limit").and_then(js_number).unwrap_or(0.0);
        // `as` saturates: NaN becomes 0, values beyond u64 become u64::MAX.
        let limit = limit.ceil().max(0.0) as u64;
        let ttl = entry.get("ttl").and_then(js_number).unwrap_or(f64::NAN);
        let status = match entry.get("status") {
            None => None,
            Some(value) if !js_truthy(value) => None,
            Some(Value::Number(n)) => Some(
                n.as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .unwrap_or(u16::MAX),
            ),
            Some(_) => Some(u16::MAX),
        };
        Some(Self { limit, ttl, status })
    }
}

impl serde::Serialize for RouteLimit {
    /// `{ "limit", "ttl", "status"? }`, with an integral `ttl` written as an integer, as
    /// JavaScript writes it.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("limit", &self.limit)?;
        let integral = self.ttl.is_finite()
            && self.ttl.fract() == 0.0
            && self.ttl.abs() < 9_007_199_254_740_992.0;
        if integral {
            // Exact: the value is integral and within ±2^53.
            map.serialize_entry("ttl", &(self.ttl as i64))?;
        } else {
            map.serialize_entry("ttl", &self.ttl)?;
        }
        if let Some(status) = self.status {
            map.serialize_entry("status", &status)?;
        }
        map.end()
    }
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A JSON number, or a string holding a decimal number.
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// One operation of an Upstream Service's document (OAS-08).
///
/// Field mapping from the reference's `RouterDetail` (API-03): `operationId` →
/// [`operation_id`](Self::operation_id), `description` → [`description`](Self::description),
/// `path` → [`path`](Self::path), `routerPath` → [`router_path`](Self::router_path),
/// `pathMatch` → [`pattern`](Self::pattern), `rateLimits` →
/// [`rate_limits`](Self::rate_limits), `isBearerAuth` →
/// [`is_bearer_auth`](Self::is_bearer_auth), `isApiKeyAuth` →
/// [`is_api_key_auth`](Self::is_api_key_auth), `extra` → [`extra`](Self::extra). The
/// [`method`](Self::method) is implied by the table a reference Route sits in.
#[derive(Debug, Clone)]
pub struct Route {
    /// The method whose table the Route belongs to.
    pub method: RouteMethod,
    /// `operationId`, `None` when absent or `null`. A non-string value is kept as its JSON
    /// text.
    pub operation_id: Option<String>,
    /// `description`, or an empty string when absent or `null`. A non-string value is kept
    /// as its JSON text.
    pub description: String,
    /// The OpenAPI path template, for example `/v1/users/{id}`.
    pub path: String,
    /// The match pattern's text: `x-router-path`, or the path with `{name}` turned into
    /// `:name` (OAS-10). A list of patterns is written as JavaScript joins it, with commas.
    pub router_path: String,
    /// The compiled match pattern.
    pub pattern: PathPattern,
    /// `x-rate-limits` (OAS-11), empty when absent.
    pub rate_limits: Vec<RouteLimit>,
    /// Whether a security requirement names `bearer` or a configured bearer key (OAS-12).
    pub is_bearer_auth: bool,
    /// Whether a security requirement names a configured API-key name (OAS-13).
    pub is_api_key_auth: bool,
    /// Every other camel-cased operation key (OAS-14), in JavaScript key order.
    pub extra: Map<String, Value>,
}

impl Route {
    /// Whether the Route matches a path, as `pathMatch(path)` does. A matched parameter
    /// that is not valid percent-encoded UTF-8 is an error, as in the reference.
    pub fn match_path(&self, path: &str) -> Result<bool, ParamDecodeError> {
        self.pattern.matches(path).map(|m| m.is_some())
    }
}

/// Why a Route lookup failed. In the reference both are unhandled exceptions, which the
/// request pipeline turns into a 500 response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteLookupError {
    /// The request URL cannot be parsed (`TypeError: Invalid URL`).
    #[error(transparent)]
    InvalidUrl(#[from] InvalidUrl),
    /// A matched parameter cannot be percent-decoded (`URIError: URI malformed`).
    #[error(transparent)]
    Decode(#[from] ParamDecodeError),
}

/// The Routes of one document, grouped by method in document order (the reference's
/// `RouterPathType`, which is not public: API-05).
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    by_method: [Vec<Arc<Route>>; 8],
}

impl RouteTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a Route to its method's list.
    pub fn push(&mut self, route: Route) {
        self.by_method[route.method.index()].push(Arc::new(route));
    }

    /// The Routes of one method, in document order.
    pub fn routes(&self, method: RouteMethod) -> &[Arc<Route>] {
        &self.by_method[method.index()]
    }

    /// Every Route, method by method in [`RouteMethod::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Route>> {
        self.by_method.iter().flatten()
    }

    /// The number of Routes.
    pub fn len(&self) -> usize {
        self.by_method.iter().map(Vec::len).sum()
    }

    /// Whether the table has no Routes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The reference's `getRouterDetail`: the first Route of `method` (compared
    /// case-insensitively) whose pattern matches the path of `url` (its query and fragment
    /// ignored), or `None`.
    ///
    /// A method outside the eight has no Routes and gives `None` (in the reference it is an
    /// unhandled error; Deviation D4 answers such requests with 405).
    pub fn lookup(&self, method: &str, url: &str) -> Result<Option<Arc<Route>>, RouteLookupError> {
        let Some(method) = RouteMethod::from_request_method(method) else {
            return Ok(None);
        };
        let path = uri::path_from_url(url)?;
        for route in self.routes(method) {
            if route.match_path(&path)? {
                return Ok(Some(Arc::clone(route)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(method: RouteMethod, path: &str, pattern: &str, id: &str) -> Route {
        Route {
            method,
            operation_id: Some(id.to_string()),
            description: String::new(),
            path: path.to_string(),
            router_path: pattern.to_string(),
            pattern: PathPattern::compile(pattern).unwrap(),
            rate_limits: Vec::new(),
            is_bearer_auth: false,
            is_api_key_auth: false,
            extra: Map::new(),
        }
    }

    fn table() -> RouteTable {
        let mut table = RouteTable::new();
        table.push(route(RouteMethod::Get, "/users/{id}", "/users/:id", "show"));
        table.push(route(RouteMethod::Get, "/users/me", "/users/me", "me"));
        table.push(route(
            RouteMethod::Delete,
            "/users/{id}",
            "/users/:id",
            "delete",
        ));
        table
    }

    fn found(table: &RouteTable, method: &str, url: &str) -> Option<String> {
        table
            .lookup(method, url)
            .unwrap()
            .and_then(|r| r.operation_id.clone())
    }

    // Ported from open-api.service.spec.ts, `OpenApiService.getRouterDetail`.
    #[test]
    fn oas_18_finds_the_route_ignoring_the_query_string() {
        assert_eq!(
            found(&table(), "GET", "/users/42?full=true").as_deref(),
            Some("show")
        );
    }

    #[test]
    fn oas_21_returns_none_when_no_route_matches() {
        assert_eq!(found(&table(), "POST", "/users/42"), None);
    }

    #[test]
    fn oas_21_first_match_in_document_order_wins() {
        assert_eq!(found(&table(), "GET", "/users/me").as_deref(), Some("show"));
    }

    #[test]
    fn oas_22_method_is_case_insensitive() {
        assert_eq!(found(&table(), "get", "/users/1").as_deref(), Some("show"));
        assert_eq!(
            found(&table(), "Delete", "/users/1").as_deref(),
            Some("delete")
        );
        assert_eq!(found(&table(), "TRACE", "/users/1"), None);
    }

    #[test]
    fn oas_19_lookup_errors() {
        let table = table();
        assert!(matches!(
            table.lookup("GET", "/users/%zz"),
            Err(RouteLookupError::Decode(_))
        ));
        assert!(matches!(
            table.lookup("GET", ":99999"),
            Err(RouteLookupError::InvalidUrl(_))
        ));
    }

    #[test]
    fn oas_06_methods() {
        let names: Vec<&str> = RouteMethod::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(
            names,
            [
                "get", "post", "delete", "patch", "put", "options", "head", "search"
            ]
        );
        assert_eq!(RouteMethod::from_path_item_key("GET"), None);
        assert_eq!(RouteMethod::from_path_item_key("trace"), None);
        assert_eq!(
            RouteMethod::from_request_method("SEARCH"),
            Some(RouteMethod::Search)
        );
    }

    #[test]
    fn oas_11_route_limit_numbers() {
        let read = |v: Value| RouteLimit::from_json(&v);
        assert_eq!(
            read(json!({"limit": 5, "ttl": 60})),
            Some(RouteLimit {
                limit: 5,
                ttl: 60.0,
                status: None
            })
        );
        assert_eq!(
            read(json!({"limit": 1, "ttl": 60, "status": 201})),
            Some(RouteLimit {
                limit: 1,
                ttl: 60.0,
                status: Some(201)
            })
        );
        // Fractions round up; negative and zero limits behave like 0.
        assert_eq!(read(json!({"limit": 2.5, "ttl": 1.5})).unwrap().limit, 3);
        assert_eq!(read(json!({"limit": 2.5, "ttl": 1.5})).unwrap().ttl, 1.5);
        assert_eq!(read(json!({"limit": -3, "ttl": 60})).unwrap().limit, 0);
        assert_eq!(
            read(json!({"limit": 1e300, "ttl": 60})).unwrap().limit,
            u64::MAX
        );
        // ttl is kept as given.
        assert_eq!(read(json!({"limit": 1, "ttl": -5})).unwrap().ttl, -5.0);
        // Falsy status means none; other truthy values never match a response.
        for falsy in [json!(0), json!(null), json!(false), json!("")] {
            assert_eq!(
                read(json!({"limit": 1, "ttl": 1, "status": falsy}))
                    .unwrap()
                    .status,
                None
            );
        }
        for unmatchable in [
            json!(200.5),
            json!(-1),
            json!(70000),
            json!("201"),
            json!(true),
            json!([]),
        ] {
            assert_eq!(
                read(json!({"limit": 1, "ttl": 1, "status": unmatchable}))
                    .unwrap()
                    .status,
                Some(u16::MAX)
            );
        }
        // Numeric strings are numbers; missing values are 0 and NaN.
        assert_eq!(read(json!({"limit": "4", "ttl": "30"})).unwrap().limit, 4);
        assert_eq!(read(json!({"limit": "4", "ttl": "30"})).unwrap().ttl, 30.0);
        let missing = read(json!({})).unwrap();
        assert_eq!(missing.limit, 0);
        assert!(missing.ttl.is_nan());
        assert_eq!(read(json!(null)), None);
        assert_eq!(read(json!(5)), None);
    }

    #[test]
    fn route_limit_serialises_like_javascript() {
        let limit = RouteLimit {
            limit: 5,
            ttl: 60.0,
            status: None,
        };
        assert_eq!(
            serde_json::to_string(&limit).unwrap(),
            r#"{"limit":5,"ttl":60}"#
        );
        let limit = RouteLimit {
            limit: 1,
            ttl: 1.5,
            status: Some(201),
        };
        assert_eq!(
            serde_json::to_string(&limit).unwrap(),
            r#"{"limit":1,"ttl":1.5,"status":201}"#
        );
    }
}
