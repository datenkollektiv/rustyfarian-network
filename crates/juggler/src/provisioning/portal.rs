//! Flat captive-portal helpers shared by both provisioning tiers (ADR 023
//! rung "flat logic").
//!
//! - the CSRF nonce: extraction from a form body, constant-time comparison and
//!   formatting from a random word ([`body_nonce_matches`], [`format_nonce`]);
//! - the form pre-fill: which source fills which field ([`Prefill::select`],
//!   [`compose_mqtt_uri`]);
//! - the start-up plan: validation order, the SoftAP configuration and the
//!   client-ID warning ([`plan_portal`]).
//!
//! The tiers keep only the I/O: the RNG read, the store read, the HTTP
//! response plumbing and the SoftAP bring-up.

use core::fmt;
use core::fmt::Write as _;

use super::config::MQTT_HOST_MAX_LEN;
use super::{DeviceName, PortalDefaults, ProvisioningConfig, SchemaProfile, DEVICE_NAME_MAX_LEN};
use crate::wifi::{validate_ap_config, ApConfig, Country, AP_PASSWORD_MIN_LEN};

// ── Nonce ─────────────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// The form key that carries the per-session CSRF nonce.
pub const NONCE_FIELD: &str = "_nonce";

/// Experimental: API may change before 1.0.
///
/// The raw (not percent-decoded) value of `key` in an
/// `application/x-www-form-urlencoded` body, or `None` when the key is absent.
///
/// The key must start the body or follow a `&`; the value runs to the next `&`
/// or the end of the body.
pub fn form_field_raw<'a>(body: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let key = key.as_bytes();
    let mut start = 0;
    while start <= body.len() {
        let end = body[start..]
            .iter()
            .position(|&b| b == b'&')
            .map_or(body.len(), |p| start + p);
        let pair = &body[start..end];
        if pair.len() > key.len() && pair.starts_with(key) && pair[key.len()] == b'=' {
            return Some(&pair[key.len() + 1..]);
        }
        start = end + 1;
    }
    None
}

/// Experimental: API may change before 1.0.
///
/// Compares a submitted nonce field value with the session nonce in constant
/// time.
///
/// The value is compared raw: nonces are lowercase hex (see
/// [`format_nonce`]), which percent-encoding never changes, so decoding would
/// only add a path for a mismatch. The comparison ORs the byte differences
/// together, so its runtime does not depend on the position of the first
/// differing byte. A length mismatch, including an empty value, returns
/// `false`.
pub fn nonce_matches(field_value: &[u8], expected: &str) -> bool {
    let expected = expected.as_bytes();
    if field_value.len() != expected.len() || expected.is_empty() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, b) in field_value.iter().zip(expected) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Experimental: API may change before 1.0.
///
/// `true` when `body` carries a `_nonce` field equal to `expected`.
///
/// The portal checks this before parsing the form, so a third party on an
/// open AP cannot forge a mutating POST without first reading the token from
/// the served form.
pub fn body_nonce_matches(body: &[u8], expected: &str) -> bool {
    form_field_raw(body, NONCE_FIELD).is_some_and(|v| nonce_matches(v, expected))
}

/// Experimental: API may change before 1.0.
///
/// Formats a random word as the 8-character lowercase-hex session nonce.
///
/// The tier supplies the word from its hardware RNG.
pub fn format_nonce(value: u32) -> heapless::String<8> {
    let mut nonce = heapless::String::new();
    // Eight hex digits always fit the eight-byte capacity.
    let _ = write!(nonce, "{value:08x}");
    nonce
}

// ── Pre-fill ──────────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Longest `mqtt://host:port` URI the pre-fill can hold: the scheme, a host at
/// its cap, `:` and a five-digit port.
pub const MQTT_URI_PREFILL_MAX: usize = "mqtt://".len() + MQTT_HOST_MAX_LEN + ":65535".len();

/// Experimental: API may change before 1.0.
///
/// Writes `mqtt://host:port` into `out`, returning `true` when it did.
///
/// `out` is cleared first. Nothing is written when `host` or `port` is empty
/// (a host-only or port-only value is not a usable broker URI and would fail
/// `parse_form`) or when the URI does not fit; a truncated URI is never
/// pre-filled.
pub fn compose_mqtt_uri(
    host: &str,
    port: &str,
    out: &mut heapless::String<MQTT_URI_PREFILL_MAX>,
) -> bool {
    out.clear();
    if host.is_empty() || port.is_empty() {
        return false;
    }
    if write!(out, "mqtt://{host}:{port}").is_err() {
        out.clear();
        return false;
    }
    true
}

/// Experimental: API may change before 1.0.
///
/// Non-secret pre-fill values for the portal form.
///
/// Carries the union of both profiles' non-secret inputs; the fields of the
/// other profile stay empty. The secret inputs (`wifi_pass`, `app_key`,
/// `mqtt_pass`) have no field here: they are never pre-filled and are
/// re-entered on every submission. Do not add a credential field.
///
/// The fields borrow from the stored record, the defaults and the device name
/// passed to [`select`](Self::select); only the recomposed broker URI is owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefill<'a> {
    /// Wi-Fi SSID.
    pub wifi_ssid: &'a str,
    /// LoRaWAN DevEUI (`LorawanFieldDevice`).
    pub dev_eui: &'a str,
    /// LoRaWAN JoinEUI (`LorawanFieldDevice`).
    pub join_eui: &'a str,
    /// `mqtt://host:port`, recomposed from host and port (`WifiMqttDevice`).
    pub mqtt_uri: heapless::String<MQTT_URI_PREFILL_MAX>,
    /// MQTT username (`WifiMqttDevice`).
    pub mqtt_user: &'a str,
    /// MQTT client ID (`WifiMqttDevice`).
    pub mqtt_client: &'a str,
    /// OTA URL.
    pub ota_url: &'a str,
    /// Device name.
    pub dev_name: &'a str,
}

impl<'a> Prefill<'a> {
    /// Experimental: API may change before 1.0.
    ///
    /// Chooses the pre-fill source for `profile`.
    ///
    /// A `stored` record of the active profile wins. Otherwise (no record, a
    /// record of the other profile, or a store error the tier mapped to
    /// `None`) the form is seeded from `defaults` plus the configured
    /// `device_name`, with only the active profile's fields filled. A record of
    /// the other profile must not pre-fill this form: its fields do not map.
    ///
    /// `mqtt_uri` is recomposed only when both host and port are set. No
    /// secret is read from either source.
    pub fn select(
        stored: Option<&'a ProvisioningConfig>,
        profile: SchemaProfile,
        defaults: &PortalDefaults<'a>,
        device_name: &'a str,
    ) -> Self {
        let mut prefill = Prefill {
            wifi_ssid: "",
            dev_eui: "",
            join_eui: "",
            mqtt_uri: heapless::String::new(),
            mqtt_user: "",
            mqtt_client: "",
            ota_url: "",
            dev_name: "",
        };
        match stored {
            Some(cfg) if cfg.profile() == profile => {
                prefill.wifi_ssid = cfg.wifi_ssid();
                prefill.ota_url = cfg.ota_url().unwrap_or("");
                prefill.dev_name = cfg.device_name();
                if let Some(lora) = cfg.lora() {
                    prefill.dev_eui = lora.dev_eui_hex();
                    prefill.join_eui = lora.join_eui_hex();
                }
                if let Some(mqtt) = cfg.mqtt() {
                    let mut port = heapless::String::<5>::new();
                    let _ = write!(port, "{}", mqtt.port());
                    compose_mqtt_uri(mqtt.host(), &port, &mut prefill.mqtt_uri);
                    prefill.mqtt_user = mqtt.username().unwrap_or("");
                    prefill.mqtt_client = mqtt.client_id().unwrap_or("");
                }
            }
            _ => {
                prefill.wifi_ssid = defaults.wifi_ssid;
                prefill.ota_url = defaults.ota_url;
                prefill.dev_name = device_name;
                match profile {
                    SchemaProfile::LorawanFieldDevice => {
                        prefill.dev_eui = defaults.dev_eui;
                        prefill.join_eui = defaults.join_eui;
                    }
                    SchemaProfile::WifiMqttDevice => {
                        compose_mqtt_uri(
                            defaults.mqtt_host,
                            defaults.mqtt_port,
                            &mut prefill.mqtt_uri,
                        );
                        prefill.mqtt_user = defaults.mqtt_user;
                        prefill.mqtt_client = defaults.mqtt_client;
                    }
                }
            }
        }
        prefill
    }
}

// ── Start-up plan ─────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Why [`plan_portal`] refused to start a portal.
///
/// Every variant carries only static text or a length, never caller input, so
/// it cannot leak an SSID or password into a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalPlanError {
    /// The tier does not serve this profile.
    ProfileNotSupported(SchemaProfile),
    /// The SSID override is invalid (empty, whitespace-only or over 32 bytes).
    InvalidSsid(&'static str),
    /// The AP password is shorter than the WPA2 minimum.
    PasswordTooShort {
        /// The minimum length.
        min: usize,
    },
    /// The SoftAP configuration is otherwise invalid (password over the
    /// maximum length, channel out of range).
    InvalidApConfig(&'static str),
}

impl fmt::Display for PortalPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortalPlanError::ProfileNotSupported(p) => {
                write!(
                    f,
                    "provisioning profile {p:?} is not supported by this tier"
                )
            }
            PortalPlanError::InvalidSsid(reason) => {
                write!(f, "invalid SoftAP SSID override: {reason}")
            }
            PortalPlanError::PasswordTooShort { min } => {
                write!(f, "AP password too short (minimum {min} characters)")
            }
            PortalPlanError::InvalidApConfig(reason) => {
                write!(f, "invalid SoftAP configuration: {reason}")
            }
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The validated start-up plan of a provisioning portal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalPlan<'a> {
    /// The resolved SoftAP SSID.
    pub ssid: heapless::String<32>,
    /// The AP password, `None` for an open AP.
    pub ap_password: Option<&'a str>,
    /// The 2.4 GHz channel.
    pub channel: u8,
    /// The Wi-Fi country the SoftAP runs under.
    pub country: Country,
    /// The device name pre-filled on a fresh device.
    pub device_name: heapless::String<DEVICE_NAME_MAX_LEN>,
    /// `true` when the device name is MAC-suffixed but a default MQTT client
    /// ID is set: that default overrides the per-device client ID unless the
    /// user clears it in the portal. The tier logs a warning.
    pub client_id_override_warning: bool,
}

impl PortalPlan<'_> {
    /// Experimental: API may change before 1.0.
    ///
    /// The SoftAP configuration: WPA2 with the password, or open, on the
    /// planned channel and country, with the default connection limit.
    pub fn ap_config(&self) -> ApConfig<'_> {
        match self.ap_password {
            Some(pw) => ApConfig::wpa2(self.ssid.as_str(), pw),
            None => ApConfig::open(self.ssid.as_str()),
        }
        .with_channel(self.channel)
        .with_country(self.country)
    }
}

/// Experimental: API may change before 1.0.
///
/// The caller-supplied portal settings [`plan_portal`] validates.
#[derive(Debug, Clone, Copy)]
pub struct PortalPlanInput<'a> {
    /// Complete SSID, used verbatim when `Some`.
    pub ssid_override: Option<&'a str>,
    /// SSID prefix the MAC suffix is appended to.
    pub ssid_prefix: &'a str,
    /// WPA2 password, `None` for an open AP.
    pub ap_password: Option<&'a str>,
    /// 2.4 GHz channel; must be one `country` allows.
    pub channel: u8,
    /// The Wi-Fi country the SoftAP runs under.
    pub country: Country,
    /// Device-name policy.
    pub device_name: DeviceName<'a>,
    /// Non-secret form defaults.
    pub defaults: PortalDefaults<'a>,
    /// The schema profile the portal serves.
    pub profile: SchemaProfile,
}

/// Experimental: API may change before 1.0.
///
/// Validates the portal settings in one fixed order and derives the SSID, the
/// SoftAP configuration and the device name from the AP `mac`.
///
/// The order is: the profile (against the tier's `supported` predicate), then
/// the SSID, then the AP password, then the rest of the SoftAP configuration.
/// The first failure is returned, so an invalid profile is reported before an
/// invalid SSID on every tier.
pub fn plan_portal<'a>(
    input: &PortalPlanInput<'a>,
    mac: &[u8; 6],
    supported: impl Fn(SchemaProfile) -> bool,
) -> Result<PortalPlan<'a>, PortalPlanError> {
    if !supported(input.profile) {
        return Err(PortalPlanError::ProfileNotSupported(input.profile));
    }
    let ssid = super::resolve_softap_ssid(input.ssid_override, input.ssid_prefix, mac)
        .map_err(PortalPlanError::InvalidSsid)?;
    if let Some(pw) = input.ap_password {
        if pw.len() < AP_PASSWORD_MIN_LEN {
            return Err(PortalPlanError::PasswordTooShort {
                min: AP_PASSWORD_MIN_LEN,
            });
        }
    }
    let plan = PortalPlan {
        ssid,
        ap_password: input.ap_password,
        channel: input.channel,
        country: input.country,
        device_name: input.device_name.resolve(mac),
        client_id_override_warning: matches!(input.device_name, DeviceName::MacSuffixed(_))
            && !input.defaults.mqtt_client.is_empty(),
    };
    validate_ap_config(&plan.ap_config()).map_err(PortalPlanError::InvalidApConfig)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::{parse_form, MqttFields};
    use crate::test_support::test_psk;

    extern crate std;
    use alloc::format;
    use alloc::string::String;

    /// Runtime nonce fixture: no literal flows into a `nonce` parameter
    /// (CodeQL `rust/hard-coded-cryptographic-value`).
    fn test_nonce() -> heapless::String<8> {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(1);
        format_nonce(hasher.finish() as u32)
    }

    // ── nonce ────────────────────────────────────────────────────────────────

    #[test]
    fn nonce_matches_equal() {
        let n = test_nonce();
        assert!(nonce_matches(n.as_bytes(), &n));
    }

    #[test]
    fn nonce_mismatch_same_length() {
        let n = test_nonce();
        let mut other = String::from(n.as_str());
        let last = if other.ends_with('0') { "1" } else { "0" };
        other.pop();
        other.push_str(last);
        assert!(!nonce_matches(other.as_bytes(), &n));
    }

    #[test]
    fn nonce_length_mismatch() {
        let n = test_nonce();
        assert!(!nonce_matches(&n.as_bytes()[..7], &n));
        let longer = format!("{n}0");
        assert!(!nonce_matches(longer.as_bytes(), &n));
    }

    #[test]
    fn nonce_empty() {
        let n = test_nonce();
        assert!(!nonce_matches(b"", &n));
        assert!(!nonce_matches(b"", ""));
    }

    /// Security-checklist item 1 lock (moved from the bare-metal portal): the
    /// comparison must not short-circuit on the first differing byte.  The
    /// functional asserts always run; the loose 50x timing bound is skipped
    /// with `SKIP_TIMING_TEST=1` on a noisy host.
    #[test]
    fn nonce_compare_is_position_independent() {
        use core::hint::black_box;
        use std::time::Instant;

        let expected = "00000000";
        let diff_at_0 = b"10000000";
        let diff_at_7 = b"00000001";
        assert!(!nonce_matches(diff_at_0, expected));
        assert!(!nonce_matches(diff_at_7, expected));
        assert!(nonce_matches(b"00000000", expected));

        const ITERATIONS: usize = 1_000;
        let mut t0s = alloc::vec::Vec::with_capacity(ITERATIONS);
        let mut t7s = alloc::vec::Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let _ = black_box(nonce_matches(black_box(diff_at_0), black_box(expected)));
            t0s.push(t.elapsed().as_nanos());
            let t = Instant::now();
            let _ = black_box(nonce_matches(black_box(diff_at_7), black_box(expected)));
            t7s.push(t.elapsed().as_nanos());
        }
        t0s.sort_unstable();
        t7s.sort_unstable();
        let (m0, m7) = (t0s[ITERATIONS / 2], t7s[ITERATIONS / 2]);
        if std::env::var("SKIP_TIMING_TEST").is_err() {
            let ratio = m0.max(m7) as f64 / m0.min(m7).max(1) as f64;
            assert!(
                ratio < 50.0,
                "nonce comparison timing ratio {ratio:.1}x exceeds 50x: \
                 diff-at-0 {m0}ns, diff-at-7 {m7}ns (SKIP_TIMING_TEST=1 skips this)"
            );
        }
    }

    #[test]
    fn format_nonce_is_8_lowercase_hex() {
        assert_eq!(format_nonce(0).as_str(), "00000000");
        assert_eq!(format_nonce(u32::MAX).as_str(), "ffffffff");
        assert_eq!(format_nonce(0x00ab_cdef).as_str(), "00abcdef");
        for v in [1u32, 0x1234_5678, 0xdead_beef] {
            let s = format_nonce(v);
            assert_eq!(s.len(), 8);
            assert!(s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
    }

    #[test]
    fn body_nonce_is_found_anywhere_in_the_body() {
        let n = test_nonce();
        assert!(body_nonce_matches(
            format!("_nonce={n}&wifi_ssid=x").as_bytes(),
            &n
        ));
        assert!(body_nonce_matches(
            format!("wifi_ssid=x&_nonce={n}").as_bytes(),
            &n
        ));
        assert!(!body_nonce_matches(b"wifi_ssid=x", &n));
        assert!(!body_nonce_matches(
            format!("x_nonce={n}&wifi_ssid=x").as_bytes(),
            &n
        ));
    }

    #[test]
    fn form_field_raw_positions() {
        assert_eq!(form_field_raw(b"foo=bar&baz=qux", "foo"), Some(&b"bar"[..]));
        assert_eq!(form_field_raw(b"a=1&foo=bar&c=3", "foo"), Some(&b"bar"[..]));
        assert_eq!(form_field_raw(b"a=1&foo=bar", "foo"), Some(&b"bar"[..]));
        assert_eq!(form_field_raw(b"a=1&foo=", "foo"), Some(&b""[..]));
        assert_eq!(form_field_raw(b"a=1&b=2", "foo"), None);
        assert_eq!(form_field_raw(b"", "foo"), None);
    }

    // ── prefill ──────────────────────────────────────────────────────────────

    fn defaults() -> PortalDefaults<'static> {
        PortalDefaults {
            wifi_ssid: "HomeNet",
            dev_eui: "0102030405060708",
            join_eui: "70B3D57ED0000000",
            mqtt_host: "broker.local",
            mqtt_port: "1883",
            mqtt_user: "sensor",
            mqtt_client: "c3-01",
            ota_url: "http://ota.local/fw.bin",
        }
    }

    /// A `WifiMqttDevice` record whose two secrets are set.
    fn stored_wifi_mqtt() -> ProvisioningConfig {
        let mqtt = MqttFields::from_storage_parts(
            "stored.local".try_into().unwrap(),
            8883,
            Some("stored-user".try_into().unwrap()),
            Some(format!("{}m", test_psk()).as_str().try_into().unwrap()),
            Some("stored-client".try_into().unwrap()),
        );
        ProvisioningConfig::from_storage_parts(
            "StoredNet".try_into().unwrap(),
            test_psk().try_into().unwrap(),
            "http://stored/fw.bin".try_into().unwrap(),
            "user-renamed".try_into().unwrap(),
            None,
            Some(mqtt),
            Default::default(),
        )
    }

    #[test]
    fn stored_record_of_matching_profile_wins() {
        let stored = stored_wifi_mqtt();
        let d = defaults();
        let p = Prefill::select(
            Some(&stored),
            SchemaProfile::WifiMqttDevice,
            &d,
            "rgb-clock",
        );
        assert_eq!(p.wifi_ssid, "StoredNet");
        assert_eq!(p.mqtt_uri.as_str(), "mqtt://stored.local:8883");
        assert_eq!(p.mqtt_user, "stored-user");
        assert_eq!(p.mqtt_client, "stored-client");
        assert_eq!(p.ota_url, "http://stored/fw.bin");
        assert_eq!(p.dev_name, "user-renamed");
    }

    #[test]
    fn stored_record_of_other_profile_falls_back_to_defaults() {
        let stored = stored_wifi_mqtt();
        let d = defaults();
        let p = Prefill::select(
            Some(&stored),
            SchemaProfile::LorawanFieldDevice,
            &d,
            "rgb-clock",
        );
        assert_eq!(p.wifi_ssid, "HomeNet");
        assert_eq!(p.dev_eui, "0102030405060708");
        assert_eq!(p.join_eui, "70B3D57ED0000000");
        assert!(p.mqtt_uri.is_empty(), "MQTT must not fill the LoRaWAN form");
        assert!(p.mqtt_user.is_empty());
        assert_eq!(p.dev_name, "rgb-clock");
    }

    #[test]
    fn fresh_device_prefills_device_name() {
        let d = PortalDefaults::default();
        for profile in [
            SchemaProfile::WifiMqttDevice,
            SchemaProfile::LorawanFieldDevice,
        ] {
            let p = Prefill::select(None, profile, &d, "rgb-clock");
            assert_eq!(p.dev_name, "rgb-clock");
            assert!(p.wifi_ssid.is_empty());
            assert!(p.mqtt_uri.is_empty());
        }
    }

    #[test]
    fn defaults_fill_only_the_active_profile() {
        let d = defaults();
        let p = Prefill::select(None, SchemaProfile::WifiMqttDevice, &d, "");
        assert_eq!(p.mqtt_uri.as_str(), "mqtt://broker.local:1883");
        assert_eq!(p.mqtt_user, "sensor");
        assert_eq!(p.mqtt_client, "c3-01");
        assert_eq!(p.ota_url, "http://ota.local/fw.bin");
        assert!(
            p.dev_eui.is_empty(),
            "EUIs must not fill the Wi-Fi+MQTT form"
        );
        assert!(p.join_eui.is_empty());
    }

    #[test]
    fn uri_composed_only_when_host_and_port_set() {
        let mut out = heapless::String::new();
        assert!(compose_mqtt_uri("broker.local", "1883", &mut out));
        assert_eq!(out.as_str(), "mqtt://broker.local:1883");
        assert!(!compose_mqtt_uri("broker.local", "", &mut out));
        assert!(out.is_empty());
        assert!(!compose_mqtt_uri("", "1883", &mut out));
        assert!(out.is_empty());

        let d = PortalDefaults {
            mqtt_host: "broker.local",
            ..PortalDefaults::default()
        };
        let p = Prefill::select(None, SchemaProfile::WifiMqttDevice, &d, "");
        assert!(p.mqtt_uri.is_empty());
    }

    #[test]
    fn uri_with_a_host_at_its_cap_and_a_five_digit_port_fits() {
        let host = "h".repeat(MQTT_HOST_MAX_LEN);
        let mut out = heapless::String::new();
        assert!(compose_mqtt_uri(&host, "65535", &mut out));
        assert_eq!(out.len(), MQTT_URI_PREFILL_MAX);
        assert!(!compose_mqtt_uri(&host, "655350", &mut out));
        assert!(out.is_empty(), "a URI that does not fit is never truncated");
    }

    #[test]
    fn prefill_never_contains_passwords() {
        let stored = stored_wifi_mqtt();
        let wifi_pass = stored.wifi_password();
        let mqtt_pass = stored.mqtt().and_then(|m| m.password()).unwrap();
        let d = defaults();
        let p = Prefill::select(Some(&stored), SchemaProfile::WifiMqttDevice, &d, "x");
        let fields = [
            p.wifi_ssid,
            p.dev_eui,
            p.join_eui,
            p.mqtt_uri.as_str(),
            p.mqtt_user,
            p.mqtt_client,
            p.ota_url,
            p.dev_name,
        ];
        for f in fields {
            assert!(!f.contains(wifi_pass), "wifi password leaked into {f:?}");
            assert!(!f.contains(mqtt_pass), "mqtt password leaked into {f:?}");
        }
        let debug = format!("{p:?}");
        assert!(!debug.contains(wifi_pass) && !debug.contains(mqtt_pass));
    }

    #[test]
    fn parsed_submission_round_trips_into_prefill() {
        let body = format!(
            "wifi_ssid=home&wifi_pass={}&mqtt_uri=mqtt://b.local:1883&dev_name=hive",
            test_psk()
        );
        let cfg = parse_form(&body, SchemaProfile::WifiMqttDevice).expect("valid body");
        let d = PortalDefaults::default();
        let p = Prefill::select(Some(&cfg), SchemaProfile::WifiMqttDevice, &d, "x");
        assert_eq!(p.mqtt_uri.as_str(), "mqtt://b.local:1883");
        assert_eq!(p.dev_name, "hive");
    }

    // ── plan_portal ──────────────────────────────────────────────────────────

    const MAC: [u8; 6] = [0xa2, 0x00, 0x11, 0x22, 0xc4, 0xf4];

    fn input() -> PortalPlanInput<'static> {
        PortalPlanInput {
            ssid_override: None,
            ssid_prefix: "Rusty",
            ap_password: None,
            channel: 6,
            country: Country::WORLD,
            device_name: DeviceName::Fixed("hive"),
            defaults: PortalDefaults::default(),
            profile: SchemaProfile::WifiMqttDevice,
        }
    }

    #[test]
    fn profile_checked_before_ssid() {
        let i = PortalPlanInput {
            ssid_override: Some(" "),
            ..input()
        };
        assert_eq!(
            plan_portal(&i, &MAC, |_| false),
            Err(PortalPlanError::ProfileNotSupported(
                SchemaProfile::WifiMqttDevice
            ))
        );
        assert!(matches!(
            plan_portal(&i, &MAC, |_| true),
            Err(PortalPlanError::InvalidSsid(_))
        ));
    }

    #[test]
    fn ssid_checked_before_password() {
        let short = &test_psk()[..AP_PASSWORD_MIN_LEN - 1];
        let i = PortalPlanInput {
            ssid_override: Some(""),
            ap_password: Some(short),
            ..input()
        };
        assert!(matches!(
            plan_portal(&i, &MAC, |_| true),
            Err(PortalPlanError::InvalidSsid(_))
        ));
        let i = PortalPlanInput {
            ssid_override: None,
            ..i
        };
        assert_eq!(
            plan_portal(&i, &MAC, |_| true),
            Err(PortalPlanError::PasswordTooShort {
                min: AP_PASSWORD_MIN_LEN
            })
        );
    }

    #[test]
    fn open_ap_when_no_password() {
        let plan = plan_portal(&input(), &MAC, |_| true).expect("valid plan");
        let ap = plan.ap_config();
        assert_eq!(ap.password, None);
        assert_eq!(ap.ssid, "Rusty-C4F4");
    }

    #[test]
    fn wpa2_with_password() {
        let i = PortalPlanInput {
            ap_password: Some(test_psk()),
            ..input()
        };
        let plan = plan_portal(&i, &MAC, |_| true).expect("valid plan");
        assert_eq!(plan.ap_config().password, Some(test_psk()));
    }

    #[test]
    fn channel_applied() {
        let plan = plan_portal(&input(), &MAC, |_| true).expect("valid plan");
        assert_eq!(plan.ap_config().channel, 6);
        let i = PortalPlanInput {
            channel: 14,
            ..input()
        };
        assert!(matches!(
            plan_portal(&i, &MAC, |_| true),
            Err(PortalPlanError::InvalidApConfig(_))
        ));
    }

    #[test]
    fn channel_checked_against_the_country() {
        let world = PortalPlanInput {
            channel: 13,
            ..input()
        };
        assert!(matches!(
            plan_portal(&world, &MAC, |_| true),
            Err(PortalPlanError::InvalidApConfig(_))
        ));
        let de = Country::parse("DE").unwrap();
        let i = PortalPlanInput {
            country: de,
            ..world
        };
        let plan = plan_portal(&i, &MAC, |_| true).expect("13 is allowed in DE");
        assert_eq!(plan.ap_config().country, de);
        assert_eq!(plan.ap_config().channel, 13);
    }

    #[test]
    fn ssid_override_is_used_verbatim() {
        let i = PortalPlanInput {
            ssid_override: Some("Workshop"),
            ..input()
        };
        let plan = plan_portal(&i, &MAC, |_| true).expect("valid plan");
        assert_eq!(plan.ssid.as_str(), "Workshop");
    }

    #[test]
    fn warning_set_only_for_mac_suffixed_with_default_client_id() {
        let with_client = PortalDefaults {
            mqtt_client: "c3-01",
            ..PortalDefaults::default()
        };
        let cases = [
            (DeviceName::MacSuffixed("hive"), with_client, true),
            (
                DeviceName::MacSuffixed("hive"),
                PortalDefaults::default(),
                false,
            ),
            (DeviceName::Fixed("hive"), with_client, false),
            (DeviceName::Fixed("hive"), PortalDefaults::default(), false),
        ];
        for (device_name, defaults, expected) in cases {
            let i = PortalPlanInput {
                device_name,
                defaults,
                ..input()
            };
            let plan = plan_portal(&i, &MAC, |_| true).expect("valid plan");
            assert_eq!(plan.client_id_override_warning, expected, "{device_name:?}");
        }
    }

    #[test]
    fn device_name_resolved_from_mac() {
        let i = PortalPlanInput {
            device_name: DeviceName::MacSuffixed("hive"),
            ..input()
        };
        let plan = plan_portal(&i, &MAC, |_| true).expect("valid plan");
        assert_eq!(plan.device_name.as_str(), "hive-c4f4");
    }
}
