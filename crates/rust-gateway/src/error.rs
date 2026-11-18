//! Gateway errors and their default responses (REQ-23, REQ-25, REQ-26, FWD-22).
//!
//! Each [`GatewayError`] renders, by default, exactly the status, content type and JSON body
//! the Reference Gateway sends (key order included). An application can replace the
//! rendering with an [`ErrorRenderer`] (REQ-25), the counterpart of a NestJS exception
//! filter in the reference's host application.
//!
//! The server adds what Express adds to these responses: `X-Powered-By: Express` and a weak
//! `ETag` (seen in the Conformance Tests, not asserted).

use std::error::Error as StdError;
use std::fmt::Write as _;

use http::{HeaderValue, Response, StatusCode, header};
use serde_json::Value;

/// `Content-Type` of the NestJS exception filter's responses.
pub const JSON_UTF8: &str = "application/json; charset=utf-8";

/// `Content-Type` of the proxy's own 503 when the upstream is unavailable (FWD-22).
pub const JSON: &str = "application/json";

/// The body a middleware failed with (REQ-19).
#[derive(Debug, Clone, PartialEq)]
pub enum MiddlewareBody {
    /// A JSON value. An object or array is sent as is; anything else is wrapped as
    /// `{"statusCode":…,"message":…}`.
    Json(Value),
    /// A string, wrapped as `{"statusCode":…,"message":"…"}`.
    Text(String),
}

/// An error the gateway answers itself instead of forwarding the request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// The request path has malformed percent-encoding (REQ-26): 400 with
    /// `{"message":"Failed to decode param '<segment>'","error":"Bad Request","statusCode":400}`.
    #[error("failed to decode path segment {segment:?}")]
    MalformedPath {
        /// The first path segment `decodeURIComponent` rejects, as received.
        segment: String,
    },
    /// No route of the application or the gateway claims the request: NestJS's 404
    /// `{"message":"Cannot <METHOD> <path>","error":"Not Found","statusCode":404}`. The
    /// proxy claims every path, so only an asterisk-form target (`*`) gets it (REQ-01).
    #[error("no route claims {method} {path}")]
    NotFound {
        /// The request method.
        method: String,
        /// The request target as received.
        path: String,
    },
    /// A middleware disallowed the request (REQ-18): 403.
    #[error("a middleware disallowed the request")]
    Forbidden,
    /// No Route matches the request (REQ-14): 405.
    #[error("no Route matches the request")]
    MethodNotAllowed,
    /// The method is not one a Route can have (REQ-15, Deviation D4): 405.
    #[error("the method {method} cannot have a Route")]
    UnsupportedMethod {
        /// The request method.
        method: String,
    },
    /// No Upstream Service takes the request and there is no Default Service (REQ-07,
    /// REQ-11): 503 `{"message":"Service Unavailable","statusCode":503}`.
    #[error("no Upstream Service takes the request")]
    NoUpstreamService,
    /// A Routed Request's service has no loaded document (REQ-08, Deviation D4): the same
    /// 503 as [`GatewayError::NoUpstreamService`].
    #[error("the Upstream Service {service:?} has no loaded document")]
    DocumentNotLoaded {
        /// The service's Service Prefix.
        service: String,
    },
    /// A middleware failed with its own status and body (REQ-19).
    #[error("a middleware failed with status {status}")]
    Middleware {
        /// The status the middleware chose.
        status: StatusCode,
        /// The body it chose.
        body: MiddlewareBody,
    },
    /// A rate limit rejected the request (RL-20, RL-21): 429 with a body the rate limiter
    /// built.
    #[error("rate limited")]
    RateLimited {
        /// The JSON body, sent as is.
        body: Value,
    },
    /// An unexpected failure inside the pipeline (REQ-24): 500
    /// `{"statusCode":500,"message":"Internal server error"}`.
    #[error("internal error")]
    Internal {
        /// What went wrong, for logs; never sent to the client.
        #[source]
        source: Option<Box<dyn StdError + Send + Sync>>,
    },
    /// The upstream could not be reached, failed before sending response headers, or the
    /// request could not be sent (FWD-22): 503 `application/json` `{"message":"Service unavailable."}`.
    #[error("the upstream is unavailable")]
    UpstreamUnavailable,
    /// `/document-json` names no service with a loaded document (DOC-14, Deviation D5):
    /// 404 `{"message":"Not Found","statusCode":404}`, the body of NestJS's
    /// `NotFoundException`.
    #[error("no such document")]
    DocumentNotFound,
}

impl GatewayError {
    /// An [`GatewayError::Internal`] error carrying `source`.
    pub fn internal(source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        GatewayError::Internal {
            source: Some(source.into()),
        }
    }

    /// The status of the default response.
    pub fn status(&self) -> StatusCode {
        match self {
            GatewayError::MalformedPath { .. } => StatusCode::BAD_REQUEST,
            GatewayError::NotFound { .. } => StatusCode::NOT_FOUND,
            GatewayError::Forbidden => StatusCode::FORBIDDEN,
            GatewayError::DocumentNotFound => StatusCode::NOT_FOUND,
            GatewayError::MethodNotAllowed | GatewayError::UnsupportedMethod { .. } => {
                StatusCode::METHOD_NOT_ALLOWED
            }
            GatewayError::NoUpstreamService
            | GatewayError::DocumentNotLoaded { .. }
            | GatewayError::UpstreamUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            GatewayError::Middleware { status, .. } => *status,
            GatewayError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The `Content-Type` of the default response.
    pub fn content_type(&self) -> &'static str {
        match self {
            GatewayError::UpstreamUnavailable => JSON,
            _ => JSON_UTF8,
        }
    }

    /// The body of the default response.
    pub fn body(&self) -> String {
        match self {
            GatewayError::MalformedPath { segment } => format!(
                r#"{{"message":{},"error":"Bad Request","statusCode":400}}"#,
                js_json(&Value::String(format!(
                    "Failed to decode param '{segment}'"
                )))
            ),
            GatewayError::NotFound { method, path } => format!(
                r#"{{"message":{},"error":"Not Found","statusCode":404}}"#,
                js_json(&Value::String(format!("Cannot {method} {path}")))
            ),
            GatewayError::Forbidden => r#"{"message":"Forbidden","statusCode":403}"#.to_string(),
            GatewayError::DocumentNotFound => {
                r#"{"message":"Not Found","statusCode":404}"#.to_string()
            }
            GatewayError::MethodNotAllowed | GatewayError::UnsupportedMethod { .. } => {
                r#"{"message":"Method Not Allowed","statusCode":405}"#.to_string()
            }
            GatewayError::NoUpstreamService | GatewayError::DocumentNotLoaded { .. } => {
                r#"{"message":"Service Unavailable","statusCode":503}"#.to_string()
            }
            GatewayError::Middleware { status, body } => {
                let wrap = |message: Value| {
                    format!(
                        r#"{{"statusCode":{},"message":{}}}"#,
                        status.as_u16(),
                        js_json(&message)
                    )
                };
                match body {
                    MiddlewareBody::Json(value @ (Value::Object(_) | Value::Array(_))) => {
                        js_json(value)
                    }
                    MiddlewareBody::Json(value) => wrap(value.clone()),
                    MiddlewareBody::Text(text) => wrap(Value::String(text.clone())),
                }
            }
            GatewayError::RateLimited { body } => js_json(body),
            GatewayError::Internal { .. } => {
                r#"{"statusCode":500,"message":"Internal server error"}"#.to_string()
            }
            GatewayError::UpstreamUnavailable => {
                r#"{"message":"Service unavailable."}"#.to_string()
            }
        }
    }

    /// The default response: [`status`](Self::status), [`content_type`](Self::content_type)
    /// and [`body`](Self::body).
    pub fn to_response(&self) -> Response<Vec<u8>> {
        let mut response = Response::new(self.body().into_bytes());
        *response.status_mut() = self.status();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(self.content_type()),
        );
        response
    }
}

/// Renders gateway errors as responses (REQ-25). The default is
/// [`DefaultErrorRenderer`]; any `Fn(&GatewayError) -> Response<Vec<u8>>` is a renderer.
pub trait ErrorRenderer: Send + Sync {
    /// The response for `error`.
    fn render(&self, error: &GatewayError) -> Response<Vec<u8>>;
}

/// The Reference Gateway's responses ([`GatewayError::to_response`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultErrorRenderer;

impl ErrorRenderer for DefaultErrorRenderer {
    fn render(&self, error: &GatewayError) -> Response<Vec<u8>> {
        error.to_response()
    }
}

impl<F> ErrorRenderer for F
where
    F: Fn(&GatewayError) -> Response<Vec<u8>> + Send + Sync,
{
    fn render(&self, error: &GatewayError) -> Response<Vec<u8>> {
        self(error)
    }
}

/// `JSON.stringify(value)`: like `serde_json`, except numbers are written the way JavaScript
/// writes them (`1.0` as `1`, `1e21` as `1e+21`, `1e-7` as `1e-7`).
pub fn js_json(value: &Value) -> String {
    let mut out = String::new();
    write_js_json(&mut out, value);
    out
}

fn write_js_json(out: &mut String, value: &Value) {
    match value {
        Value::Number(n) if n.is_f64() => out.push_str(&js_number(n.as_f64().unwrap_or(0.0))),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js_json(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_js_json(out, item);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// `Number.prototype.toString()` for a finite number.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let k = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    let n = exponent + 1;
    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n(
            '0',
            usize::try_from(n - k).unwrap_or(0),
        ));
    } else if 0 < n && n <= 21 {
        let point = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..point]);
        out.push('.');
        out.push_str(&digits[point..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', usize::try_from(-n).unwrap_or(0)));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(
            out,
            "e{}{}",
            if n - 1 < 0 { '-' } else { '+' },
            (n - 1).abs()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(error: &GatewayError) -> (u16, String, String) {
        let response = error.to_response();
        (
            response.status().as_u16(),
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_string(),
            String::from_utf8(response.body().clone()).unwrap(),
        )
    }

    // errors::req_23_error_bodies, forwarding::fwd_22_upstream_unavailable
    #[test]
    fn default_bodies_match_the_reference() {
        let utf8 = JSON_UTF8.to_string();
        assert_eq!(
            view(&GatewayError::Forbidden),
            (
                403,
                utf8.clone(),
                r#"{"message":"Forbidden","statusCode":403}"#.into()
            )
        );
        assert_eq!(
            view(&GatewayError::MethodNotAllowed),
            (
                405,
                utf8.clone(),
                r#"{"message":"Method Not Allowed","statusCode":405}"#.into()
            )
        );
        assert_eq!(
            view(&GatewayError::UnsupportedMethod {
                method: "TRACE".into()
            }),
            (
                405,
                utf8.clone(),
                r#"{"message":"Method Not Allowed","statusCode":405}"#.into()
            )
        );
        for error in [
            GatewayError::NoUpstreamService,
            GatewayError::DocumentNotLoaded {
                service: "x".into(),
            },
        ] {
            assert_eq!(
                view(&error),
                (
                    503,
                    utf8.clone(),
                    r#"{"message":"Service Unavailable","statusCode":503}"#.into()
                )
            );
        }
        assert_eq!(
            view(&GatewayError::internal("boom")),
            (
                500,
                utf8.clone(),
                r#"{"statusCode":500,"message":"Internal server error"}"#.into()
            )
        );
        assert_eq!(
            view(&GatewayError::UpstreamUnavailable),
            (
                503,
                "application/json".into(),
                r#"{"message":"Service unavailable."}"#.into()
            )
        );
        assert_eq!(
            view(&GatewayError::RateLimited {
                body: json!({"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"the next one minute"}})
            }),
            (
                429,
                utf8,
                r#"{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"the next one minute"}}"#.into()
            )
        );
    }

    // pipeline::req_02_url_normalisation (the asterisk-form target)
    #[test]
    fn not_found_names_the_method_and_target() {
        assert_eq!(
            view(&GatewayError::NotFound {
                method: "GET".into(),
                path: "*".into()
            }),
            (
                404,
                JSON_UTF8.to_string(),
                r#"{"message":"Cannot GET *","error":"Not Found","statusCode":404}"#.into()
            )
        );
    }

    // errors::req_26_malformed_percent_encoding
    #[test]
    fn malformed_path_quotes_the_segment() {
        let (status, _, body) = view(&GatewayError::MalformedPath {
            segment: "%zz".into(),
        });
        assert_eq!(status, 400);
        assert_eq!(
            body,
            r#"{"message":"Failed to decode param '%zz'","error":"Bad Request","statusCode":400}"#
        );
        let (_, _, body) = view(&GatewayError::MalformedPath {
            segment: "a'b\"c%zz".into(),
        });
        assert_eq!(
            body,
            r#"{"message":"Failed to decode param 'a'b\"c%zz'","error":"Bad Request","statusCode":400}"#
        );
        let (_, _, body) = view(&GatewayError::MalformedPath {
            segment: "svc\\%zz".into(),
        });
        assert_eq!(
            body,
            r#"{"message":"Failed to decode param 'svc\\%zz'","error":"Bad Request","statusCode":400}"#
        );
    }

    // errors::req_19_middleware_errors
    #[test]
    fn middleware_bodies_are_sent_or_wrapped_like_nestjs() {
        let middleware = |status: u16, body: MiddlewareBody| {
            view(&GatewayError::Middleware {
                status: StatusCode::from_u16(status).unwrap(),
                body,
            })
            .2
        };
        let parsed = |text: &str| MiddlewareBody::Json(serde_json::from_str(text).unwrap());
        assert_eq!(
            middleware(418, parsed(r#"{"b":1,"a":"x","n":1.0}"#)),
            r#"{"b":1,"a":"x","n":1}"#
        );
        assert_eq!(
            middleware(401, MiddlewareBody::Text("not json".into())),
            r#"{"statusCode":401,"message":"not json"}"#
        );
        assert_eq!(
            middleware(401, parsed("5")),
            r#"{"statusCode":401,"message":5}"#
        );
        assert_eq!(middleware(401, parsed(r#"[1,"a"]"#)), r#"[1,"a"]"#);
        assert_eq!(
            middleware(401, parsed(r#""quoted""#)),
            r#"{"statusCode":401,"message":"quoted"}"#
        );
        assert_eq!(
            middleware(401, parsed("null")),
            r#"{"statusCode":401,"message":null}"#
        );
        assert_eq!(
            middleware(401, parsed("true")),
            r#"{"statusCode":401,"message":true}"#
        );
        assert_eq!(middleware(401, parsed("{}")), "{}");
        let response = GatewayError::Middleware {
            status: StatusCode::FOUND,
            body: parsed(r#"{"message":"moved"}"#),
        }
        .to_response();
        assert_eq!(response.status(), 302);
        assert_eq!(response.headers()[header::CONTENT_TYPE], JSON_UTF8);
    }

    #[test]
    fn numbers_are_written_like_javascript() {
        for (value, expected) in [
            (1.0, "1"),
            (-0.0, "0"),
            (1.5, "1.5"),
            (-2.25, "-2.25"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1.2345e-7, "1.2345e-7"),
            (0.000001, "0.000001"),
            (123456.789, "123456.789"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(js_number(value), expected, "{value}");
        }
    }

    #[test]
    fn an_application_can_replace_the_rendering() {
        let custom = |error: &GatewayError| {
            let mut response =
                Response::new(format!("custom {}", error.status().as_u16()).into_bytes());
            *response.status_mut() = error.status();
            response
        };
        let renderers: Vec<Box<dyn ErrorRenderer>> =
            vec![Box::new(DefaultErrorRenderer), Box::new(custom)];
        let bodies: Vec<String> = renderers
            .iter()
            .map(|r| String::from_utf8(r.render(&GatewayError::Forbidden).into_body()).unwrap())
            .collect();
        assert_eq!(
            bodies,
            [r#"{"message":"Forbidden","statusCode":403}"#, "custom 403"]
        );
    }
}
