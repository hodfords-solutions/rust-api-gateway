//! Golden vectors generated from the Reference Gateway's own libraries by
//! `testdata/generate.mjs` (see that file for how to regenerate them).

use serde_json::Value;

pub(crate) const URL: &str = include_str!("testdata/url.json");
pub(crate) const PATH_PARAM: &str = include_str!("testdata/path_param.json");
pub(crate) const QUERY: &str = include_str!("testdata/query.json");
pub(crate) const KEBAB: &str = include_str!("testdata/kebab.json");
pub(crate) const HEADERS: &str = include_str!("testdata/headers.json");

/// The `cases` array of a vector file.
pub(crate) fn cases(file: &str) -> Vec<Value> {
    let parsed: Value = serde_json::from_str(file).expect("golden vectors are JSON");
    parsed["cases"]
        .as_array()
        .expect("golden vectors have a cases array")
        .clone()
}

/// Fails with the first mismatches, so a regression is readable.
pub(crate) fn assert_no_failures(file: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{file}: {} mismatches, first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
