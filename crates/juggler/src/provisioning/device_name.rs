//! Per-device name derivation for the portal's `dev_name` pre-fill.
//!
//! [`derive_device_name`] produces `{prefix}-{xxxx}` where `xxxx` is the
//! lowercase hex of the AP MAC's last two bytes, so several devices running
//! the same firmware pre-fill distinct names (and, via
//! [`resolve_client_id`](crate::mqtt::resolve_client_id), distinct MQTT client
//! IDs).  [`DeviceName`] selects between a fixed and a MAC-suffixed pre-fill.
//!
//! Names are distinct only for devices whose last two AP-MAC bytes differ (four
//! hex digits), and the suffix equals the suffix of the *derived* SSID; an
//! `ssid_override` removes that relation.

use core::fmt::Write;

use super::config::DEVICE_NAME_MAX_LEN;

/// The fixed suffix is `-xxxx`: one dash plus four lowercase hex characters.
const SUFFIX_LEN: usize = 5;

/// Experimental: API may change before 1.0.
///
/// Derives the device name `{prefix}-{xxxx}` from a prefix and the AP MAC.
///
/// `xxxx` is the **lowercase** hex of `mac[4]` and `mac[5]`.  The digits equal
/// the SSID suffix of
/// [`derive_softap_ssid`](crate::provisioning::derive_softap_ssid) when
/// compared case-insensitively (the SSID suffix is uppercase), so an operator
/// can match a device name to its access point.
///
/// Names are distinct only for devices whose last two AP-MAC bytes differ (four
/// hex digits).  The suffix equals the suffix of the *derived* SSID; an
/// `ssid_override` removes that relation.
///
/// The prefix is trimmed with [`str::trim`].  If nothing remains, the result
/// is just the four hex digits without a leading dash.  The result always fits
/// [`DEVICE_NAME_MAX_LEN`] bytes: an over-long prefix is truncated on a UTF-8
/// character boundary so the dash and the four hex characters always survive.
/// Infallible; never panics.
pub fn derive_device_name(prefix: &str, mac: &[u8; 6]) -> heapless::String<DEVICE_NAME_MAX_LEN> {
    let prefix = prefix.trim();
    let mut name: heapless::String<DEVICE_NAME_MAX_LEN> = heapless::String::new();
    if prefix.is_empty() {
        let _ = write!(name, "{:02x}{:02x}", mac[4], mac[5]);
        return name;
    }
    let mut end = prefix.len().min(DEVICE_NAME_MAX_LEN - SUFFIX_LEN);
    while end > 0 && !prefix.is_char_boundary(end) {
        end -= 1;
    }
    let _ = name.push_str(&prefix[..end]);
    let _ = write!(name, "-{:02x}{:02x}", mac[4], mac[5]);
    name
}

/// Experimental: API may change before 1.0.
///
/// How a provisioning portal chooses the pre-filled, still-editable
/// `dev_name` value.
///
/// In both variants the pre-fill stays editable and a stored name always wins
/// on re-provisioning.  There is no `Default`: the caller must choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceName<'a> {
    /// Used as-is, truncated on a char boundary to [`DEVICE_NAME_MAX_LEN`]
    /// bytes if longer; the form rejects longer names anyway.
    ///
    /// Every device running the firmware gets the same name and, via
    /// [`resolve_client_id`](crate::mqtt::resolve_client_id), the same MQTT
    /// client ID.  The caller owns uniqueness.
    Fixed(&'a str),
    /// The pre-fill is [`derive_device_name`]`(prefix, ap_mac)`.
    ///
    /// The portal reads the AP MAC itself; consumers never pass a MAC.
    /// Names are distinct only for devices whose last two AP-MAC bytes differ
    /// (four hex digits), and the suffix equals the suffix of the *derived*
    /// SSID; an `ssid_override` removes that relation.
    MacSuffixed(&'a str),
}

impl DeviceName<'_> {
    /// Experimental: API may change before 1.0.
    ///
    /// Resolves the pre-fill name for the given AP MAC.
    ///
    /// [`DeviceName::Fixed`] returns the string as-is (no trimming), truncated
    /// on a UTF-8 char boundary to [`DEVICE_NAME_MAX_LEN`] bytes if longer.
    /// [`DeviceName::MacSuffixed`] returns [`derive_device_name`]`(prefix, mac)`.
    /// Infallible; never panics.
    pub fn resolve(&self, mac: &[u8; 6]) -> heapless::String<DEVICE_NAME_MAX_LEN> {
        match *self {
            DeviceName::Fixed(s) => {
                let mut end = s.len().min(DEVICE_NAME_MAX_LEN);
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                let mut out = heapless::String::new();
                let _ = out.push_str(&s[..end]);
                out
            }
            DeviceName::MacSuffixed(p) => derive_device_name(p, mac),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::derive_softap_ssid;

    const TEST_MAC: [u8; 6] = [0xDE, 0xAD, 0xBE, 0xEF, 0xAB, 0x12];

    #[test]
    fn normal_prefix_appends_lowercase_hex_suffix() {
        assert_eq!(
            derive_device_name("rgb-clock", &TEST_MAC).as_str(),
            "rgb-clock-ab12"
        );
    }

    #[test]
    fn low_bytes_are_zero_padded() {
        let mac = [0, 0, 0, 0, 0x01, 0x0F];
        assert_eq!(derive_device_name("x", &mac).as_str(), "x-010f");
    }

    #[test]
    fn over_long_ascii_prefix_is_truncated_to_24_bytes() {
        let name = derive_device_name(&"a".repeat(40), &TEST_MAC);
        assert_eq!(name.len(), 24);
        assert!(name.ends_with("-ab12"));
        assert_eq!(&name[..19], "a".repeat(19));
    }

    #[test]
    fn multibyte_prefix_truncation_stays_on_char_boundary() {
        let name = derive_device_name(&"é".repeat(20), &TEST_MAC);
        assert!(name.len() <= 24);
        let body = name.as_str().strip_suffix("-ab12").unwrap();
        assert!(!body.is_empty());
        assert!(body.chars().all(|c| c == 'é'));
    }

    #[test]
    fn prefix_exactly_filling_budget_is_untouched() {
        let name = derive_device_name(&"p".repeat(19), &TEST_MAC);
        assert_eq!(name.len(), 24);
        assert!(name.starts_with(&"p".repeat(19)));
        assert!(name.ends_with("-ab12"));
    }

    #[test]
    fn empty_and_whitespace_prefix_yield_bare_digits() {
        assert_eq!(derive_device_name("", &TEST_MAC).as_str(), "ab12");
        assert_eq!(derive_device_name("  \t ", &TEST_MAC).as_str(), "ab12");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            derive_device_name("  clock ", &TEST_MAC).as_str(),
            "clock-ab12"
        );
    }

    #[test]
    fn suffix_matches_ssid_suffix_case_insensitively() {
        let macs = [TEST_MAC, [1, 2, 3, 4, 0xC4, 0xF5], [0; 6], [0xFF; 6]];
        for prefix in ["rgb-clock", "a", "é", "short", &"z".repeat(40)] {
            for mac in &macs {
                let name = derive_device_name(prefix, mac);
                let ssid = derive_softap_ssid(prefix, mac);
                assert!(name[name.len() - 4..].eq_ignore_ascii_case(&ssid[ssid.len() - 4..]));
                if prefix.len() + SUFFIX_LEN <= DEVICE_NAME_MAX_LEN {
                    assert_eq!(name[..name.len() - 5], ssid[..ssid.len() - 5]);
                }
            }
        }
    }

    #[test]
    fn resolve_fixed_is_verbatim() {
        let n = DeviceName::Fixed(" hive-01 ").resolve(&TEST_MAC);
        assert_eq!(n.as_str(), " hive-01 ");
    }

    #[test]
    fn resolve_fixed_truncates_on_char_boundary() {
        let long = "é".repeat(15);
        let n = DeviceName::Fixed(&long).resolve(&TEST_MAC);
        assert_eq!(n.len(), 24);
        assert!(long.starts_with(n.as_str()));
        let odd = alloc::format!("a{}", "é".repeat(14));
        let n = DeviceName::Fixed(&odd).resolve(&TEST_MAC);
        assert_eq!(n.len(), 23);
        assert!(odd.starts_with(n.as_str()));
    }

    #[test]
    fn resolve_mac_suffixed_equals_derive() {
        let n = DeviceName::MacSuffixed("hive").resolve(&TEST_MAC);
        assert_eq!(n, derive_device_name("hive", &TEST_MAC));
        assert_eq!(n.as_str(), "hive-ab12");
    }

    /// Builds a `WifiMqttDevice` submission body for the given device name and
    /// MQTT client ID.
    fn submission(dev_name: &str, mqtt_client: &str) -> alloc::string::String {
        alloc::format!(
            "wifi_ssid=home-net&wifi_pass={}&mqtt_uri=mqtt://broker.local:1883\
             &mqtt_user=&mqtt_pass=&mqtt_client={mqtt_client}\
             &ota_url=http://example.com/firmware.bin&dev_name={dev_name}",
            crate::test_support::test_psk(),
        )
    }

    fn resolved_client_id(body: &str) -> (alloc::string::String, alloc::string::String) {
        use crate::provisioning::{parse_form, SchemaProfile};
        let cfg = parse_form(body, SchemaProfile::WifiMqttDevice).expect("valid submission");
        let id = crate::mqtt::resolve_client_id(
            cfg.mqtt().and_then(|m| m.client_id()),
            cfg.device_name(),
            "rustyfarian",
        )
        .expect("valid client id");
        (cfg.device_name().into(), id.into())
    }

    #[test]
    fn mac_suffixed_names_give_distinct_client_ids_end_to_end() {
        let mac_a = [1, 2, 3, 4, 0xAB, 0x12];
        let mac_b = [1, 2, 3, 4, 0xAB, 0x13];
        let name_a = DeviceName::MacSuffixed("rgb-clock").resolve(&mac_a);
        let name_b = DeviceName::MacSuffixed("rgb-clock").resolve(&mac_b);
        let (dev_a, id_a) = resolved_client_id(&submission(&name_a, ""));
        let (dev_b, id_b) = resolved_client_id(&submission(&name_b, ""));
        assert_eq!(dev_a, name_a.as_str());
        assert_eq!(dev_b, name_b.as_str());
        assert_ne!(id_a, id_b);
        assert_eq!(id_a, name_a.as_str());
        assert_eq!(id_b, name_b.as_str());
    }

    #[test]
    fn explicit_client_id_default_collides_across_devices() {
        let mac_a = [1, 2, 3, 4, 0xAB, 0x12];
        let mac_b = [1, 2, 3, 4, 0xAB, 0x13];
        let name_a = DeviceName::MacSuffixed("rgb-clock").resolve(&mac_a);
        let name_b = DeviceName::MacSuffixed("rgb-clock").resolve(&mac_b);
        let (_, id_a) = resolved_client_id(&submission(&name_a, "shared-id"));
        let (_, id_b) = resolved_client_id(&submission(&name_b, "shared-id"));
        assert_eq!(id_a, "shared-id");
        assert_eq!(id_b, "shared-id");
    }
}
