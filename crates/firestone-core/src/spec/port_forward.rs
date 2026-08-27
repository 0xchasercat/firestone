use std::{borrow::Cow, fmt, net::IpAddr, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Transport protocol used by a host-to-guest port forward.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl Protocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = ParsePortForwardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            _ => Err(ParsePortForwardError::InvalidProtocol(value.to_owned())),
        }
    }
}

/// One port or an inclusive, ascending range of ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    /// Creates an inclusive port range. Both endpoints must be nonzero and ordered.
    pub fn new(start: u16, end: u16) -> Result<Self, ParsePortForwardError> {
        if start == 0 || end == 0 {
            return Err(ParsePortForwardError::InvalidPort {
                component: "port range",
                value: format_range(start, end),
            });
        }
        if start > end {
            return Err(ParsePortForwardError::InvertedRange {
                component: "port range",
                start,
                end,
            });
        }
        Ok(Self { start, end })
    }

    #[must_use]
    pub const fn start(self) -> u16 {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> u16 {
        self.end
    }

    #[must_use]
    const fn len(self) -> u32 {
        self.end as u32 - self.start as u32 + 1
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(formatter, "{}", self.start)
        } else {
            write!(formatter, "{}-{}", self.start, self.end)
        }
    }
}

impl FromStr for PortRange {
    type Err = ParsePortForwardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_port_range(value, "port range")
    }
}

impl Serialize for PortRange {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PortRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for PortRange {
    fn schema_name() -> Cow<'static, str> {
        "PortRange".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "description": "port or inclusive port range from 1 through 65535"
        })
    }
}

/// A host-to-guest port forward in Firestone's canonical configuration syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortForward {
    protocol: Protocol,
    bind: Option<IpAddr>,
    host: PortRange,
    guest: PortRange,
}

impl PortForward {
    pub fn new(
        protocol: Protocol,
        bind: Option<IpAddr>,
        host: PortRange,
        guest: PortRange,
    ) -> Result<Self, ParsePortForwardError> {
        if host.len() != guest.len() {
            return Err(ParsePortForwardError::UnequalRangeLengths {
                host: host.len(),
                guest: guest.len(),
            });
        }
        Ok(Self {
            protocol,
            bind,
            host,
            guest,
        })
    }

    #[must_use]
    pub const fn protocol(self) -> Protocol {
        self.protocol
    }

    #[must_use]
    pub const fn bind(self) -> Option<IpAddr> {
        self.bind
    }

    #[must_use]
    pub const fn host(self) -> PortRange {
        self.host
    }

    #[must_use]
    pub const fn guest(self) -> PortRange {
        self.guest
    }
}

impl fmt::Display for PortForward {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.protocol == Protocol::Udp {
            formatter.write_str("udp:")?;
        }
        if let Some(bind) = self.bind {
            match bind {
                IpAddr::V4(address) => write!(formatter, "{address}:")?,
                IpAddr::V6(address) => write!(formatter, "[{address}]:")?,
            }
        }
        write!(formatter, "{}:{}", self.host, self.guest)
    }
}

impl FromStr for PortForward {
    type Err = ParsePortForwardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (before_guest, guest_value) = value
            .rsplit_once(':')
            .ok_or(ParsePortForwardError::InvalidSyntax)?;
        let (prefix, host_value) = match before_guest.rsplit_once(':') {
            Some((prefix, host)) => (Some(prefix), host),
            None => (None, before_guest),
        };

        let host = parse_port_range(host_value, "host")?;
        let guest = parse_port_range(guest_value, "guest")?;
        let (protocol, bind) = parse_prefix(prefix)?;
        Self::new(protocol, bind, host, guest)
    }
}

impl Serialize for PortForward {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PortForward {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for PortForward {
    fn schema_name() -> Cow<'static, str> {
        "PortForward".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 3,
            "description": "[proto:][bind:]HOST:GUEST"
        })
    }
}

/// Reason a port-forward string did not match `[proto:][bind:]HOST:GUEST`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParsePortForwardError {
    #[error("expected a port forward in the form '[proto:][bind:]HOST:GUEST'")]
    InvalidSyntax,
    #[error("unsupported port-forward protocol '{0}'; expected 'tcp' or 'udp'")]
    InvalidProtocol(String),
    #[error("invalid bind address '{0}'; expected an IPv4 or IPv6 literal")]
    InvalidBind(String),
    #[error("invalid {component} port or range '{value}'; expected ports from 1 through 65535")]
    InvalidPort {
        component: &'static str,
        value: String,
    },
    #[error("invalid {component} range '{start}-{end}'; range start exceeds range end")]
    InvertedRange {
        component: &'static str,
        start: u16,
        end: u16,
    },
    #[error(
        "host and guest port ranges must have equal lengths; host has {host} ports and guest has {guest} ports"
    )]
    UnequalRangeLengths { host: u32, guest: u32 },
}

fn parse_prefix(prefix: Option<&str>) -> Result<(Protocol, Option<IpAddr>), ParsePortForwardError> {
    let Some(prefix) = prefix else {
        return Ok((Protocol::Tcp, None));
    };

    match prefix {
        "tcp" => return Ok((Protocol::Tcp, None)),
        "udp" => return Ok((Protocol::Udp, None)),
        _ => {}
    }

    let (protocol, bind_value) = if let Some(bind) = prefix.strip_prefix("tcp:") {
        (Protocol::Tcp, bind)
    } else if let Some(bind) = prefix.strip_prefix("udp:") {
        (Protocol::Udp, bind)
    } else {
        (Protocol::Tcp, prefix)
    };

    match parse_bind(bind_value) {
        Ok(bind) => Ok((protocol, Some(bind))),
        Err(error) if protocol == Protocol::Tcp && bind_value == prefix => {
            Err(classify_prefix_error(prefix, error))
        }
        Err(error) => Err(error),
    }
}

fn parse_bind(value: &str) -> Result<IpAddr, ParsePortForwardError> {
    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return match inner.parse::<IpAddr>() {
            Ok(address @ IpAddr::V6(_)) => Ok(address),
            Ok(IpAddr::V4(_)) | Err(_) => Err(ParsePortForwardError::InvalidBind(value.to_owned())),
        };
    }
    if value.contains('[') || value.contains(']') {
        return Err(ParsePortForwardError::InvalidBind(value.to_owned()));
    }
    value
        .parse()
        .map_err(|_| ParsePortForwardError::InvalidBind(value.to_owned()))
}

fn classify_prefix_error(prefix: &str, bind_error: ParsePortForwardError) -> ParsePortForwardError {
    let candidate = prefix
        .split_once(':')
        .map_or(prefix, |(candidate, _)| candidate);
    if !candidate.is_empty() && candidate.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        ParsePortForwardError::InvalidProtocol(candidate.to_owned())
    } else {
        bind_error
    }
}

fn parse_port_range(
    value: &str,
    component: &'static str,
) -> Result<PortRange, ParsePortForwardError> {
    let (start_value, end_value) = value
        .split_once('-')
        .map_or((value, value), |(start, end)| (start, end));
    let start = parse_port(start_value, component, value)?;
    let end = parse_port(end_value, component, value)?;
    if start > end {
        return Err(ParsePortForwardError::InvertedRange {
            component,
            start,
            end,
        });
    }
    Ok(PortRange { start, end })
}

fn parse_port(
    value: &str,
    component: &'static str,
    original: &str,
) -> Result<u16, ParsePortForwardError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParsePortForwardError::InvalidPort {
            component,
            value: original.to_owned(),
        });
    }
    let port = value
        .parse::<u32>()
        .map_err(|_| ParsePortForwardError::InvalidPort {
            component,
            value: original.to_owned(),
        })?;
    u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| ParsePortForwardError::InvalidPort {
            component,
            value: original.to_owned(),
        })
}

fn format_range(start: u16, end: u16) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{net::IpAddr, str::FromStr};

    use super::{ParsePortForwardError, PortForward, PortRange, Protocol};

    #[test]
    fn port_forward_minimal_form_uses_tcp_and_all_addresses() {
        let forward = PortForward::from_str("8080:80").expect("valid forward");

        assert_eq!(forward.protocol, Protocol::Tcp);
        assert_eq!(forward.bind, None);
        assert_eq!(
            forward.host,
            PortRange::new(8080, 8080).expect("valid range")
        );
        assert_eq!(forward.guest, PortRange::new(80, 80).expect("valid range"));
    }

    #[test]
    fn port_forward_udp_form_sets_udp_protocol() {
        let forward = PortForward::from_str("udp:5353:53").expect("valid forward");

        assert_eq!(forward.protocol, Protocol::Udp);
        assert_eq!(forward.bind, None);
    }

    #[test]
    fn port_forward_explicit_tcp_form_sets_tcp_protocol() {
        let forward = PortForward::from_str("tcp:8443:443").expect("valid forward");

        assert_eq!(forward.protocol, Protocol::Tcp);
        assert_eq!(forward.to_string(), "8443:443");
    }

    #[test]
    fn port_forward_ipv4_bind_parses_address() {
        let forward = PortForward::from_str("127.0.0.1:2222:22").expect("valid forward");

        assert_eq!(forward.bind, Some(IpAddr::from([127, 0, 0, 1])));
        assert_eq!(forward.to_string(), "127.0.0.1:2222:22");
    }

    #[test]
    fn port_forward_protocol_and_ipv4_bind_parse_together() {
        let forward = PortForward::from_str("udp:0.0.0.0:5353:53").expect("valid forward");

        assert_eq!(forward.protocol, Protocol::Udp);
        assert_eq!(forward.bind, Some(IpAddr::from([0, 0, 0, 0])));
    }

    #[test]
    fn port_forward_bracketed_ipv6_bind_parses_address() {
        let forward = PortForward::from_str("[::1]:2222:22").expect("valid forward");

        assert_eq!(
            forward.bind,
            Some(IpAddr::from_str("::1").expect("valid IP"))
        );
        assert_eq!(forward.to_string(), "[::1]:2222:22");
    }

    #[test]
    fn port_forward_unbracketed_ipv6_bind_uses_last_port_components() {
        let forward = PortForward::from_str("udp:2001:db8::1:5353:53").expect("valid forward");

        assert_eq!(forward.protocol, Protocol::Udp);
        assert_eq!(
            forward.bind,
            Some(IpAddr::from_str("2001:db8::1").expect("valid IP"))
        );
        assert_eq!(forward.to_string(), "udp:[2001:db8::1]:5353:53");
    }

    #[test]
    fn port_forward_equal_ranges_parse_inclusively() {
        let forward = PortForward::from_str("8000-8010:9000-9010").expect("valid forward");

        assert_eq!((forward.host.start(), forward.host.end()), (8000, 8010));
        assert_eq!((forward.guest.start(), forward.guest.end()), (9000, 9010));
    }

    #[test]
    fn port_forward_boundary_ports_are_accepted() {
        let forward = PortForward::from_str("1-65535:1-65535").expect("valid forward");

        assert_eq!((forward.host.start(), forward.host.end()), (1, 65535));
        assert_eq!((forward.guest.start(), forward.guest.end()), (1, 65535));
    }

    #[test]
    fn port_forward_leading_zero_ports_display_canonically() {
        let forward = PortForward::from_str("08080:00080").expect("valid forward");

        assert_eq!(forward.to_string(), "8080:80");
    }

    #[test]
    fn port_range_constructor_exposes_valid_endpoints() {
        let range = PortRange::new(1000, 1005).expect("valid range");

        assert_eq!(range.start(), 1000);
        assert_eq!(range.end(), 1005);
        assert_eq!(range.to_string(), "1000-1005");
    }

    #[test]
    fn port_range_string_serde_round_trips() {
        let range = PortRange::new(1000, 1005).expect("valid range");
        let json = serde_json::to_string(&range).expect("serialize range");

        assert_eq!(json, "\"1000-1005\"");
        assert_eq!(
            serde_json::from_str::<PortRange>(&json).expect("deserialize range"),
            range
        );
    }

    #[test]
    fn port_forward_string_serde_uses_canonical_form() {
        let forward: PortForward =
            serde_json::from_str("\"tcp:2001:db8::1:08080:00080\"").expect("deserialize forward");

        assert_eq!(
            serde_json::to_string(&forward).expect("serialize forward"),
            "\"[2001:db8::1]:8080:80\""
        );
    }

    #[test]
    fn port_forward_non_string_serde_returns_error() {
        assert!(serde_json::from_str::<PortForward>("{\"host\":8080}").is_err());
    }

    #[test]
    fn port_forward_schema_has_string_type() {
        let schema =
            serde_json::to_value(schemars::schema_for!(PortForward)).expect("serialize schema");

        assert_eq!(schema.get("type"), Some(&serde_json::json!("string")));
    }

    #[test]
    fn port_range_schema_has_string_type() {
        let schema =
            serde_json::to_value(schemars::schema_for!(PortRange)).expect("serialize schema");

        assert_eq!(schema.get("type"), Some(&serde_json::json!("string")));
    }

    #[test]
    fn port_forward_constructor_unequal_ranges_returns_error() {
        let host = PortRange::new(8000, 8010).expect("valid range");
        let guest = PortRange::new(80, 80).expect("valid range");
        assert!(PortForward::new(Protocol::Tcp, None, host, guest).is_err());
    }

    #[test]
    fn port_forward_missing_components_returns_error() {
        for value in ["", "8080", ":80", "8080:", "tcp::80"] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_forward_extra_components_returns_error() {
        for value in ["8080:80:8", "tcp:udp:8080:80", "127.0.0.1:1:8080:80"] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_forward_unknown_protocol_returns_protocol_error() {
        for value in ["sctp:8080:80", "SCTP:127.0.0.1:8080:80"] {
            assert!(
                matches!(
                    PortForward::from_str(value),
                    Err(ParsePortForwardError::InvalidProtocol(_))
                ),
                "unexpected result for {value}"
            );
        }
    }

    #[test]
    fn port_forward_invalid_bind_returns_bind_error() {
        for value in ["999.0.0.1:8080:80", "tcp:1.2.3:8080:80", "udp::1:8080:80"] {
            assert!(
                matches!(
                    PortForward::from_str(value),
                    Err(ParsePortForwardError::InvalidBind(_))
                ),
                "unexpected result for {value}"
            );
        }
    }

    #[test]
    fn port_forward_malformed_bracketed_bind_returns_error() {
        for value in [
            "[::1:8080:80",
            "::1]:8080:80",
            "[127.0.0.1]:8080:80",
            "[]:8080:80",
        ] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_forward_malformed_ports_returns_error() {
        for value in [
            "abc:80", "8080:def", "+8080:80", "8080:+80", "1.5:80", "1-2-3:80", "1--2:80",
        ] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_forward_zero_or_out_of_range_ports_returns_error() {
        for value in [
            "0:80",
            "8080:0",
            "65536:80",
            "8080:65536",
            "1-65536:1-65536",
        ] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_forward_inverted_ranges_returns_error() {
        for value in ["10-1:20-11", "1-10:20-11"] {
            assert!(
                matches!(
                    PortForward::from_str(value),
                    Err(ParsePortForwardError::InvertedRange { .. })
                ),
                "unexpected result for {value}"
            );
        }
    }

    #[test]
    fn port_forward_unequal_ranges_returns_error() {
        for value in ["8000-8010:80", "80:8000-8010", "100-101:200-202"] {
            assert!(
                matches!(
                    PortForward::from_str(value),
                    Err(ParsePortForwardError::UnequalRangeLengths { .. })
                ),
                "unexpected result for {value}"
            );
        }
    }

    #[test]
    fn port_forward_surrounding_or_internal_whitespace_returns_error() {
        for value in [" 8080:80", "8080:80 ", "8080 :80", "udp :8080:80"] {
            assert!(PortForward::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn port_range_zero_or_inverted_constructor_returns_error() {
        assert!(PortRange::new(0, 1).is_err());
        assert!(PortRange::new(1, 0).is_err());
        assert!(PortRange::new(2, 1).is_err());
    }

    #[test]
    fn protocol_supported_values_round_trip() {
        for protocol in [Protocol::Tcp, Protocol::Udp] {
            assert_eq!(Protocol::from_str(protocol.as_str()), Ok(protocol));
            assert_eq!(protocol.to_string(), protocol.as_str());
        }
    }

    #[test]
    fn protocol_unknown_value_returns_error() {
        assert!(matches!(
            Protocol::from_str("sctp"),
            Err(ParsePortForwardError::InvalidProtocol(value)) if value == "sctp"
        ));
    }
}
