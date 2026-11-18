//! An upstream OpenAPI document (untrusted JSON) loaded into the document store and its
//! Route table queried with arbitrary requests, as the gateway does on every refresh and
//! request (OAS-06 to OAS-15). Also the key conversions applied to each operation.
#![no_main]

use api_gateway::openapi::camelcase::camelcase_keys;
use api_gateway::openapi::{DocumentStore, SecurityKeys, convert_to_express_path};
use api_gateway::request::SUPPORTED_METHODS;
use api_gateway::upstream::UpstreamService;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    // The request URLs to look up follow the document, after a NUL byte.
    let (document, urls) = match data.iter().position(|&b| b == 0) {
        Some(at) => (&data[..at], &data[at + 1..]),
        None => (data, &[][..]),
    };
    // The store parses the body as axios does: JSON, or else the text as a string.
    let document: Value = serde_json::from_slice(document)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(document).into_owned()));

    if let Value::Object(root) = &document {
        if let Some(Value::Object(paths)) = root.get("paths") {
            for (path, item) in paths {
                let _ = convert_to_express_path(path);
                if let Value::Object(item) = item {
                    for operation in item.values() {
                        if let Value::Object(operation) = operation {
                            let _ = camelcase_keys(operation);
                        }
                    }
                }
            }
        }
    }

    let security = SecurityKeys {
        bearer: vec!["jwt".into()],
        api_key: vec!["x-api-key".into()],
    };
    let store = DocumentStore::new(
        vec![UpstreamService::new(
            "svc",
            "http://127.0.0.1:1/doc",
            "http://127.0.0.1:1",
        )],
        security,
    );
    if store
        .apply("svc", "http://127.0.0.1:1/doc", document)
        .is_err()
    {
        return;
    }
    let urls = String::from_utf8_lossy(urls);
    for url in urls.split('\n').take(8) {
        for method in SUPPORTED_METHODS {
            let _ = store.route("svc", method, url);
        }
    }
});
