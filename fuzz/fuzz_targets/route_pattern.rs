//! `path-to-regexp` patterns (from upstream OpenAPI documents) compiled and matched against
//! request paths: no panic, no runaway backtracking, and the two matching entry points agree.
#![no_main]

//!
//! Input: `pattern[\x1f pattern...] \0 path [\0 path...]` as (lossy) UTF-8.

use api_gateway::route::PathPattern;
use api_gateway_fuzz::skip_open_finding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut fields = text.split('\0');
    let patterns: Vec<&str> = fields.next().unwrap_or("").split('\x1f').take(4).collect();
    let paths: Vec<&str> = fields.take(4).collect();
    // FUZZ-03: matching time is exponential in the number of wildcards.
    if skip_open_finding("FUZZ-03")
        && patterns
            .iter()
            .map(|p| p.matches('*').count())
            .sum::<usize>()
            > 12
    {
        return;
    }
    let Ok(pattern) = PathPattern::compile_all(&patterns) else {
        return;
    };
    let _ = pattern.regexp_source();
    for path in paths {
        let is_match = pattern.is_match(path);
        match pattern.matches(path) {
            Ok(Some(found)) => {
                assert!(
                    is_match,
                    "matches() matched but is_match() did not: {path:?}"
                );
                assert_eq!(found.path, path);
                for (i, (name, _)) in found.params.iter().enumerate() {
                    assert!(
                        pattern.keys().iter().any(|k| &k.name == name),
                        "parameter {name:?} is not a key of the pattern"
                    );
                    assert!(
                        !found.params[..i].iter().any(|(n, _)| n == name),
                        "parameter {name:?} reported twice"
                    );
                }
            }
            Ok(None) => assert!(
                !is_match,
                "is_match() matched but matches() did not: {path:?}"
            ),
            // A matched parameter that does not decode.
            Err(_) => assert!(is_match, "matches() failed on a path is_match() rejects"),
        }
    }
});
