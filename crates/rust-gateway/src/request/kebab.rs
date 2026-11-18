//! es-toolkit `kebabCase` (REQ-21).

use super::unicode_tables::{
    EMOJI_PRESENTATION, EXTENDED_PICTOGRAPHIC, LETTER, LOWERCASE_LETTER, UPPERCASE_LETTER,
};

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let c = u32::from(c);
    table
        .binary_search_by(|&(start, end)| {
            if end < c {
                std::cmp::Ordering::Less
            } else if start > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_upper(c: char) -> bool {
    in_table(UPPERCASE_LETTER, c)
}

fn is_lower(c: char) -> bool {
    in_table(LOWERCASE_LETTER, c)
}

/// es-toolkit `kebabCase`: the words of `input`, lower-cased and joined with `-`.
///
/// Words are found with es-toolkit's pattern
/// `\p{Lu}?\p{Ll}+|[0-9]+|\p{Lu}+(?!\p{Ll})|\p{Emoji_Presentation}|\p{Extended_Pictographic}|\p{L}+`,
/// tried in that order at each position; anything else separates words. So `authUserId`
/// becomes `auth-user-id`, `oauth2Token` becomes `oauth-2-token` and `HTTPRequest` becomes
/// `http-request`.
pub fn kebab_case(input: &str) -> String {
    words(input)
        .iter()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join("-")
}

fn words(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match word_at(&chars, i) {
            Some(end) => {
                words.push(chars[i..end].iter().collect());
                i = end;
            }
            None => i += 1,
        }
    }
    words
}

/// The end of the word the pattern matches at `start`, if any.
fn word_at(chars: &[char], start: usize) -> Option<usize> {
    let count_while = |from: usize, pred: &dyn Fn(char) -> bool| {
        chars[from..].iter().take_while(|&&c| pred(c)).count()
    };
    let c = chars[start];
    // \p{Lu}?\p{Ll}+
    if is_upper(c) {
        let lowers = count_while(start + 1, &is_lower);
        if lowers > 0 {
            return Some(start + 1 + lowers);
        }
    } else if is_lower(c) {
        return Some(start + count_while(start, &is_lower));
    }
    // [0-9]+
    if c.is_ascii_digit() {
        return Some(start + count_while(start, &|c: char| c.is_ascii_digit()));
    }
    // \p{Lu}+(?!\p{Ll}), backtracking until the next character is not lower-case.
    if is_upper(c) {
        let mut end = start + count_while(start, &is_upper);
        while end > start {
            if chars.get(end).is_none_or(|&next| !is_lower(next)) {
                return Some(end);
            }
            end -= 1;
        }
    }
    // \p{Emoji_Presentation}|\p{Extended_Pictographic}
    if in_table(EMOJI_PRESENTATION, c) || in_table(EXTENDED_PICTOGRAPHIC, c) {
        return Some(start + 1);
    }
    // \p{L}+
    if in_table(LETTER, c) {
        return Some(start + count_while(start, &|c: char| in_table(LETTER, c)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::golden;

    // object.helper.spec.ts and the REQ-21 row.
    #[test]
    fn converts_like_es_toolkit() {
        assert_eq!(kebab_case("authUserId"), "auth-user-id");
        assert_eq!(kebab_case("xRequestId"), "x-request-id");
        assert_eq!(kebab_case("auth_user_id"), "auth-user-id");
        assert_eq!(kebab_case("auth-user-id"), "auth-user-id");
        assert_eq!(kebab_case("oauth2Token"), "oauth-2-token");
        assert_eq!(kebab_case("HTTPRequest"), "http-request");
        assert_eq!(
            kebab_case("camelCaseHTTPRequest🚀"),
            "camel-case-http-request-🚀"
        );
        assert_eq!(kebab_case(""), "");
    }

    #[test]
    fn matches_es_toolkit_golden_vectors() {
        let mut failures = Vec::new();
        for case in golden::cases(golden::KEBAB) {
            let input = case[0].as_str().unwrap();
            let expected = case[1].as_str().unwrap();
            let actual = kebab_case(input);
            if actual != expected {
                failures.push(format!(
                    "{input:?}: es-toolkit {expected:?}, rust {actual:?}"
                ));
            }
        }
        golden::assert_no_failures("kebab.json", &failures);
    }
}
