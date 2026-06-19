//! Schema profiles and their per-profile field groups.
//!
//! A [`SchemaProfile`] is a closed, workspace-curated selection of field groups
//! (see [ADR 014](../../../docs/adr/014-wifi-mqtt-provisioning-profile.md)).
//! Exactly two exist: [`SchemaProfile::LorawanFieldDevice`] (Core + LoRaWAN +
//! OTA) and [`SchemaProfile::WifiMqttDevice`] (Core + MQTT + OTA). Each owns the
//! canonical [`Field`] list its form renders and validates, returned by
//! [`SchemaProfile::fields`].
//!
//! The profile-specific values live in the [`LoraFields`] and [`MqttFields`]
//! groups, carried as [`Option`]s on
//! [`ProvisioningConfig`](crate::provisioning::ProvisioningConfig).

use core::fmt;

use crate::provisioning::config::{
    APP_KEY_HEX_LEN, EUI_HEX_LEN, MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN, MQTT_USER_MAX_LEN,
};
use crate::provisioning::error::Field;

/// The canonical fields of [`SchemaProfile::LorawanFieldDevice`] (Core + LoRaWAN
/// + OTA), indexed positionally for the working slots in `parse_form`.
const LORAWAN_FIELDS: [Field; 7] = [
    Field::WifiSsid,
    Field::WifiPassword,
    Field::DevEui,
    Field::JoinEui,
    Field::AppKey,
    Field::OtaUrl,
    Field::DeviceName,
];

/// The canonical fields of [`SchemaProfile::WifiMqttDevice`] (Core + MQTT +
/// OTA), indexed positionally for the working slots in `parse_form`.
const WIFI_MQTT_FIELDS: [Field; 8] = [
    Field::WifiSsid,
    Field::WifiPassword,
    Field::MqttUri,
    Field::MqttUser,
    Field::MqttPass,
    Field::MqttClient,
    Field::OtaUrl,
    Field::DeviceName,
];

/// Experimental: API may change before 1.0.
///
/// The provisioning schema a submission is parsed under.
///
/// A profile is a closed combination of field groups, not a generic schema:
/// the pure crate still owns every validation rule, and the host only selects
/// which curated combination it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaProfile {
    /// Core + LoRaWAN + OTA — the beekeeper field-device schema (unchanged
    /// from v1).
    LorawanFieldDevice,
    /// Core + MQTT + OTA — Wi-Fi credentials, an MQTT broker, an OTA URL, and a
    /// device name, with no LoRaWAN.
    WifiMqttDevice,
}

impl SchemaProfile {
    /// Experimental: API may change before 1.0.
    ///
    /// The canonical field list this profile's form renders and validates.
    pub fn fields(self) -> &'static [Field] {
        match self {
            SchemaProfile::LorawanFieldDevice => &LORAWAN_FIELDS,
            SchemaProfile::WifiMqttDevice => &WIFI_MQTT_FIELDS,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The NVS discriminator string for this profile (`"lorawan"` /
    /// `"wifi_mqtt"`), matching the `profile` key the store writes.
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaProfile::LorawanFieldDevice => "lorawan",
            SchemaProfile::WifiMqttDevice => "wifi_mqtt",
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Parses the NVS discriminator string back into a profile.
    ///
    /// Returns `None` for any unrecognised value; the store maps an absent
    /// `profile` key (a v1 record) to [`SchemaProfile::LorawanFieldDevice`]
    /// itself rather than relying on this.
    pub fn from_nvs_str(s: &str) -> Option<SchemaProfile> {
        match s {
            "lorawan" => Some(SchemaProfile::LorawanFieldDevice),
            "wifi_mqtt" => Some(SchemaProfile::WifiMqttDevice),
            _ => None,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The validated LoRaWAN OTAA credentials of a
/// [`SchemaProfile::LorawanFieldDevice`] submission.
#[derive(Clone, PartialEq, Eq)]
pub struct LoraFields {
    pub(crate) dev_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub(crate) join_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub(crate) app_key_hex: heapless::String<APP_KEY_HEX_LEN>,
}

impl LoraFields {
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
    /// Builds a [`crate::lora::LoraConfig`] from the validated OTAA credentials.
    ///
    /// The DevEUI, JoinEUI, and AppKey were validated at parse time with the
    /// exact length-and-hex rules [`crate::lora::LoraConfig::from_hex_strings`]
    /// applies, so the `Option` it returns is `Some` by construction. The
    /// `expect` below is therefore unreachable for any value this type can
    /// hold; it would only fire if the parse-time and `from_hex_strings`
    /// validation rules drifted apart, which the host tests guard against.
    pub fn to_lora_config(&self, region: crate::lora::Region) -> crate::lora::LoraConfig {
        crate::lora::LoraConfig::from_hex_strings(
            region,
            &self.dev_eui_hex,
            &self.join_eui_hex,
            &self.app_key_hex,
        )
        .expect("OTAA credentials validated at parse time")
    }
}

impl fmt::Debug for LoraFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoraFields")
            .field("dev_eui_hex", &self.dev_eui_hex())
            .field("join_eui_hex", &self.join_eui_hex())
            .field("app_key_hex", &"<redacted>")
            .finish()
    }
}

/// Experimental: API may change before 1.0.
///
/// The validated MQTT broker fields of a [`SchemaProfile::WifiMqttDevice`]
/// submission.
///
/// `host` and `port` are split from the single `mqtt_uri` form input at parse
/// time so the load-then-connect path needs no re-parsing. `username`,
/// `password`, and `client_id` are optional: an anonymous, host-derived-client
/// connection leaves all three `None`.
#[derive(Clone, PartialEq, Eq)]
pub struct MqttFields {
    pub(crate) host: heapless::String<MQTT_HOST_MAX_LEN>,
    pub(crate) port: u16,
    pub(crate) username: Option<heapless::String<MQTT_USER_MAX_LEN>>,
    pub(crate) password: Option<heapless::String<MQTT_PASS_MAX_LEN>>,
    pub(crate) client_id: Option<heapless::String<{ crate::mqtt::CLIENT_ID_MAX_LEN }>>,
}

impl MqttFields {
    /// Experimental: API may change before 1.0.
    ///
    /// The validated broker host (non-empty).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The validated broker port (non-zero).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The MQTT username, or `None` for an anonymous connection.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The MQTT password, or `None` when no password was supplied.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The MQTT client ID, or `None` when the host should derive one at boot.
    ///
    /// When `None`, the host derives a client ID — for example from the device
    /// name, sanitised and truncated. [`DEVICE_NAME_MAX_LEN`] (24) exceeds the
    /// 23-byte client-ID cap, so a naive "client ID = device name" derivation
    /// must truncate.
    ///
    /// [`DEVICE_NAME_MAX_LEN`]: crate::provisioning::DEVICE_NAME_MAX_LEN
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Construct `MqttFields` directly from validated values.
    ///
    /// Intended for storage adapters that have just decoded a previously-stored
    /// record and CRC-verified its integrity. The constructor itself performs
    /// no validation; the values must have originated from a previous
    /// `parse_form` and been round-tripped through a checked encode / decode
    /// pair.
    pub fn from_storage_parts(
        host: heapless::String<MQTT_HOST_MAX_LEN>,
        port: u16,
        username: Option<heapless::String<MQTT_USER_MAX_LEN>>,
        password: Option<heapless::String<MQTT_PASS_MAX_LEN>>,
        client_id: Option<heapless::String<{ crate::mqtt::CLIENT_ID_MAX_LEN }>>,
    ) -> Self {
        Self {
            host,
            port,
            username,
            password,
            client_id,
        }
    }
}

impl fmt::Debug for MqttFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttFields")
            .field("host", &self.host())
            .field("port", &self.port())
            .field("username", &self.username())
            .field("password", &self.password().map(|_| "<redacted>"))
            .field("client_id", &self.client_id())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lorawan_fields_set_is_exact() {
        assert_eq!(
            SchemaProfile::LorawanFieldDevice.fields(),
            &[
                Field::WifiSsid,
                Field::WifiPassword,
                Field::DevEui,
                Field::JoinEui,
                Field::AppKey,
                Field::OtaUrl,
                Field::DeviceName,
            ]
        );
    }

    #[test]
    fn wifi_mqtt_fields_set_is_exact() {
        assert_eq!(
            SchemaProfile::WifiMqttDevice.fields(),
            &[
                Field::WifiSsid,
                Field::WifiPassword,
                Field::MqttUri,
                Field::MqttUser,
                Field::MqttPass,
                Field::MqttClient,
                Field::OtaUrl,
                Field::DeviceName,
            ]
        );
    }

    #[test]
    fn discriminator_round_trips() {
        for profile in [
            SchemaProfile::LorawanFieldDevice,
            SchemaProfile::WifiMqttDevice,
        ] {
            assert_eq!(SchemaProfile::from_nvs_str(profile.as_str()), Some(profile));
        }
        assert_eq!(SchemaProfile::from_nvs_str("nonsense"), None);
    }
}
