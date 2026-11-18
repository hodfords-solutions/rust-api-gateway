//! Upstream Services (CFG-01).

use serde::{Deserialize, Serialize};

/// A backend the gateway forwards requests to and reads an OpenAPI document from: the
/// reference's `ApiServiceDetail` (API-02).
///
/// It (de)serialises with the reference's field names: `prefix`, `docUrl`, `host` and the
/// optional `directPrefixes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamService {
    /// The Service Prefix, for example `content-service`. A request whose first path
    /// segment is the prefix goes to this service, with the prefix removed.
    pub prefix: String,
    /// Where the service's OpenAPI document is fetched from, for example
    /// `http://127.0.0.1:2009/documents-json`.
    pub doc_url: String,
    /// The origin requests are forwarded to, for example `http://127.0.0.1:2009`.
    pub host: String,
    /// Direct Prefixes: leading path segments owned by this service that are forwarded
    /// without being removed, for example `["oauth", "oidc"]`.
    #[serde(default)]
    pub direct_prefixes: Vec<String>,
}

impl UpstreamService {
    /// A service without Direct Prefixes.
    pub fn new(
        prefix: impl Into<String>,
        doc_url: impl Into<String>,
        host: impl Into<String>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            doc_url: doc_url.into(),
            host: host.into(),
            direct_prefixes: Vec::new(),
        }
    }

    /// Sets the Direct Prefixes.
    pub fn with_direct_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.direct_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_01_reads_the_reference_field_names() {
        let services: Vec<UpstreamService> = serde_json::from_str(
            r#"[
                {"prefix":"svc","docUrl":"http://127.0.0.1:5000/openapi.json","host":"http://127.0.0.1:5000","directPrefixes":["oauth"]},
                {"prefix":"default","docUrl":"http://d/doc","host":"http://d"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            services,
            vec![
                UpstreamService::new(
                    "svc",
                    "http://127.0.0.1:5000/openapi.json",
                    "http://127.0.0.1:5000"
                )
                .with_direct_prefixes(["oauth"]),
                UpstreamService::new("default", "http://d/doc", "http://d"),
            ]
        );
        assert_eq!(
            serde_json::to_value(&services[1]).unwrap(),
            serde_json::json!({"prefix":"default","docUrl":"http://d/doc","host":"http://d","directPrefixes":[]})
        );
    }
}
