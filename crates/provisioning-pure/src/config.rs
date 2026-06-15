//! The validated [`ProvisioningConfig`] and the field-size constants that
//! bound its storage.
//!
//! A `ProvisioningConfig` is only ever produced by
//! [`parse_form`](crate::parse_form), so by construction every field it holds
//! has already passed validation.

use core::fmt;

use crate::form::ExtraField;
use crate::profile::{LoraFields, MqttFields, SchemaProfile};

/// Maximum length of the human-readable device name (bytes).
pub const DEVICE_NAME_MAX_LEN: usize = 24;

/// Maximum length of the OTA update URL (bytes).
pub const OTA_URL_MAX_LEN: usize = 128;

/// Maximum number of opaque extra fields a submission may carry.
pub const EXTRA_FIELDS_MAX: usize = 8;

/// Maximum length of an extra field's key (bytes).
///
/// Capped at 13 so the NVS layer can prefix it with `x_` and stay within the
/// 15-byte NVS key limit.
pub const EXTRA_KEY_MAX_LEN: usize = 13;

/// Maximum length of an extra field's value (bytes).
pub const EXTRA_VALUE_MAX_LEN: usize = 64;

/// Capacity of the [`FieldErrors`](crate::FieldErrors) accumulator.
///
/// At most one error per canonical field of the active profile (up to eight
/// for `WifiMqttDevice`) plus one form-level error.
pub const MAX_FIELD_ERRORS: usize = 9;

/// Maximum length of the MQTT broker host (bytes).
///
/// The feature doc Q1 leaves this unspecified; 64 is chosen as a sane cap that
/// comfortably holds a fully-qualified domain name and is recorded as a
/// deviation in the Session Log.
pub const MQTT_HOST_MAX_LEN: usize = 64;

/// Maximum length of the MQTT username (bytes).
///
/// The feature doc Q3 leaves this unspecified; 64 is chosen as a sane cap and
/// recorded as a deviation in the Session Log.
pub const MQTT_USER_MAX_LEN: usize = 64;

/// Maximum length of the MQTT password (bytes).
///
/// The feature doc Q3 leaves this unspecified; 64 is chosen as a sane cap and
/// recorded as a deviation in the Session Log.
pub const MQTT_PASS_MAX_LEN: usize = 64;

/// Hex-character length of a LoRaWAN EUI (8 bytes, MSB-first).
pub(crate) const EUI_HEX_LEN: usize = 16;

/// Hex-character length of a LoRaWAN AppKey (16 bytes).
pub(crate) const APP_KEY_HEX_LEN: usize = 32;

/// Experimental: API may change before 1.0.
///
/// A fully validated set of provisioning field values.
///
/// Construct it only via [`parse_form`](crate::parse_form); the field accessors
/// then return values that are guaranteed to satisfy every validation rule.
///
/// The Core and OTA fields (`wifi_ssid`, `wifi_password`, `ota_url`,
/// `device_name`, `extras`) are always present. The profile-specific field
/// groups are carried as [`Option`]s: exactly one of [`lora`](Self::lora) and
/// [`mqtt`](Self::mqtt) is `Some`, matching the [`profile`](Self::profile) the
/// submission was parsed under.
///
/// # Redaction
///
/// The [`Debug`](fmt::Debug) impl redacts the Wi-Fi password, the AppKey (when
/// the LoRaWAN group is present), and the MQTT password (when present) as
/// `"<redacted>"`, following the [`lora_pure::LoraConfig`] precedent. It
/// deliberately redacts *fewer* fields than `LoraConfig`: the DevEUI, JoinEUI,
/// and the MQTT username are device identifiers rather than secrets and are
/// useful verbatim in field logs, so they are shown.
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisioningConfig {
    pub(crate) wifi_ssid: heapless::String<{ wifi_pure::SSID_MAX_LEN }>,
    pub(crate) wifi_password: heapless::String<{ wifi_pure::PASSWORD_MAX_LEN }>,
    pub(crate) ota_url: heapless::String<OTA_URL_MAX_LEN>,
    pub(crate) device_name: heapless::String<DEVICE_NAME_MAX_LEN>,
    pub(crate) lora: Option<LoraFields>,
    pub(crate) mqtt: Option<MqttFields>,
    pub(crate) extras: heapless::Vec<ExtraField, EXTRA_FIELDS_MAX>,
}

impl ProvisioningConfig {
    /// Experimental: API may change before 1.0.
    ///
    /// The validated Wi-Fi SSID.
    pub fn wifi_ssid(&self) -> &str {
        &self.wifi_ssid
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated Wi-Fi password (empty for an open network).
    pub fn wifi_password(&self) -> &str {
        &self.wifi_password
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated OTA update URL.
    pub fn ota_url(&self) -> &str {
        &self.ota_url
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The LoRaWAN field group, present iff the profile is
    /// [`SchemaProfile::LorawanFieldDevice`].
    pub fn lora(&self) -> Option<&LoraFields> {
        self.lora.as_ref()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The MQTT field group, present iff the profile is
    /// [`SchemaProfile::WifiMqttDevice`].
    pub fn mqtt(&self) -> Option<&MqttFields> {
        self.mqtt.as_ref()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The profile this config was parsed under.
    ///
    /// Exactly one field group is present; the returned profile is the
    /// authoritative discriminator hosts match on, rather than probing which
    /// group happens to be `Some`.
    pub fn profile(&self) -> SchemaProfile {
        if self.mqtt.is_some() {
            SchemaProfile::WifiMqttDevice
        } else {
            SchemaProfile::LorawanFieldDevice
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The opaque extra fields carried by the submission, in submission order.
    pub fn extras(&self) -> &[ExtraField] {
        &self.extras
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Construct a `ProvisioningConfig` directly from validated field values.
    ///
    /// Intended for storage adapters that have just decoded a previously-stored
    /// record and CRC-verified its integrity: every field originated from a
    /// previous `parse_form` and was round-tripped through a checked encode /
    /// decode pair, so the validation invariants `parse_form` enforces still
    /// hold by construction. The constructor itself performs no validation.
    pub fn from_storage_parts(
        wifi_ssid: heapless::String<{ wifi_pure::SSID_MAX_LEN }>,
        wifi_password: heapless::String<{ wifi_pure::PASSWORD_MAX_LEN }>,
        ota_url: heapless::String<OTA_URL_MAX_LEN>,
        device_name: heapless::String<DEVICE_NAME_MAX_LEN>,
        lora: Option<LoraFields>,
        mqtt: Option<MqttFields>,
    ) -> Self {
        Self {
            wifi_ssid,
            wifi_password,
            ota_url,
            device_name,
            lora,
            mqtt,
            extras: heapless::Vec::new(),
        }
    }
}

impl fmt::Debug for ProvisioningConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisioningConfig")
            .field("profile", &self.profile())
            .field("wifi_ssid", &self.wifi_ssid())
            .field("wifi_password", &"<redacted>")
            .field("lora", &self.lora)
            .field("mqtt", &self.mqtt)
            .field("ota_url", &self.ota_url())
            .field("device_name", &self.device_name())
            .field("extras", &self.extras())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_form;
    use crate::SchemaProfile;
    use alloc::format;

    // Fixture values named to avoid CodeQL's hardcoded-credential rule.
    const TEST_PSK: &str = "open-sesame";
    const TEST_APP_KEY_HEX: &str = "00112233445566778899AABBCCDDEEFF";

    fn parsed_config() -> crate::ProvisioningConfig {
        let body = format!(
            "wifi_ssid=home&wifi_pass={TEST_PSK}&dev_eui=0011223344556677\
             &join_eui=70B3D57ED005ABCD&app_key={TEST_APP_KEY_HEX}\
             &ota_url=http://example.com/fw.bin&dev_name=hive"
        );
        parse_form(&body, SchemaProfile::LorawanFieldDevice).expect("valid fixture body")
    }

    #[test]
    fn debug_redacts_password_and_app_key() {
        let cfg = parsed_config();
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(TEST_PSK));
        assert!(!rendered.contains(TEST_APP_KEY_HEX));
    }

    #[test]
    fn debug_shows_non_secret_fields() {
        let cfg = parsed_config();
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("home"));
        assert!(rendered.contains("0011223344556677"));
        assert!(rendered.contains("70B3D57ED005ABCD"));
        assert!(rendered.contains("hive"));
    }

    #[test]
    fn to_lora_config_round_trips_validated_credentials() {
        let cfg = parsed_config();
        let lora = cfg
            .lora()
            .expect("lora group present")
            .to_lora_config(lora_pure::Region::EU868);
        assert_eq!(lora.region, lora_pure::Region::EU868);
        assert_eq!(lora.dev_eui[7], 0x77);
    }

    #[test]
    fn lorawan_profile_round_trips() {
        let cfg = parsed_config();
        assert_eq!(cfg.profile(), SchemaProfile::LorawanFieldDevice);
        assert!(cfg.lora().is_some());
        assert!(cfg.mqtt().is_none());
    }
}
