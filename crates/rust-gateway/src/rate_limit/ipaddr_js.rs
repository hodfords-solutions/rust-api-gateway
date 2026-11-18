//! The address parsing of `ipaddr.js` 1.9.1, which `proxy-addr` uses to read trusted-proxy
//! settings and to decide whether a hop is trusted.
//!
//! It is reproduced quirk for quirk, because it decides which `X-Forwarded-For` entry becomes
//! the client address (RL-06): `ipaddr.js` accepts IPv4 in octal, hexadecimal and 32-bit
//! forms (`0177.0.0.1`, `0x7f.0.0.1`, `2130706433`), IPv6 with a zone index, and some IPv6
//! texts that are not valid (`1:2:3:4:5:6:7:8::`). The regular expressions below are the
//! library's own, with its case-insensitive classes written out.

use std::sync::LazyLock;

use regex::Regex;

/// A parsed address. The IPv6 zone index is parsed and dropped: matching ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Addr {
    V4([u8; 4]),
    V6([u16; 8]),
}

const IPV4_PART: &str = "(0?[0-9]+|0[xX][0-9a-fA-F]+)";
const IPV6_PART: &str = "(?:[0-9a-fA-F]+::?)+";
const ZONE_INDEX: &str = "%[0-9a-zA-Z]{1,}";

static IPV4_FOUR_OCTET: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"^{IPV4_PART}\.{IPV4_PART}\.{IPV4_PART}\.{IPV4_PART}$"
    ))
});
static IPV4_LONG_VALUE: LazyLock<Regex> = LazyLock::new(|| compile(&format!("^{IPV4_PART}$")));
static IPV6_NATIVE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        "^(::)?({IPV6_PART})?([0-9a-fA-F]+)?(::)?({ZONE_INDEX})?$"
    ))
});
static IPV6_TRANSITIONAL: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"^((?:{IPV6_PART})|(?:::)(?:{IPV6_PART})?){IPV4_PART}\.{IPV4_PART}\.{IPV4_PART}\.{IPV4_PART}({ZONE_INDEX})?$"
    ))
});
static ZONE: LazyLock<Regex> = LazyLock::new(|| compile(ZONE_INDEX));

fn compile(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        // The patterns are constants; a failure here is a programming error caught by tests.
        Err(error) => panic!("invalid ipaddr.js pattern {pattern:?}: {error}"),
    }
}

/// JavaScript `parseInt(text, radix)` for text that starts with a digit of `radix` (or is
/// empty), returning NaN when no digit is read. Precision past 2^53 does not matter here:
/// callers only compare the result with small bounds.
fn parse_int_radix(text: &str, radix: u32) -> f64 {
    let mut value = 0.0_f64;
    let mut read = false;
    for c in text.chars() {
        match c.to_digit(radix) {
            Some(digit) => {
                value = value * f64::from(radix) + f64::from(digit);
                read = true;
            }
            None => break,
        }
    }
    if read { value } else { f64::NAN }
}

/// JavaScript `parseInt(text)` (no radix): hexadecimal after `0x`/`0X`, decimal otherwise.
fn parse_int(text: &str) -> f64 {
    match text.get(..2) {
        Some("0x" | "0X") => parse_int_radix(&text[2..], 16),
        _ => parse_int_radix(text, 10),
    }
}

/// `parseIntAuto` of `IPv4.parser`: octal after a leading `0` unless the next character is a
/// lower-case `x` (so `0X1f` is read as octal, giving 0).
fn parse_int_auto(text: &str) -> f64 {
    if text.starts_with('0') && text.as_bytes().get(1) != Some(&b'x') {
        parse_int_radix(text, 8)
    } else {
        parse_int(text)
    }
}

fn octet(value: f64) -> Option<u8> {
    if (0.0..=255.0).contains(&value) {
        // In range and integral (parseInt never yields fractions).
        Some(value as u8)
    } else {
        None
    }
}

/// `ipaddr.IPv4.isValid` and `ipaddr.IPv4.parse`.
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    if let Some(captures) = IPV4_FOUR_OCTET.captures(text) {
        let mut octets = [0_u8; 4];
        for (i, slot) in octets.iter_mut().enumerate() {
            *slot = octet(parse_int_auto(captures.get(i + 1)?.as_str()))?;
        }
        return Some(octets);
    }
    if let Some(captures) = IPV4_LONG_VALUE.captures(text) {
        let value = parse_int_auto(captures.get(1)?.as_str());
        if !(0.0..=4_294_967_295.0).contains(&value) {
            return None;
        }
        return Some((value as u32).to_be_bytes());
    }
    None
}

/// `expandIPv6(string, parts)`: `None` where it returns `null`. Parts are JavaScript numbers
/// (NaN for an empty part).
fn expand_ipv6(text: &str, parts: usize) -> Option<Vec<f64>> {
    if text.find("::") != text.rfind("::") {
        return None;
    }
    let mut text = text;
    if ZONE.is_match(text)
        && let Some(percent) = text.find('%')
    {
        text = &text[..percent];
    }
    let mut colons = text.matches(':').count() as i64;
    if text.starts_with("::") {
        colons -= 1;
    }
    if text.ends_with("::") {
        colons -= 1;
    }
    let parts = parts as i64;
    if colons > parts {
        return None;
    }
    let mut replacement = String::from(":");
    for _ in 0..(parts - colons) {
        replacement.push_str("0:");
    }
    let mut expanded = text.replacen("::", &replacement, 1);
    if expanded.starts_with(':') {
        expanded.remove(0);
    }
    if expanded.ends_with(':') {
        expanded.pop();
    }
    Some(
        expanded
            .split(':')
            .map(|part| parse_int_radix(part, 16))
            .collect(),
    )
}

/// The `new IPv6(parts)` check: eight parts, each 0 to 0xffff.
fn ipv6_parts(parts: &[f64]) -> Option<[u16; 8]> {
    if parts.len() != 8 {
        return None;
    }
    let mut groups = [0_u16; 8];
    for (slot, &part) in groups.iter_mut().zip(parts) {
        if !(0.0..=65_535.0).contains(&part) {
            return None;
        }
        *slot = part as u16;
    }
    Some(groups)
}

/// `ipaddr.IPv6.isValid` and `ipaddr.IPv6.parse`.
fn parse_ipv6(text: &str) -> Option<[u16; 8]> {
    if !text.contains(':') {
        return None;
    }
    if IPV6_NATIVE.is_match(text) {
        return ipv6_parts(&expand_ipv6(text, 8)?);
    }
    let captures = IPV6_TRANSITIONAL.captures(text)?;
    let prefix = captures.get(1)?.as_str();
    let zone = captures.get(6).map_or("", |m| m.as_str());
    let head = &prefix[..prefix.len().saturating_sub(1)];
    let mut parts = expand_ipv6(&format!("{head}{zone}"), 6)?;
    let mut octets = [0_u8; 4];
    for (i, slot) in octets.iter_mut().enumerate() {
        *slot = octet(parse_int(captures.get(i + 2)?.as_str()))?;
    }
    parts.push(f64::from(u16::from_be_bytes([octets[0], octets[1]])));
    parts.push(f64::from(u16::from_be_bytes([octets[2], octets[3]])));
    ipv6_parts(&parts)
}

/// `ipaddr.parse` guarded by `ipaddr.isValid`: `None` where `isValid` is false.
pub(crate) fn parse(text: &str) -> Option<Addr> {
    parse_ipv6(text)
        .map(Addr::V6)
        .or_else(|| parse_ipv4(text).map(Addr::V4))
}

impl Addr {
    /// `IPv6.isIPv4MappedAddress` (always false for IPv4).
    pub(crate) fn is_ipv4_mapped(self) -> bool {
        matches!(self, Addr::V6([0, 0, 0, 0, 0, 0xffff, _, _]))
    }

    /// `IPv6.toIPv4Address` for a mapped address; other addresses are returned unchanged.
    pub(crate) fn to_ipv4(self) -> Addr {
        match self {
            Addr::V6([0, 0, 0, 0, 0, 0xffff, high, low]) => {
                let [a, b] = high.to_be_bytes();
                let [c, d] = low.to_be_bytes();
                Addr::V4([a, b, c, d])
            }
            other => other,
        }
    }

    /// `IPv4.toIPv4MappedAddress`; IPv6 is returned unchanged.
    pub(crate) fn to_ipv4_mapped(self) -> Addr {
        match self {
            Addr::V4([a, b, c, d]) => Addr::V6([
                0,
                0,
                0,
                0,
                0,
                0xffff,
                u16::from_be_bytes([a, b]),
                u16::from_be_bytes([c, d]),
            ]),
            other => other,
        }
    }

    pub(crate) fn is_ipv4(self) -> bool {
        matches!(self, Addr::V4(_))
    }

    /// `match(other, bits)` for two addresses of the same kind; false across kinds.
    pub(crate) fn matches(self, subnet: Addr, bits: u32) -> bool {
        fn prefix_equal(a: u128, b: u128, width: u32, bits: u32) -> bool {
            let bits = bits.min(width);
            if bits == 0 {
                return true;
            }
            let shift = width - bits;
            (a >> shift) == (b >> shift)
        }
        match (self, subnet) {
            (Addr::V4(a), Addr::V4(b)) => prefix_equal(
                u128::from(u32::from_be_bytes(a)),
                u128::from(u32::from_be_bytes(b)),
                32,
                bits,
            ),
            (Addr::V6(a), Addr::V6(b)) => {
                prefix_equal(groups_to_u128(a), groups_to_u128(b), 128, bits)
            }
            _ => false,
        }
    }

    /// `IPv4.prefixLengthFromSubnetMask`; `None` for IPv6 or a value that is not a mask.
    pub(crate) fn prefix_length_from_subnet_mask(self) -> Option<u32> {
        let Addr::V4(octets) = self else {
            return None;
        };
        let mut cidr = 0;
        let mut stop = false;
        for &octet in octets.iter().rev() {
            let zeros = match octet {
                0 => 8,
                128 => 7,
                192 => 6,
                224 => 5,
                240 => 4,
                248 => 3,
                252 => 2,
                254 => 1,
                255 => 0,
                _ => return None,
            };
            if stop && zeros != 0 {
                return None;
            }
            if zeros != 8 {
                stop = true;
            }
            cidr += zeros;
        }
        Some(32 - cidr)
    }
}

fn groups_to_u128(groups: [u16; 8]) -> u128 {
    groups
        .iter()
        .fold(0_u128, |acc, &group| (acc << 16) | u128::from(group))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_forms_accepted_by_ipaddr_js() {
        assert_eq!(parse("127.0.0.1"), Some(Addr::V4([127, 0, 0, 1])));
        assert_eq!(parse("0177.0.0.1"), Some(Addr::V4([127, 0, 0, 1])));
        assert_eq!(parse("0x7f.0.0.1"), Some(Addr::V4([127, 0, 0, 1])));
        assert_eq!(parse("0X7f.0.0.1"), Some(Addr::V4([0, 0, 0, 1])));
        assert_eq!(parse("2130706433"), Some(Addr::V4([127, 0, 0, 1])));
        assert_eq!(parse("127.1"), None);
        assert_eq!(parse("256.0.0.0"), None);
        assert_eq!(parse("4294967296"), None);
    }

    #[test]
    fn ipv6_forms_accepted_by_ipaddr_js() {
        assert_eq!(parse("::1"), Some(Addr::V6([0, 0, 0, 0, 0, 0, 0, 1])));
        assert_eq!(
            parse("fe80::1%eth0"),
            Some(Addr::V6([0xfe80, 0, 0, 0, 0, 0, 0, 1]))
        );
        assert_eq!(
            parse("1:2:3:4:5:6:7:8::"),
            Some(Addr::V6([1, 2, 3, 4, 5, 6, 7, 8]))
        );
        assert_eq!(
            parse("::ffff:1.2.3.4"),
            Some(Addr::V6([0, 0, 0, 0, 0, 0xffff, 0x102, 0x304]))
        );
        assert_eq!(parse(":::"), None);
        assert_eq!(parse("1::2::3"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn subnet_masks() {
        assert_eq!(
            Addr::V4([255, 0, 0, 0]).prefix_length_from_subnet_mask(),
            Some(8)
        );
        assert_eq!(
            Addr::V4([255, 255, 255, 255]).prefix_length_from_subnet_mask(),
            Some(32)
        );
        assert_eq!(
            Addr::V4([255, 0, 255, 0]).prefix_length_from_subnet_mask(),
            None
        );
        assert_eq!(
            Addr::V4([0, 0, 0, 0]).prefix_length_from_subnet_mask(),
            Some(0)
        );
    }
}
