//! Classification of the configured broker host (shared decision, ADR 023).

use core::net::Ipv4Addr;
use core::str::FromStr;

/// Longest DNS name accepted (RFC 1035, without the trailing dot).
pub const MAX_HOSTNAME_LEN: usize = 253;

/// How a tier must reach the broker named by `MqttConfig::host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerHost<'a> {
    /// An IPv4 literal: connect directly, no DNS query.
    Ipv4(Ipv4Addr),
    /// A DNS name (trimmed): resolve it on every (re)connect.
    Name(&'a str),
}

/// Classifies `host` as an IPv4 literal or a DNS name.
///
/// Surrounding whitespace is ignored. Anything that looks like an IPv4 literal
/// (digits and dots only) but does not parse is rejected rather than sent to DNS,
/// and so is an IPv6 literal (contains `:`).
pub fn classify_broker_host(host: &str) -> Result<BrokerHost<'_>, &'static str> {
    let host = host.trim();
    if host.is_empty() {
        return Err("MQTT broker host must not be empty");
    }
    if let Ok(ip) = Ipv4Addr::from_str(host) {
        return Ok(BrokerHost::Ipv4(ip));
    }
    if host.contains(':') {
        return Err("MQTT broker host must not be an IPv6 literal");
    }
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err("MQTT broker host is a malformed IPv4 literal");
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.len() > MAX_HOSTNAME_LEN {
        return Err("MQTT broker host name must be 1..=253 bytes");
    }
    for label in name.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err("MQTT broker host labels must be 1..=63 bytes");
        }
        if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
            return Err("MQTT broker host labels must not start or end with '-'");
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
        {
            return Err("MQTT broker host contains an invalid character");
        }
    }
    Ok(BrokerHost::Name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_is_classified_without_dns() {
        assert_eq!(
            classify_broker_host(" 192.168.31.5 "),
            Ok(BrokerHost::Ipv4(Ipv4Addr::new(192, 168, 31, 5)))
        );
    }

    #[test]
    fn names_are_classified_for_dns() {
        assert_eq!(
            classify_broker_host("broker.local"),
            Ok(BrokerHost::Name("broker.local"))
        );
        assert_eq!(
            classify_broker_host("mqtt-1.example.org."),
            Ok(BrokerHost::Name("mqtt-1.example.org"))
        );
        assert_eq!(
            classify_broker_host("broker"),
            Ok(BrokerHost::Name("broker"))
        );
        assert_eq!(
            classify_broker_host("1broker.lan"),
            Ok(BrokerHost::Name("1broker.lan"))
        );
    }

    #[test]
    fn malformed_hosts_are_rejected() {
        for bad in [
            "",
            "  ",
            "::1",
            "999.1.1.1",
            "1.2.3",
            "1.2.3.4.5",
            "a..b",
            ".a",
            "-a.b",
            "a-.b",
            "a b",
            "a/b",
            "a:1883",
            ".",
        ] {
            assert!(classify_broker_host(bad).is_err(), "{bad:?}");
        }
        let label = [b'a'; 64];
        assert!(classify_broker_host(core::str::from_utf8(&label).unwrap()).is_err());
        // Four 60-byte labels plus one 20-byte label and dots: 264 bytes.
        let mut name = [b'a'; 264];
        for dot in [60, 121, 182, 243] {
            name[dot] = b'.';
        }
        assert!(classify_broker_host(core::str::from_utf8(&name).unwrap()).is_err());
    }
}
