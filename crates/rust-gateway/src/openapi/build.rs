//! Building the Route table from a document: the reference's `getEndpointDetail`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::camelcase::{camelcase_keys, js_key_order};
use crate::route::{PathError, PathPattern, Route, RouteLimit, RouteMethod, RouteTable, js_truthy};

/// The security scheme names that set a Route's flags: the reference's
/// `openApiSecurityKeys` and `openApiSecurityApiKeys` options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityKeys {
    /// Scheme names that set [`Route::is_bearer_auth`], besides `bearer` itself (OAS-12).
    pub bearer: Vec<String>,
    /// Scheme names that set [`Route::is_api_key_auth`] (OAS-13).
    pub api_key: Vec<String>,
}

/// A loaded document's Routes: the reference's `EndpointDetail`.
#[derive(Debug, Clone)]
pub struct ServiceDocument {
    /// `info.title` when it is a string.
    pub title: Option<String>,
    /// `info.version` when it is a string.
    pub version: Option<String>,
    /// The URL the document was fetched from.
    pub doc_url: String,
    /// The Routes.
    pub routes: RouteTable,
}

/// Why a document's Route table could not be built. The reference throws in the same
/// cases, so the document does not load: a service that never loaded stays without a
/// document, and one that loaded before keeps its previous Routes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The document is not a JSON object (for example a body that is not JSON at all).
    #[error("the document is not a JSON object")]
    NotAnObject,
    /// An operation is `null`.
    #[error("{method} {path}: the operation is null")]
    NullOperation {
        /// The OpenAPI path.
        path: String,
        /// The method key.
        method: RouteMethod,
    },
    /// `x-router-path` is neither a string nor a list of strings.
    #[error("{method} {path}: x-router-path must be a string or a list of strings")]
    RouterPathType {
        /// The OpenAPI path.
        path: String,
        /// The method key.
        method: RouteMethod,
    },
    /// `path-to-regexp` rejects the match pattern.
    #[error("{method} {path}: {source}")]
    Pattern {
        /// The OpenAPI path.
        path: String,
        /// The method key.
        method: RouteMethod,
        /// The library's error.
        #[source]
        source: PathError,
    },
    /// `security` is not a list, or a requirement that is checked is `null`.
    #[error("{method} {path}: security must be a list of requirement objects")]
    Security {
        /// The OpenAPI path.
        path: String,
        /// The method key.
        method: RouteMethod,
    },
}

static SWAGGER_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{([A-Za-z0-9_]+)\}")
        .unwrap_or_else(|error| panic!("invalid built-in pattern: {error}"))
});

/// The reference's `convertToExpressPath`: every `{name}` whose name is made of ASCII word
/// characters becomes `:name` (OAS-10). Other braces are left for `path-to-regexp`, which
/// reads them as optional groups.
pub fn convert_to_express_path(path: &str) -> String {
    SWAGGER_PARAM.replace_all(path, ":$1").into_owned()
}

/// Builds a document's Route table (OAS-06 to OAS-15).
///
/// Deviation D3 (OAS-17, OAS-23): path-item keys that are not one of the eight method
/// names are skipped, and `info` is optional. Everything else the reference rejects is
/// rejected too ([`BuildError`]).
pub fn build_service_document(
    doc_url: &str,
    document: &Value,
    security: &SecurityKeys,
) -> Result<ServiceDocument, BuildError> {
    let Value::Object(root) = document else {
        return Err(BuildError::NotAnObject);
    };
    let mut routes = RouteTable::new();
    // `for (const router in doc.paths)`: an object's keys in JavaScript order, an array's
    // indices; anything else has no path items with method keys.
    let path_items: Vec<(String, &Value)> = match root.get("paths") {
        Some(Value::Object(paths)) => js_key_order(paths.iter())
            .into_iter()
            .map(|(k, v)| (k.clone(), v))
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => Vec::new(),
    };
    for (path, item) in path_items {
        // Only objects have method keys; the keys of strings and arrays are indices.
        let Value::Object(operations) = item else {
            continue;
        };
        for (key, operation) in js_key_order(operations.iter()) {
            let Some(method) = RouteMethod::from_path_item_key(key) else {
                // Deviation D3: the reference throws here.
                continue;
            };
            routes.push(build_route(method, &path, operation, security)?);
        }
    }
    let info = root.get("info").and_then(Value::as_object);
    let info_string = |key: &str| {
        info.and_then(|info| info.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Ok(ServiceDocument {
        title: info_string("title"),
        version: info_string("version"),
        doc_url: doc_url.to_string(),
        routes,
    })
}

/// What `camelcaseKeys(operation)` gives for each kind of JSON value: the properties the
/// reference reads by name, and the entries `for (const key in apiDetail)` visits.
struct Operation {
    properties: Map<String, Value>,
    entries: Vec<(String, Value)>,
}

impl Operation {
    fn read(value: &Value) -> Option<Self> {
        match value {
            Value::Null => None,
            Value::Object(object) => {
                let properties = camelcase_keys(object);
                let entries = properties
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(Self {
                    properties,
                    entries,
                })
            }
            // An array is mapped item by item; its only enumerable keys are its indices.
            Value::Array(items) => Some(Self {
                properties: Map::new(),
                entries: items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let item = match item {
                            Value::Object(object) => Value::Object(camelcase_keys(object)),
                            other => other.clone(),
                        };
                        (i.to_string(), item)
                    })
                    .collect(),
            }),
            // A string's enumerable keys are the indices of its UTF-16 code units.
            Value::String(s) => Some(Self {
                properties: Map::new(),
                entries: s
                    .encode_utf16()
                    .enumerate()
                    .map(|(i, unit)| {
                        (
                            i.to_string(),
                            Value::String(String::from_utf16_lossy(&[unit])),
                        )
                    })
                    .collect(),
            }),
            Value::Bool(_) | Value::Number(_) => Some(Self {
                properties: Map::new(),
                entries: Vec::new(),
            }),
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
}

/// The text a JavaScript value converts to with `String(value)`, for strings and nested
/// lists of strings.
fn js_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(js_to_string).collect::<Vec<_>>().join(","),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn collect_patterns(value: &Value, out: &mut Vec<String>) -> bool {
    match value {
        Value::String(s) => {
            out.push(s.clone());
            true
        }
        Value::Array(items) => items.iter().all(|item| collect_patterns(item, out)),
        _ => false,
    }
}

fn text_or_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn build_route(
    method: RouteMethod,
    path: &str,
    operation: &Value,
    security: &SecurityKeys,
) -> Result<Route, BuildError> {
    let Some(operation) = Operation::read(operation) else {
        return Err(BuildError::NullOperation {
            path: path.to_string(),
            method,
        });
    };

    // `apiDetail.xRouterPath || convertToExpressPath(router)`.
    let (router_path, pattern) = match operation.get("xRouterPath").filter(|v| js_truthy(v)) {
        Some(value) => {
            let mut patterns = Vec::new();
            if !collect_patterns(value, &mut patterns) {
                return Err(BuildError::RouterPathType {
                    path: path.to_string(),
                    method,
                });
            }
            let compiled = PathPattern::compile_all(&patterns);
            (js_to_string(value), compiled)
        }
        None => {
            let express = convert_to_express_path(path);
            let compiled = PathPattern::compile(&express);
            (express, compiled)
        }
    };
    let pattern = pattern.map_err(|source| BuildError::Pattern {
        path: path.to_string(),
        method,
        source,
    })?;

    let security_value = operation.get("security");
    let security_error = || BuildError::Security {
        path: path.to_string(),
        method,
    };
    let mut bearer_names = vec!["bearer"];
    bearer_names.extend(security.bearer.iter().map(String::as_str));
    let is_bearer_auth =
        security_flag(security_value, &bearer_names, false).ok_or_else(security_error)?;
    let api_key_names: Vec<&str> = security.api_key.iter().map(String::as_str).collect();
    let is_api_key_auth =
        security_flag(security_value, &api_key_names, true).ok_or_else(security_error)?;

    let rate_limits = match operation.get("xRateLimits") {
        Some(Value::Array(items)) => items.iter().filter_map(RouteLimit::from_json).collect(),
        _ => Vec::new(),
    };

    let extra = operation
        .entries
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "parameters" | "responses" | "xRateLimits" | "xRouterPath"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    Ok(Route {
        method,
        operation_id: operation
            .get("operationId")
            .filter(|v| !v.is_null())
            .map(text_or_json),
        description: operation
            .get("description")
            .filter(|v| !v.is_null())
            .map(text_or_json)
            .unwrap_or_default(),
        path: path.to_string(),
        router_path,
        pattern,
        rate_limits,
        is_bearer_auth,
        is_api_key_auth,
        extra,
    })
}

/// `checkRouterNeedBearerToken` / `checkRouterNeedApiKey`: whether any requirement object
/// in `security` has one of `names` with a truthy value. `None` where the reference
/// throws: `security` is truthy but not iterable, or a `null` requirement is reached.
///
/// With `stop_without_names` (the API-key check), an empty `names` returns false at the
/// first requirement, before it is read.
fn security_flag(
    security: Option<&Value>,
    names: &[&str],
    stop_without_names: bool,
) -> Option<bool> {
    let Some(security) = security.filter(|v| js_truthy(v)) else {
        return Some(false);
    };
    match security {
        Value::Array(requirements) => {
            for requirement in requirements {
                if stop_without_names && names.is_empty() {
                    return Some(false);
                }
                match requirement {
                    Value::Null => return None,
                    Value::Object(schemes)
                        if names
                            .iter()
                            .any(|name| schemes.get(*name).is_some_and(js_truthy)) =>
                    {
                        return Some(true);
                    }
                    _ => {}
                }
            }
            Some(false)
        }
        // A string iterates its characters, which have no scheme properties.
        Value::String(_) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
