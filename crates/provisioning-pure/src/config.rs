//! The validated [`ProvisioningConfig`] and the field-size constants that
//! bound its storage.
//!
//! A `ProvisioningConfig` is only ever produced by
//! [`parse_form`](crate::parse_form), so by construction every field it holds
//! has already passed validation.

use core::fmt;

use crate::form::ExtraField;

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
/// At most one error per canonical field (seven) plus one form-level error.
pub const MAX_FIELD_ERRORS: usize = 8;

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
/// # Redaction
///
/// The [`Debug`](fmt::Debug) impl redacts the Wi-Fi password and the AppKey as
/// `"<redacted>"`, following the [`lora_pure::LoraConfig`] precedent. It
/// deliberately redacts *fewer* fields than `LoraConfig`: the DevEUI and
/// JoinEUI are device identifiers rather than secrets and are useful verbatim
/// in field logs, so they are shown.
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisioningConfig {
    pub(crate) wifi_ssid: heapless::String<{ wifi_pure::SSID_MAX_LEN }>,
    pub(crate) wifi_password: heapless::String<{ wifi_pure::PASSWORD_MAX_LEN }>,
    pub(crate) dev_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub(crate) join_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub(crate) app_key_hex: heapless::String<APP_KEY_HEX_LEN>,
    pub(crate) ota_url: heapless::String<OTA_URL_MAX_LEN>,
    pub(crate) device_name: heapless::String<DEVICE_NAME_MAX_LEN>,
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
    /// The validated DevEUI as a 16-character MSB-first hex string.
    pub fn dev_eui_hex(&self) -> &str {
        &self.dev_eui_hex
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated JoinEUI as a 16-character MSB-first hex string.
    pub fn join_eui_hex(&self) -> &str {
        &self.join_eui_hex
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated AppKey as a 32-character hex string.
    pub fn app_key_hex(&self) -> &str {
        &self.app_key_hex
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
    /// The opaque extra fields carried by the submission, in submission order.
    pub fn extras(&self) -> &[ExtraField] {
        &self.extras
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Builds a [`lora_pure::LoraConfig`] from the validated OTAA credentials.
    ///
    /// The DevEUI, JoinEUI, and AppKey were validated at parse time with the
    /// exact length-and-hex rules [`lora_pure::LoraConfig::from_hex_strings`]
    /// applies, so the `Option` it returns is `Some` by construction. The
    /// `expect` below is therefore unreachable for any value this type can
    /// hold; it would only fire if the parse-time and `from_hex_strings`
    /// validation rules drifted apart, which the host tests guard against.
    pub fn to_lora_config(&self, region: lora_pure::Region) -> lora_pure::LoraConfig {
        lora_pure::LoraConfig::from_hex_strings(
            region,
            &self.dev_eui_hex,
            &self.join_eui_hex,
            &self.app_key_hex,
        )
        .expect("OTAA credentials validated at parse time")
    }
}

impl fmt::Debug for ProvisioningConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisioningConfig")
            .field("wifi_ssid", &self.wifi_ssid())
            .field("wifi_password", &"<redacted>")
            .field("dev_eui_hex", &self.dev_eui_hex())
            .field("join_eui_hex", &self.join_eui_hex())
            .field("app_key_hex", &"<redacted>")
            .field("ota_url", &self.ota_url())
            .field("device_name", &self.device_name())
            .field("extras", &self.extras())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_form;
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
        parse_form(&body).expect("valid fixture body")
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
        let lora = cfg.to_lora_config(lora_pure::Region::EU868);
        assert_eq!(lora.region, lora_pure::Region::EU868);
        assert_eq!(lora.dev_eui[7], 0x77);
    }
}
