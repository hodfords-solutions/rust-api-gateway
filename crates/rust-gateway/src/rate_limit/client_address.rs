//! The client address IP-based Buckets are keyed by (RL-06, RL-07, RL-08).
//!
//! The Reference Gateway reads Express's `request.ip`, which `proxy-addr` derives from the
//! connection's peer address and `X-Forwarded-For` according to the application's
//! `trust proxy` setting, and then normalises it with `normalizeIp`. Both are reproduced here
//! and checked against golden vectors generated from the real packages
//! (`testdata/generate.mjs`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use http::HeaderMap;

use super::ipaddr_js::{self, Addr};
use super::js;

/// The default prefix length IPv6 client addresses are masked to (`DEFAULT_IPV6_SUBNET`).
pub const DEFAULT_IPV6_SUBNET: u32 = 64;

/// Which proxies in front of the gateway are trusted to report the client address: the
/// equivalent of Express's `trust proxy` setting as the Reference Gateway's application sets it
/// (`app.set('trust proxy', …)`).
///
/// The default, like Express's, trusts nothing: the client address is the connection's peer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrustProxy {
    /// `false`: trust no proxy.
    #[default]
    None,
    /// `true`: trust every hop, so the client address is the left-most `X-Forwarded-For`
    /// entry.
    All,
    /// A number: trust this many hops, counting from the peer.
    Hops(u32),
    /// An array: trust hops whose address is in one of these entries. An entry is an
    /// address, a CIDR range (`10.0.0.0/8`), an address with an IPv4 netmask
    /// (`10.0.0.0/255.0.0.0`), or one of the presets `loopback`, `linklocal` and
    /// `uniquelocal`. Entries are used exactly as given (no trimming), as Express does for
    /// arrays.
    List(Vec<String>),
}

impl TrustProxy {
    /// The string form of the setting: comma-separated entries, each trimmed (Express splits
    /// and trims a string value before compiling it).
    pub fn from_comma_separated(value: &str) -> Self {
        TrustProxy::List(value.split(',').map(|v| js::trim(v).to_owned()).collect())
    }
}

/// A [`TrustProxy`] setting that could not be compiled. The message is `proxy-addr`'s.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidTrustProxy {
    message: String,
}

/// A compiled [`TrustProxy`] setting, which derives the client address of a request.
#[derive(Debug, Clone)]
pub struct TrustedProxies {
    rule: Rule,
}

#[derive(Debug, Clone)]
enum Rule {
    Nothing,
    Everything,
    Hops(u32),
    Subnets(Vec<(Addr, u32)>),
}

impl TrustedProxies {
    /// Compiles `trust`, failing for the entries `proxy-addr` rejects: anything that is not
    /// an address (`ipaddr.js` syntax) or preset, and ranges outside 1 to the address width.
    pub fn new(trust: &TrustProxy) -> Result<Self, InvalidTrustProxy> {
        let rule = match trust {
            TrustProxy::None => Rule::Nothing,
            TrustProxy::All => Rule::Everything,
            TrustProxy::Hops(hops) => Rule::Hops(*hops),
            TrustProxy::List(entries) => {
                let mut subnets = Vec::new();
                for entry in entries {
                    match preset(entry) {
                        Some(ranges) => {
                            for range in ranges {
                                subnets.push(parse_notation(range)?);
                            }
                        }
                        None => subnets.push(parse_notation(entry)?),
                    }
                }
                if subnets.is_empty() {
                    Rule::Nothing
                } else {
                    Rule::Subnets(subnets)
                }
            }
        };
        Ok(Self { rule })
    }

    /// The client address as Express's `request.ip` gives it: walking from the peer address
    /// through `X-Forwarded-For` (right to left) while each hop is trusted, and returning the
    /// last address reached. `None` only when there is no peer address and it is not trusted.
    ///
    /// `peer` is the connection's remote address; `forwarded_for` is the request's
    /// `X-Forwarded-For` value, with repeated headers joined (see [`forwarded_for`]). The
    /// result is not normalised (see [`normalize_ip`]) and need not be an IP address: an
    /// untrusted hop can name anything.
    pub fn client_address(
        &self,
        peer: Option<IpAddr>,
        forwarded_for: Option<&str>,
    ) -> Option<String> {
        let mut addresses: Vec<Option<String>> = vec![peer.map(|ip| ip.to_string())];
        if let Some(header) = forwarded_for {
            addresses.extend(parse_forwarded_for(header).into_iter().map(Some));
        }
        let mut last = addresses.len() - 1;
        for (i, address) in addresses.iter().enumerate().take(addresses.len() - 1) {
            if !self.trusts(address.as_deref(), i) {
                last = i;
                break;
            }
        }
        addresses.swap_remove(last)
    }

    fn trusts(&self, address: Option<&str>, hop: usize) -> bool {
        match &self.rule {
            Rule::Nothing => false,
            Rule::Everything => true,
            Rule::Hops(hops) => (hop as u64) < u64::from(*hops),
            Rule::Subnets(subnets) => address.is_some_and(|a| trusted_by_subnets(a, subnets)),
        }
    }
}

/// `proxy-addr`'s `trustMulti`: IPv4-mapped candidates are compared as IPv4, and an IPv6
/// range only covers IPv4 when it is an IPv4-mapped range of at least 96 bits.
fn trusted_by_subnets(address: &str, subnets: &[(Addr, u32)]) -> bool {
    let Some(mut ip) = ipaddr_js::parse(address) else {
        return false;
    };
    if ip.is_ipv4_mapped() {
        ip = ip.to_ipv4();
    }
    for &(subnet, bits) in subnets {
        let candidate = if ip.is_ipv4() != subnet.is_ipv4() {
            if subnet.is_ipv4() && !ip.is_ipv4_mapped() {
                continue;
            }
            if !subnet.is_ipv4() && !(bits >= 96 && subnet.is_ipv4_mapped()) {
                continue;
            }
            if subnet.is_ipv4() {
                ip.to_ipv4()
            } else {
                ip.to_ipv4_mapped()
            }
        } else if !ip.is_ipv4() && subnet.is_ipv4_mapped() {
            continue;
        } else {
            ip
        };
        if candidate.matches(subnet, bits) {
            return true;
        }
    }
    false
}

fn preset(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "linklocal" => Some(&["169.254.0.0/16", "fe80::/10"]),
        "loopback" => Some(&["127.0.0.1/8", "::1/128"]),
        "uniquelocal" => Some(&["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"]),
        _ => None,
    }
}

/// `proxy-addr`'s `parseipNotation`.
fn parse_notation(note: &str) -> Result<(Addr, u32), InvalidTrustProxy> {
    let (address, range) = match note.rfind('/') {
        Some(slash) => (&note[..slash], Some(&note[slash + 1..])),
        None => (note, None),
    };
    let Some(mut ip) = ipaddr_js::parse(address) else {
        return Err(InvalidTrustProxy {
            message: format!("invalid IP address: {address}"),
        });
    };
    if range.is_none() && ip.is_ipv4_mapped() {
        ip = ip.to_ipv4();
    }
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits: Option<u64> = match range {
        None => Some(max),
        Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            // `parseInt(range, 10)`; anything too long to fit is out of range anyway.
            Some(text.parse::<u64>().unwrap_or(u64::MAX))
        }
        Some(text) if ip.is_ipv4() => ipaddr_js::parse(text)
            .and_then(Addr::prefix_length_from_subnet_mask)
            .map(u64::from),
        Some(_) => None,
    };
    match bits {
        Some(bits) if bits > 0 && bits <= max => Ok((ip, bits as u32)),
        _ => Err(InvalidTrustProxy {
            message: format!("invalid range on address: {note}"),
        }),
    }
}

/// The `forwarded` package's `X-Forwarded-For` parser: entries split on `,`, spaces (only
/// U+0020) trimmed from each, empty entries dropped, returned nearest hop first.
fn parse_forwarded_for(header: &str) -> Vec<String> {
    let bytes = header.as_bytes();
    let mut list = Vec::new();
    let mut end = bytes.len();
    let mut start = bytes.len();
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b' ' => {
                if start == end {
                    start = i;
                    end = i;
                }
            }
            b',' => {
                if start != end {
                    list.push(header[start..end].to_owned());
                }
                start = i;
                end = i;
            }
            _ => start = i,
        }
    }
    if start != end {
        list.push(header[start..end].to_owned());
    }
    list
}

/// The request's `X-Forwarded-For` value as Node presents it to Express: every
/// `X-Forwarded-For` header, decoded as Latin-1 and joined with `", "`. `None` when the
/// request has none.
pub fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all("x-forwarded-for").iter().peekable();
    values.peek()?;
    let decoded: Vec<String> = values
        .map(|value| value.as_bytes().iter().map(|&b| char::from(b)).collect())
        .collect();
    Some(decoded.join(", "))
}

/// Normalises a client address so that equivalent addresses share one Bucket (RL-07):
///
/// - IPv4 is returned unchanged;
/// - IPv4-mapped IPv6 (`::ffff:1.2.3.4`, `::ffff:102:304`) becomes the plain IPv4 address;
/// - an IPv6 zone index (`fe80::1%eth0`) is dropped;
/// - other IPv6 is masked to its first `ipv6_subnet` bits and written as eight unpadded
///   lower-case hex groups followed by `/<ipv6_subnet>`, e.g. `2001:db8:1:2:0:0:0:0/64`;
/// - anything that is not an IP address (including the empty string) is returned unchanged.
///
/// Pass [`DEFAULT_IPV6_SUBNET`] for the default.
pub fn normalize_ip(ip: &str, ipv6_subnet: u32) -> String {
    if ip.is_empty() || ip.parse::<Ipv4Addr>().is_ok() {
        return ip.to_owned();
    }
    let address = ip.split_once('%').map_or(ip, |(address, _zone)| address);
    let Ok(v6) = address.parse::<Ipv6Addr>() else {
        return ip.to_owned();
    };
    let groups = v6.segments();
    if groups[..5].iter().all(|&g| g == 0) && groups[5] == 0xffff {
        let [a, b] = groups[6].to_be_bytes();
        let [c, d] = groups[7].to_be_bytes();
        return format!("{a}.{b}.{c}.{d}");
    }
    let subnet = i64::from(ipv6_subnet);
    let masked: Vec<String> = groups
        .iter()
        .enumerate()
        .map(|(index, &group)| {
            let bits = (subnet - index as i64 * 16).clamp(0, 16) as u32;
            let mask = (0xffff_u32 << (16 - bits)) & 0xffff;
            format!("{:x}", u32::from(group) & mask)
        })
        .collect();
    format!("{}/{}", masked.join(":"), ipv6_subnet)
}

/// The key IP-based Buckets use for a client address: the normalised address, or `unknown`
/// when there is none (RL-06).
pub(crate) fn client_key(address: Option<&str>, ipv6_subnet: u32) -> String {
    match address {
        Some(address) if !address.is_empty() => normalize_ip(address, ipv6_subnet),
        _ => "unknown".to_owned(),
    }
}
