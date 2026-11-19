//! The `/document-json` endpoint's JavaScript-semantics JSON (`JSON.parse` /
//! `JSON.stringify`) and document transform (DOC-05 to DOC-12) on an untrusted upstream body.
//! Properties: the round trip is a fixed point, and a transformed document is again a
//! document.
#![no_main]

use api_gateway::documents::{DocumentOptions, transform_document};
use api_gateway::fuzzing::js_json_round_trip;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(once) = js_json_round_trip(&text) {
        let twice = js_json_round_trip(&once)
            .unwrap_or_else(|| panic!("stringify output does not parse: {once:?}"));
        assert_eq!(once, twice, "JSON round trip is not a fixed point");
    }

    let options = DocumentOptions {
        exclude_headers: vec!["authorization".into(), "x-internal".into()],
        open_api_security_keys: vec!["jwt".into()],
        hide_document_ids: vec!["hidden".into()],
    };
    if let Ok(document) = transform_document("svc", data, &options) {
        assert!(
            transform_document("svc", document.as_bytes(), &options).is_ok(),
            "a transformed document is not a document: {document:?}"
        );
    }
});
