//! The outgoing header set middleware fill in (REQ-21, REQ-22).

use super::js_object::JsObject;
use super::kebab::kebab_case;

/// The headers middleware add to a Routed Request on its way upstream: the Reference
/// Gateway's `ProxyRequest`.
///
/// Names are kept as given (for example `authUserId`) and converted with [`kebab_case`] when
/// the request is forwarded. A later value for the same name replaces the earlier one in
/// place (REQ-22). Iteration follows JavaScript object order: names that are array indices
/// (`0`, `42`) first, then insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyRequest {
    headers: JsObject<String>,
}

impl ProxyRequest {
    /// An empty header set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets one header.
    pub fn add_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.insert(name.into(), value.into());
    }

    /// Merges several headers; later values win.
    pub fn add_headers<N, V>(&mut self, headers: impl IntoIterator<Item = (N, V)>)
    where
        N: Into<String>,
        V: Into<String>,
    {
        for (name, value) in headers {
            self.add_header(name, value);
        }
    }

    /// Removes a header by its exact name. The pipeline removes every `excludeHeaders` name
    /// this way before the middleware chain runs (REQ-17).
    pub fn remove_header(&mut self, name: &str) -> Option<String> {
        self.headers.remove(name)
    }

    /// The value of a header, by its exact name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The headers as set, in order.
    pub fn headers(&self) -> Vec<(String, String)> {
        self.headers
            .entries()
            .into_iter()
            .map(|(k, v)| (k, v.clone()))
            .collect()
    }

    /// Whether no header is set.
    pub fn is_empty(&self) -> bool {
        self.headers.len() == 0
    }

    /// The headers with kebab-cased names (REQ-21), as forwarded upstream. When two names
    /// convert to the same kebab name, the later value wins at the earlier position.
    pub fn kebab_headers(&self) -> Vec<(String, String)> {
        let mut converted: JsObject<String> = JsObject::new();
        for (name, value) in self.headers.entries() {
            converted.insert(kebab_case(&name), value.clone());
        }
        converted.into_entries()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    // proxy-request.model.spec.ts
    #[test]
    fn starts_empty_and_adds_one() {
        let mut request = ProxyRequest::new();
        assert!(request.is_empty());
        request.add_header("authUserId", "123");
        assert_eq!(request.headers(), owned(&[("authUserId", "123")]));
    }

    #[test]
    fn merges_with_later_values_winning() {
        let mut request = ProxyRequest::new();
        request.add_header("a", "1");
        request.add_headers([("a", "2"), ("b", "3")]);
        assert_eq!(request.headers(), owned(&[("a", "2"), ("b", "3")]));
    }

    #[test]
    fn kebab_headers_convert_names() {
        let mut request = ProxyRequest::new();
        request.add_headers([("authUserId", "123"), ("xTraceId", "abc")]);
        assert_eq!(
            request.kebab_headers(),
            owned(&[("auth-user-id", "123"), ("x-trace-id", "abc")])
        );
    }

    // object.helper.spec.ts
    #[test]
    fn kebab_conversion_keeps_values_and_handles_collisions() {
        let mut request = ProxyRequest::new();
        request.add_headers([("auth_user_id", "1"), ("authUserId", "2"), ("7", "n")]);
        assert_eq!(
            request.kebab_headers(),
            owned(&[("7", "n"), ("auth-user-id", "2")])
        );
        assert!(ProxyRequest::new().kebab_headers().is_empty());
    }

    #[test]
    fn remove_header_uses_the_exact_name() {
        let mut request = ProxyRequest::new();
        request.add_header("auth-user-id", "x");
        assert_eq!(request.remove_header("Auth-User-Id"), None);
        assert_eq!(request.remove_header("auth-user-id"), Some("x".to_string()));
        assert_eq!(request.header("auth-user-id"), None);
    }
}
