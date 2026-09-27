//! The validated [`ProvisioningConfig`] and the field-size constants that
//! bound its storage.
//!
//! A `ProvisioningConfig` is only ever produced by
//! [`parse_form`](crate::provisioning::parse_form), so by construction every field it holds
//! has already passed validation.

use core::fmt;

use crate::provisioning::form::ExtraField;
use crate::provisioning::profile::{LoraFields, MqttFields, SchemaProfile};

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

/// Capacity of the [`FieldErrors`](crate::provisioning::FieldErrors) accumulator.
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
/// Construct it only via [`parse_form`](crate::provisioning::parse_form); the field accessors
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
/// `"<redacted>"`, following the [`crate::lora::LoraConfig`] precedent. It
/// deliberately redacts *fewer* fields than `LoraConfig`: the DevEUI, JoinEUI,
/// and the MQTT username are device identifiers rather than secrets and are
/// useful verbatim in field logs, so they are shown.
///
/// # Secret lifetime
///
/// When a `ProvisioningConfig`, [`LoraFields`], or [`MqttFields`] value is
/// dropped, the Wi-Fi password, the MQTT password, the AppKey, and every
/// extra field's value in *that value's own storage* are overwritten (zeroed,
/// via [`zeroize`]) before the memory is released.
///
/// This is best-effort hygiene for the final location only, not a guarantee
/// that no copy of a secret survives:
///
/// - The fields are inline `heapless` buffers, so every Rust move (returning
///   the config, `Option::take`, pushing into a collection) is a bitwise copy
///   that leaves the bytes behind in the source location, which is never
///   scrubbed.
/// - Buffers outside this type are not covered: the raw form body and the
///   parser's percent-decode scratch, NVS encode buffers, and any `&str`
///   the caller copies out of an accessor.
/// - A panic dump that prints live stack frames while the config is still
///   alive exposes it regardless; keeping fewer copies on the stack is the
///   mitigation for that case
///   (bug 002, `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md`).
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisioningConfig {
    pub(crate) wifi_ssid: heapless::String<{ crate::wifi::SSID_MAX_LEN }>,
    pub(crate) wifi_password: heapless::String<{ crate::wifi::PASSWORD_MAX_LEN }>,
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
    ///
    /// An empty string means "no OTA configured". This is only possible under
    /// [`SchemaProfile::WifiMqttDevice`], where the URL is optional (ADR 014
    /// amendment); `LorawanFieldDevice` configs always carry a non-empty URL.
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
        wifi_ssid: heapless::String<{ crate::wifi::SSID_MAX_LEN }>,
        wifi_password: heapless::String<{ crate::wifi::PASSWORD_MAX_LEN }>,
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

impl Drop for ProvisioningConfig {
    /// Scrubs `wifi_password` and every extra field's value before the memory
    /// is released; see the `# Secret lifetime` section above.
    ///
    /// `lora` and `mqtt` scrub their own secret (the AppKey / MQTT password
    /// respectively) via their own `Drop` impls, chained automatically once
    /// this function returns. `wifi_ssid`, `ota_url`, `device_name`, and
    /// every extra field's *key* are not secrets and are left as-is.
    fn drop(&mut self) {
        crate::provisioning::secret::scrub(&mut self.wifi_password);
        for extra in self.extras.iter_mut() {
            crate::provisioning::secret::scrub(&mut extra.value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::provisioning::parse_form;
    use crate::provisioning::SchemaProfile;
    use alloc::format;

    extern crate std;

    const TEST_APP_KEY_HEX: &str = "00112233445566778899AABBCCDDEEFF";

    /// This process's Wi-Fi test key, generated once on first use.
    ///
    /// Derived from OS entropy rather than written as a literal, so no fixed key
    /// material exists in the source -- the same pattern as `test_psk()` in
    /// `juggler::wifi`.  These tests only check parsing and `Debug` redaction,
    /// so any well-formed value works.
    fn test_psk() -> &'static str {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        use std::sync::OnceLock;

        static PSK: OnceLock<alloc::string::String> = OnceLock::new();
        PSK.get_or_init(|| {
            alloc::format!("{:016x}", {
                let mut hasher = RandomState::new().build_hasher();
                hasher.write_u8(0);
                hasher.finish()
            })
        })
        .as_str()
    }

    fn parsed_config() -> crate::provisioning::ProvisioningConfig {
        let psk = test_psk();
        let body = format!(
            "wifi_ssid=home&wifi_pass={psk}&dev_eui=0011223344556677\
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
        assert!(!rendered.contains(test_psk()));
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
            .to_lora_config(crate::lora::Region::EU868);
        assert_eq!(lora.region, crate::lora::Region::EU868);
        assert_eq!(lora.dev_eui[7], 0x77);
    }

    #[test]
    fn lorawan_profile_round_trips() {
        let cfg = parsed_config();
        assert_eq!(cfg.profile(), SchemaProfile::LorawanFieldDevice);
        assert!(cfg.lora().is_some());
        assert!(cfg.mqtt().is_none());
    }

    /// A `WifiMqttDevice` fixture that routes the runtime test key through
    /// every secret slot a portal submission can carry: the Wi-Fi password,
    /// the MQTT password, and an opaque extra field.
    fn mqtt_config_with_extra() -> crate::provisioning::ProvisioningConfig {
        let psk = test_psk();
        let body = format!(
            "wifi_ssid=home&wifi_pass={psk}&mqtt_uri=mqtt://broker.local:1883\
             &mqtt_user=hive&mqtt_pass={psk}&ota_url=http://example.com/fw.bin\
             &dev_name=hive&api_token={psk}"
        );
        parse_form(&body, SchemaProfile::WifiMqttDevice).expect("valid fixture body")
    }

    #[test]
    fn debug_redacts_mqtt_password_and_extra_values() {
        let cfg = mqtt_config_with_extra();
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("api_token"), "extra keys stay visible");
        assert!(
            !rendered.contains(test_psk()),
            "leaked a secret: {rendered}"
        );
    }

    /// Address and length of a live secret buffer, recorded so the same bytes
    /// can be re-read after the owning config has been dropped in place.
    fn window(s: &str) -> (*const u8, usize) {
        (s.as_ptr(), s.len())
    }

    /// Re-read a recorded window.
    ///
    /// # Safety
    ///
    /// The storage the window points into must still be allocated: the tests
    /// keep the config inside a `ManuallyDrop` on their own stack frame, so
    /// running `Drop` in place releases nothing.
    unsafe fn bytes_at((ptr, len): (*const u8, usize)) -> &'static [u8] {
        core::slice::from_raw_parts(ptr, len)
    }

    fn assert_scrubbed(label: &str, w: (*const u8, usize)) {
        let bytes = unsafe { bytes_at(w) };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "{label} buffer still holds {} non-zero bytes after drop",
            bytes.iter().filter(|&&b| b != 0).count()
        );
    }

    #[test]
    fn drop_scrubs_wifi_password_mqtt_password_and_extra_value() {
        use core::mem::ManuallyDrop;

        let mut cfg = ManuallyDrop::new(mqtt_config_with_extra());
        let wifi = window(cfg.wifi_password());
        let mqtt = window(
            cfg.mqtt()
                .expect("mqtt group")
                .password()
                .expect("password"),
        );
        let extra = window(&cfg.extras()[0].value);
        assert_eq!(cfg.extras()[0].key, "api_token");

        // Sanity: the buffers hold the secret while the config is alive.
        assert_eq!(unsafe { bytes_at(wifi) }, test_psk().as_bytes());
        assert_eq!(unsafe { bytes_at(mqtt) }, test_psk().as_bytes());
        assert_eq!(unsafe { bytes_at(extra) }, test_psk().as_bytes());

        unsafe { ManuallyDrop::drop(&mut cfg) };

        assert_scrubbed("wifi_password", wifi);
        assert_scrubbed("mqtt.password", mqtt);
        assert_scrubbed("extras[0].value", extra);
    }

    #[test]
    fn drop_scrubs_lora_app_key() {
        use core::mem::ManuallyDrop;

        let mut cfg = ManuallyDrop::new(parsed_config());
        let app_key = window(cfg.lora().expect("lora group").app_key_hex());
        assert_eq!(unsafe { bytes_at(app_key) }, TEST_APP_KEY_HEX.as_bytes());

        unsafe { ManuallyDrop::drop(&mut cfg) };

        assert_scrubbed("lora.app_key_hex", app_key);
    }
}
