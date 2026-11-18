//! Selecting the Upstream Service (REQ-04 to REQ-08, REQ-11).

use crate::error::GatewayError;

use super::prefix::matches_prefix_segment;

/// The Service Prefix that makes an Upstream Service the Default Service.
pub const DEFAULT_SERVICE_PREFIX: &str = "default";

/// How an Upstream Service was selected for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedVia {
    /// The URL starts with one of the service's Direct Prefixes (REQ-04). The service's
    /// document need not be loaded (REQ-08).
    DirectPrefix,
    /// The URL starts with the service's Service Prefix and its document is loaded (REQ-05).
    ServicePrefix,
    /// No prefix matched, or the matched service has no loaded document (REQ-07).
    DefaultService,
}

/// The Upstream Service a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedService<'a> {
    /// The selected service's Service Prefix, which identifies it.
    pub prefix: &'a str,
    /// How it was selected.
    pub via: SelectedVia,
}

impl SelectedService<'_> {
    /// Deviation D4 for REQ-08: a Routed Request needs the selected service's document to
    /// look up its Route. A Direct Prefix owner or Default Service without a loaded document
    /// answers 503 ([`GatewayError::DocumentNotLoaded`]); the Reference Gateway answers 500.
    /// Static and Bypass Requests do not call this.
    pub fn require_document(
        &self,
        has_document: impl Fn(&str) -> bool,
    ) -> Result<(), GatewayError> {
        if has_document(self.prefix) {
            Ok(())
        } else {
            Err(GatewayError::DocumentNotLoaded {
                service: self.prefix.to_string(),
            })
        }
    }
}

/// The outcome of service selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    /// A service takes the request.
    Service(SelectedService<'a>),
    /// No service takes it and there is no Default Service: 503 for every request class
    /// (REQ-07, REQ-11).
    Unavailable,
}

impl<'a> Selection<'a> {
    /// The selected service, or [`GatewayError::NoUpstreamService`].
    pub fn into_result(self) -> Result<SelectedService<'a>, GatewayError> {
        match self {
            Selection::Service(service) => Ok(service),
            Selection::Unavailable => Err(GatewayError::NoUpstreamService),
        }
    }
}

/// The configured Upstream Services' prefixes, in configuration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceSelector {
    service_prefixes: Vec<String>,
    /// Direct Prefix and the Service Prefix of its owner; the first configured owner wins.
    direct_prefixes: Vec<(String, String)>,
    has_default: bool,
}

impl ServiceSelector {
    /// Builds the selector from `(service prefix, direct prefixes)` pairs in configuration
    /// order.
    pub fn new<P, D, I>(services: I) -> Self
    where
        P: Into<String>,
        D: IntoIterator,
        D::Item: Into<String>,
        I: IntoIterator<Item = (P, D)>,
    {
        let mut selector = ServiceSelector::default();
        for (prefix, directs) in services {
            let prefix = prefix.into();
            if prefix == DEFAULT_SERVICE_PREFIX {
                selector.has_default = true;
            }
            for direct in directs {
                let direct = direct.into();
                if !selector.direct_prefixes.iter().any(|(d, _)| *d == direct) {
                    selector.direct_prefixes.push((direct, prefix.clone()));
                }
            }
            selector.service_prefixes.push(prefix);
        }
        selector
    }

    /// Selects the Upstream Service for a normalised URL. `has_document` says whether a
    /// service (by Service Prefix) has a loaded document.
    ///
    /// Direct Prefixes are tried first, in configuration order, without looking at the
    /// document (REQ-04, REQ-08). Then Service Prefixes in configuration order; the first
    /// match wins (REQ-05). When none matches, or the match has no loaded document, the
    /// Default Service takes the request if one is configured (REQ-07).
    pub fn select(&self, url: &str, has_document: impl Fn(&str) -> bool) -> Selection<'_> {
        for (direct, owner) in &self.direct_prefixes {
            if matches_prefix_segment(url, direct) {
                return Selection::Service(SelectedService {
                    prefix: owner,
                    via: SelectedVia::DirectPrefix,
                });
            }
        }
        let matched = self
            .service_prefixes
            .iter()
            .find(|prefix| matches_prefix_segment(url, prefix));
        match matched {
            Some(prefix) if has_document(prefix) => Selection::Service(SelectedService {
                prefix,
                via: SelectedVia::ServicePrefix,
            }),
            _ if self.has_default => Selection::Service(SelectedService {
                prefix: DEFAULT_SERVICE_PREFIX,
                via: SelectedVia::DefaultService,
            }),
            _ => Selection::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(prefixes: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p| prefixes.contains(&p)
    }

    fn service(prefix: &str, via: SelectedVia) -> Selection<'_> {
        Selection::Service(SelectedService { prefix, via })
    }

    // proxy.service.spec.ts, getServerName and direct-prefix registration
    #[test]
    fn direct_prefixes_route_to_their_owner() {
        let selector = ServiceSelector::new([("user-services", vec!["oauth", "oidc"])]);
        let docs = loaded(&["user-services"]);
        let direct = service("user-services", SelectedVia::DirectPrefix);
        assert_eq!(selector.select("/oauth/authorize?x=1", &docs), direct);
        assert_eq!(
            selector.select("/oidc/.well-known/openid-configuration", &docs),
            direct
        );
        assert_eq!(selector.select("/oauth", &docs), direct);
        assert_eq!(
            selector.select("/user-services/login", &docs),
            service("user-services", SelectedVia::ServicePrefix)
        );
        assert_eq!(
            selector.select("/oauthtoken/x", &docs),
            Selection::Unavailable
        );
    }

    #[test]
    fn three_direct_prefixes_and_first_owner_wins() {
        let selector = ServiceSelector::new([
            ("service-a", vec!["oauth", "oidc", "saml"]),
            ("service-b", vec!["oauth"]),
        ]);
        let docs = loaded(&["service-a", "service-b"]);
        for path in ["/oauth/x", "/oidc/x", "/saml/x"] {
            assert_eq!(
                selector.select(path, &docs),
                service("service-a", SelectedVia::DirectPrefix)
            );
        }
    }

    #[test]
    fn direct_prefixes_come_before_service_prefixes_and_need_no_document() {
        let selector = ServiceSelector::new([("oauth", vec![]), ("a", vec!["oauth"])]);
        assert_eq!(
            selector.select("/oauth/x", loaded(&[])),
            service("a", SelectedVia::DirectPrefix)
        );
    }

    #[test]
    fn service_prefixes_first_match_in_configuration_order() {
        let short_first = ServiceSelector::new([("api", Vec::<String>::new()), ("api/v2", vec![])]);
        let long_first = ServiceSelector::new([("api/v2", Vec::<String>::new()), ("api", vec![])]);
        let docs = loaded(&["api", "api/v2"]);
        assert_eq!(
            short_first.select("/api/v2/x", &docs),
            service("api", SelectedVia::ServicePrefix)
        );
        assert_eq!(
            long_first.select("/api/v2/x", &docs),
            service("api/v2", SelectedVia::ServicePrefix)
        );
    }

    #[test]
    fn default_service_takes_unmatched_and_unloaded() {
        let selector = ServiceSelector::new([
            ("svc", Vec::<String>::new()),
            ("down", vec![]),
            ("default", vec![]),
        ]);
        let docs = loaded(&["svc", "default"]);
        let fallback = service("default", SelectedVia::DefaultService);
        assert_eq!(selector.select("/health", &docs), fallback);
        assert_eq!(selector.select("/down/health", &docs), fallback);
        assert_eq!(selector.select("/svcx", &docs), fallback);
        assert_eq!(
            selector.select("/default/x", &docs),
            service("default", SelectedVia::ServicePrefix)
        );
        // Without its document, the Default Service is still the fallback.
        assert_eq!(selector.select("/default/x", loaded(&[])), fallback);
    }

    #[test]
    fn without_a_default_service_the_answer_is_503() {
        let selector = ServiceSelector::new([("svc", Vec::<String>::new()), ("down", vec![])]);
        let docs = loaded(&["svc"]);
        assert_eq!(selector.select("/down/x", &docs), Selection::Unavailable);
        assert!(matches!(
            selector.select("/nowhere", &docs).into_result(),
            Err(GatewayError::NoUpstreamService)
        ));
    }

    #[test]
    fn routed_requests_need_a_document_deviation_d4() {
        let selector = ServiceSelector::new([("own", vec!["oauth"])]);
        let selected = selector
            .select("/oauth/x", loaded(&[]))
            .into_result()
            .unwrap();
        assert!(matches!(
            selected.require_document(loaded(&[])),
            Err(GatewayError::DocumentNotLoaded { ref service }) if service == "own"
        ));
        assert!(selected.require_document(loaded(&["own"])).is_ok());
    }
}
