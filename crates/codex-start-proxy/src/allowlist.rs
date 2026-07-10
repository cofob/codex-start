//! Strict host and port allow-list parsing and matching.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A normalized DNS name or IP address.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NormalizedHost {
    /// An ASCII, lower-case IDNA DNS name.
    Domain(String),
    /// An IPv4 or IPv6 literal.
    Ip(IpAddr),
}

impl NormalizedHost {
    /// Parses an IP literal or strict IDNA domain name.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, malformed, or non-IDNA host names.
    pub fn parse(input: &str) -> Result<Self, AllowlistError> {
        if input.is_empty() {
            return Err(AllowlistError::EmptyHost);
        }
        if input.starts_with('[') || input.ends_with(']') {
            return Err(AllowlistError::InvalidHost(input.to_owned()));
        }
        if let Ok(ip) = input.parse::<IpAddr>() {
            return Ok(Self::Ip(ip));
        }

        let without_root = input.strip_suffix('.').unwrap_or(input);
        if without_root.is_empty() || without_root.len() > 253 {
            return Err(AllowlistError::InvalidHost(input.to_owned()));
        }
        let ascii = idna::domain_to_ascii_strict(without_root)
            .map_err(|_| AllowlistError::InvalidHost(input.to_owned()))?
            .to_ascii_lowercase();
        validate_ascii_domain(&ascii)
            .then_some(Self::Domain(ascii))
            .ok_or_else(|| AllowlistError::InvalidHost(input.to_owned()))
    }

    /// Returns a DNS lookup representation without IPv6 brackets.
    #[must_use]
    pub fn lookup_name(&self) -> String {
        match self {
            Self::Domain(domain) => domain.clone(),
            Self::Ip(ip) => ip.to_string(),
        }
    }

    /// Returns an HTTP authority host representation.
    #[must_use]
    pub fn authority_name(&self) -> String {
        match self {
            Self::Domain(domain) => domain.clone(),
            Self::Ip(IpAddr::V4(ip)) => ip.to_string(),
            Self::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
        }
    }
}

impl fmt::Display for NormalizedHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.authority_name())
    }
}

fn validate_ascii_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// A fully resolved host and port from an HTTP target.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Authority {
    /// Normalized target host.
    pub host: NormalizedHost,
    /// Non-zero TCP target port.
    pub port: u16,
}

impl Authority {
    /// Parses an authority, using `default_port` only when the input omits a port.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed hosts, ambiguous IPv6 syntax, missing
    /// ports, or ports outside the TCP range.
    pub fn parse(input: &str, default_port: Option<u16>) -> Result<Self, AllowlistError> {
        validate_clean_input(input)?;
        let (host, port) = split_authority(input, default_port)?;
        Ok(Self {
            host: NormalizedHost::parse(host)?,
            port,
        })
    }

    /// Formats the authority with its explicit port.
    #[must_use]
    pub fn display_with_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display_with_port())
    }
}

fn validate_clean_input(input: &str) -> Result<(), AllowlistError> {
    if input.is_empty() {
        return Err(AllowlistError::EmptyRule);
    }
    if input
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(AllowlistError::InvalidAuthority(input.to_owned()));
    }
    if input.contains('@') {
        return Err(AllowlistError::CredentialsNotAllowed);
    }
    Ok(())
}

fn split_authority(input: &str, default_port: Option<u16>) -> Result<(&str, u16), AllowlistError> {
    if let Some(remainder) = input.strip_prefix('[') {
        let closing = remainder
            .find(']')
            .ok_or_else(|| AllowlistError::InvalidAuthority(input.to_owned()))?;
        let host = &remainder[..closing];
        let suffix = &remainder[closing + 1..];
        let port = if suffix.is_empty() {
            required_default_port(default_port)?
        } else {
            parse_colon_port(suffix)?
        };
        if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err(AllowlistError::InvalidAuthority(input.to_owned()));
        }
        return Ok((host, port));
    }

    if input.parse::<Ipv6Addr>().is_ok() {
        return Ok((input, required_default_port(default_port)?));
    }
    if input.contains('[') || input.contains(']') {
        return Err(AllowlistError::InvalidAuthority(input.to_owned()));
    }

    match input.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err(AllowlistError::Ipv6PortRequiresBrackets);
            }
            Ok((host, parse_port(port)?))
        }
        None => Ok((input, required_default_port(default_port)?)),
    }
}

fn parse_colon_port(input: &str) -> Result<u16, AllowlistError> {
    let port = input
        .strip_prefix(':')
        .ok_or_else(|| AllowlistError::InvalidPort(input.to_owned()))?;
    parse_port(port)
}

fn parse_port(input: &str) -> Result<u16, AllowlistError> {
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AllowlistError::InvalidPort(input.to_owned()));
    }
    input
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| AllowlistError::InvalidPort(input.to_owned()))
}

fn required_default_port(default: Option<u16>) -> Result<u16, AllowlistError> {
    default
        .filter(|port| *port != 0)
        .ok_or(AllowlistError::MissingPort)
}

/// The host portion of an allow-list rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HostMatcher {
    /// Match exactly one normalized host.
    Exact(NormalizedHost),
    /// Match a domain below this suffix, but not the suffix apex itself.
    Subdomain(String),
}

/// The port portion of an allow-list rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PortMatcher {
    /// Match one explicit port.
    Exact(u16),
    /// Match conventional HTTP and HTTPS default ports (80 and 443).
    HttpDefaults,
    /// Match any non-zero TCP port. This requires an explicit `:*` rule suffix.
    Any,
}

/// One parsed allow-list rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AllowRule {
    /// Host matching policy.
    pub host: HostMatcher,
    /// Port matching policy.
    pub port: PortMatcher,
}

impl AllowRule {
    /// Returns whether this rule permits `authority`.
    #[must_use]
    pub fn matches(&self, authority: &Authority) -> bool {
        let host_matches = match (&self.host, &authority.host) {
            (HostMatcher::Exact(expected), actual) => expected == actual,
            (HostMatcher::Subdomain(suffix), NormalizedHost::Domain(domain)) => domain
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
            (HostMatcher::Subdomain(_), NormalizedHost::Ip(_)) => false,
        };
        let port_matches = match self.port {
            PortMatcher::Exact(port) => authority.port == port,
            PortMatcher::HttpDefaults => matches!(authority.port, 80 | 443),
            PortMatcher::Any => authority.port != 0,
        };
        host_matches && port_matches
    }
}

impl FromStr for AllowRule {
    type Err = AllowlistError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        validate_clean_input(input)?;
        let (scheme, remainder) = match input.split_once("://") {
            Some(("http", remainder)) => (Some(80), remainder),
            Some(("https", remainder)) => (Some(443), remainder),
            Some((scheme, _)) => return Err(AllowlistError::UnsupportedScheme(scheme.to_owned())),
            None => (None, input),
        };
        if remainder.contains(['/', '?', '#']) {
            return Err(AllowlistError::PathNotAllowed);
        }

        let (host_input, port) = split_rule_host_port(remainder, scheme)?;
        let host = if let Some(suffix) = host_input.strip_prefix("*.") {
            if suffix.contains('*') {
                return Err(AllowlistError::InvalidWildcard(host_input.to_owned()));
            }
            match NormalizedHost::parse(suffix)? {
                NormalizedHost::Domain(domain) => HostMatcher::Subdomain(domain),
                NormalizedHost::Ip(_) => {
                    return Err(AllowlistError::InvalidWildcard(host_input.to_owned()));
                }
            }
        } else {
            if host_input.contains('*') {
                return Err(AllowlistError::InvalidWildcard(host_input.to_owned()));
            }
            HostMatcher::Exact(NormalizedHost::parse(host_input)?)
        };
        Ok(Self { host, port })
    }
}

fn split_rule_host_port(
    input: &str,
    scheme_port: Option<u16>,
) -> Result<(&str, PortMatcher), AllowlistError> {
    if let Some(host) = input.strip_suffix(":*") {
        if host.is_empty() {
            return Err(AllowlistError::EmptyHost);
        }
        return Ok((strip_ipv6_brackets(host)?, PortMatcher::Any));
    }

    let default_matcher = || scheme_port.map_or(PortMatcher::HttpDefaults, PortMatcher::Exact);
    if let Some(remainder) = input.strip_prefix('[') {
        let closing = remainder
            .find(']')
            .ok_or_else(|| AllowlistError::InvalidAuthority(input.to_owned()))?;
        let host = &remainder[..closing];
        if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err(AllowlistError::InvalidAuthority(input.to_owned()));
        }
        let suffix = &remainder[closing + 1..];
        return if suffix.is_empty() {
            Ok((host, default_matcher()))
        } else {
            Ok((host, PortMatcher::Exact(parse_colon_port(suffix)?)))
        };
    }
    if input.parse::<Ipv6Addr>().is_ok() {
        return Ok((input, default_matcher()));
    }
    if input.contains(['[', ']']) {
        return Err(AllowlistError::InvalidAuthority(input.to_owned()));
    }
    match input.rsplit_once(':') {
        Some((host, port)) => Ok((host, PortMatcher::Exact(parse_port(port)?))),
        None => Ok((input, default_matcher())),
    }
}

fn strip_ipv6_brackets(host: &str) -> Result<&str, AllowlistError> {
    if let Some(remainder) = host.strip_prefix('[') {
        let inner = remainder
            .strip_suffix(']')
            .ok_or_else(|| AllowlistError::InvalidAuthority(host.to_owned()))?;
        if !matches!(inner.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err(AllowlistError::InvalidAuthority(host.to_owned()));
        }
        Ok(inner)
    } else {
        Ok(host)
    }
}

/// A collection of independent allow-list rules.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllowList(Vec<AllowRule>);

impl AllowList {
    /// Creates an allow-list from already validated rules.
    #[must_use]
    pub fn new(rules: Vec<AllowRule>) -> Self {
        Self(rules)
    }

    /// Parses all rules, returning the first precise parse failure.
    ///
    /// # Errors
    ///
    /// Returns an error when any rule is malformed.
    pub fn parse<'a>(rules: impl IntoIterator<Item = &'a str>) -> Result<Self, AllowlistError> {
        rules
            .into_iter()
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// Returns whether any rule permits the authority.
    #[must_use]
    pub fn allows(&self, authority: &Authority) -> bool {
        self.0.iter().any(|rule| rule.matches(authority))
    }

    /// Returns the parsed rules.
    #[must_use]
    pub fn rules(&self) -> &[AllowRule] {
        &self.0
    }

    /// Returns whether this list has no rules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Policy controlling resolved private and reserved destinations.
#[derive(Clone, Debug, Default)]
pub struct AddressPolicy {
    /// Authorities allowed to resolve to private/reserved addresses.
    pub private_authorities: AllowList,
}

impl AddressPolicy {
    /// Validates every resolved address. A mixed public/private answer is denied.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty DNS answer or a private/reserved address
    /// whose authority is not explicitly permitted.
    pub fn validate(
        &self,
        authority: &Authority,
        addresses: &[SocketAddr],
    ) -> Result<(), AllowlistError> {
        if addresses.is_empty() {
            return Err(AllowlistError::NoAddresses(authority.clone()));
        }
        if self.private_authorities.allows(authority) {
            return Ok(());
        }
        if let Some(address) = addresses
            .iter()
            .find(|address| !is_publicly_routable(address.ip()))
        {
            return Err(AllowlistError::PrivateAddressDenied {
                authority: authority.clone(),
                address: *address,
            });
        }
        Ok(())
    }
}

/// Conservatively classifies addresses safe for an Internet-only egress proxy.
#[must_use]
pub fn is_publicly_routable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let value = u32::from(ip);
    ![
        (0x0000_0000, 8),  // current network
        (0x0a00_0000, 8),  // private
        (0x6440_0000, 10), // shared address space
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local
        (0xac10_0000, 12), // private
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // documentation
        (0xc0a8_0000, 16), // private
        (0xc612_0000, 15), // benchmarking
        (0xc633_6400, 24), // documentation
        (0xcb00_7100, 24), // documentation
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved/broadcast
    ]
    .iter()
    .any(|(network, prefix)| prefix_matches_u32(value, *network, *prefix))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(ipv4);
    }
    let value = u128::from(ip);
    // IPv4-compatible and well-known NAT64 forms can otherwise hide an IPv4
    // loopback/private destination inside a syntactically global IPv6 literal.
    if value >> 32 == 0 || prefix_matches_u128(value, 0x0064_ff9b_u128 << 96, 96) {
        let octets = ip.octets();
        return is_public_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    ![
        (0_u128, 128),                          // unspecified
        (1_u128, 128),                          // loopback
        (0x0100_0000_0000_0000_u128 << 64, 64), // discard-only 100::/64
        (0x2001_0000_0000_0000_u128 << 64, 32), // Teredo and special 2001::/32
        (0x2001_0002_0000_0000_u128 << 64, 48), // benchmarking
        (0x2001_0010_0000_0000_u128 << 64, 28), // deprecated ORCHID
        (0x2001_0020_0000_0000_u128 << 64, 28), // ORCHIDv2
        (0x2001_0db8_0000_0000_u128 << 64, 32), // documentation
        (0x2002_0000_0000_0000_u128 << 64, 16), // deprecated 6to4
        (0x3fff_0000_0000_0000_u128 << 64, 20), // documentation
        (0x0064_ff9b_0001_0000_u128 << 64, 48), // local-use translation prefix
        (0xfc00_0000_0000_0000_u128 << 64, 7),  // unique-local
        (0xfe80_0000_0000_0000_u128 << 64, 10), // link-local
        (0xfec0_0000_0000_0000_u128 << 64, 10), // deprecated site-local
        (0xff00_0000_0000_0000_u128 << 64, 8),  // multicast
    ]
    .iter()
    .any(|(network, prefix)| prefix_matches_u128(value, *network, *prefix))
}

fn prefix_matches_u32(value: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    value & mask == network & mask
}

fn prefix_matches_u128(value: u128, network: u128, prefix: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
    value & mask == network & mask
}

/// Strict allow-list parsing and resolution errors.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AllowlistError {
    #[error("allow-list rule is empty")]
    EmptyRule,
    #[error("host is empty")]
    EmptyHost,
    #[error("invalid host `{0}`")]
    InvalidHost(String),
    #[error("invalid authority `{0}`")]
    InvalidAuthority(String),
    #[error("authority is missing an explicit or default port")]
    MissingPort,
    #[error("invalid TCP port `{0}`")]
    InvalidPort(String),
    #[error("an IPv6 literal with an explicit port must use brackets")]
    Ipv6PortRequiresBrackets,
    #[error("URL credentials are not allowed in an allow-list rule")]
    CredentialsNotAllowed,
    #[error("URL paths, queries, and fragments are not allowed in an allow-list rule")]
    PathNotAllowed,
    #[error("unsupported allow-list URL scheme `{0}`")]
    UnsupportedScheme(String),
    #[error("invalid wildcard host `{0}`; only a leading `*.` is accepted")]
    InvalidWildcard(String),
    #[error("DNS returned no addresses for {0}")]
    NoAddresses(Authority),
    #[error("{authority} resolved to denied private/reserved address {address}")]
    PrivateAddressDenied {
        authority: Authority,
        address: SocketAddr,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(input: &str) -> Authority {
        Authority::parse(input, None).unwrap()
    }

    #[test]
    fn normalizes_idna_case_and_root_dot() {
        assert_eq!(
            NormalizedHost::parse("BÜCHER.Example.").unwrap(),
            NormalizedHost::Domain("xn--bcher-kva.example".to_owned())
        );
    }

    #[test]
    fn parses_ipv4_and_bracketed_ipv6_authorities() {
        assert_eq!(authority("192.0.2.1:443").port, 443);
        assert_eq!(authority("[2001:db8::1]:8443").port, 8443);
        assert_eq!(
            Authority::parse("2001:db8::1", Some(443)).unwrap().port,
            443
        );
        assert!(Authority::parse("2001:db8::1:443", None).is_err());
    }

    #[test]
    fn wildcard_excludes_apex_and_wrong_label_boundary() {
        let rule: AllowRule = "*.example.com:443".parse().unwrap();
        assert!(rule.matches(&authority("api.example.com:443")));
        assert!(rule.matches(&authority("a.b.example.com:443")));
        assert!(!rule.matches(&authority("example.com:443")));
        assert!(!rule.matches(&authority("badexample.com:443")));
        assert!(!rule.matches(&authority("api.example.com:80")));
    }

    #[test]
    fn omitted_ports_mean_http_defaults_and_any_is_explicit() {
        let defaults: AllowRule = "example.com".parse().unwrap();
        let any: AllowRule = "example.net:*".parse().unwrap();
        assert!(defaults.matches(&authority("example.com:80")));
        assert!(defaults.matches(&authority("example.com:443")));
        assert!(!defaults.matches(&authority("example.com:8080")));
        assert!(any.matches(&authority("example.net:65535")));
        let maximum: AllowRule = "example.org:65535".parse().unwrap();
        assert!(maximum.matches(&authority("example.org:65535")));
        assert!(!maximum.matches(&authority("example.org:443")));
    }

    #[test]
    fn scheme_selects_one_default_port() {
        let http: AllowRule = "http://example.com".parse().unwrap();
        let https: AllowRule = "https://example.com".parse().unwrap();
        assert!(http.matches(&authority("example.com:80")));
        assert!(!http.matches(&authority("example.com:443")));
        assert!(https.matches(&authority("example.com:443")));
    }

    #[test]
    fn rejects_ambiguous_or_dangerous_rules() {
        for rule in [
            "",
            "*example.com",
            "foo.*.com",
            "http://user@example.com",
            "ftp://x.test",
            "example.com/path",
            "example.com:0",
            "[127.0.0.1]:80",
        ] {
            assert!(rule.parse::<AllowRule>().is_err(), "accepted {rule}");
        }
    }

    #[test]
    fn address_policy_is_conservative_and_supports_explicit_services() {
        let public = authority("api.example.com:443");
        let private = authority("ollama.internal:11434");
        let public_result = ["93.184.216.34:443".parse().unwrap()];
        let private_result = ["127.0.0.1:11434".parse().unwrap()];
        let policy = AddressPolicy {
            private_authorities: AllowList::parse(["ollama.internal:11434"]).unwrap(),
        };
        assert!(policy.validate(&public, &public_result).is_ok());
        assert!(policy.validate(&public, &private_result).is_err());
        assert!(policy.validate(&private, &private_result).is_ok());
    }

    #[test]
    fn routability_rejects_common_ssrf_ranges() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.2",
            "172.16.0.1",
            "192.168.1.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "64:ff9b::127.0.0.1",
            "64:ff9b:1::1",
            "2002::1",
        ] {
            assert!(!is_publicly_routable(ip.parse().unwrap()), "allowed {ip}");
        }
        assert!(is_publicly_routable("93.184.216.34".parse().unwrap()));
        assert!(is_publicly_routable(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }
}
