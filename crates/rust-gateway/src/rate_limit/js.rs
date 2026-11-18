//! Small pieces of JavaScript semantics the Reference Gateway relies on implicitly.

/// Whether `c` is removed by `String.prototype.trim` (ECMAScript WhiteSpace and
/// LineTerminator). Differs from [`char::is_whitespace`]: includes U+FEFF, excludes U+0085.
fn is_js_trim_char(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(is_js_trim_char)
}

/// `String(n)` for a number that is integral, infinite or NaN (the only kinds
/// `ttlToHumanReadable` interpolates).
pub(crate) fn integer_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_owned();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if n == 0.0 {
        // `String(-0)` is "0".
        return "0".to_owned();
    }
    if n.abs() < 1e21 {
        return format!("{n:.0}");
    }
    // JavaScript switches to exponent notation at 1e21 and always signs the exponent.
    let formatted = format!("{n:e}");
    match formatted.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => formatted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_matches_ecmascript() {
        assert_eq!(trim("\u{FEFF} a \u{00A0}\t"), "a");
        assert_eq!(trim("\u{0085}a"), "\u{0085}a");
    }

    #[test]
    fn integers_format_like_javascript() {
        assert_eq!(integer_to_string(2.0), "2");
        assert_eq!(integer_to_string(-0.0), "0");
        assert_eq!(integer_to_string(1e20), "100000000000000000000");
        assert_eq!(integer_to_string(1e21), "1e+21");
        assert_eq!(integer_to_string(1.5e300), "1.5e+300");
        assert_eq!(integer_to_string(f64::INFINITY), "Infinity");
        assert_eq!(integer_to_string(f64::NAN), "NaN");
    }
}
