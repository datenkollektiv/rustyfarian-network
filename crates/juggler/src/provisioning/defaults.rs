//! Non-secret pre-fill defaults for the provisioning portal form.
//!
//! [`PortalDefaults`] carries compile-time (or otherwise caller-supplied)
//! values used to **seed the portal form on a fresh / factory-reset device**,
//! where the [`ProvisioningStore`](crate::provisioning) holds nothing to
//! pre-fill from. It is consumed by both provisioning tiers
//! (`rustyfarian-esp-hal-network` and `rustyfarian-esp-idf-network`) as the
//! empty-store fallback for their `load_prefill` step.
//!
//! # Security
//!
//! This struct is **non-secret by construction** — it deliberately carries no
//! `wifi_pass`, `mqtt_pass`, or `app_key` field. Secrets are never pre-filled
//! into portal HTML (the templates carry no `{{WIFI_PASS}}` / `{{MQTT_PASS}}` /
//! `{{APP_KEY}}` placeholder) and must be re-entered on every submission.
//! Do **not** extend this struct with any credential field.
//!
//! All fields borrow `&str`; an empty string (`""`) means "no default — render
//! the field empty". The [`Default`] impl yields an all-empty value, i.e. the
//! same behavior as before this type existed.
//!
//! # Contract: best-effort, not validated
//!
//! Defaults are a convenience for the *initial* form render only. Values are
//! passed through to the form as-is — they are **not validated** at this layer.
//! An over-long or malformed default surfaces as an ordinary per-field
//! validation error on submitting (via [`parse_form`](crate::provisioning::parse_form)),
//! exactly as if the value had been typed by hand. Consuming tiers may store
//! their owned copy in a bounded buffer (the bare-metal tier truncates to fixed
//! render buffers because it has no heap); this never widens what a submission
//! is allowed to contain.

/// Non-secret pre-fill values that seed the provisioning portal form when the
/// store is empty (fresh / factory-reset device).
///
/// The active [`SchemaProfile`](crate::provisioning::SchemaProfile) selects
/// which fields are relevant: `LorawanFieldDevice` uses `dev_eui` / `join_eui`,
/// `WifiMqttDevice` uses `mqtt_host` / `mqtt_port` / `mqtt_user` /
/// `mqtt_client`; both use `wifi_ssid` and `ota_url`. Fields not relevant to
/// the active profile are ignored.
///
/// The device name is intentionally absent — it flows through the portal's
/// existing `device_name` configuration and its `{{DEV_NAME}}` fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalDefaults<'a> {
    /// Wi-Fi SSID default (both profiles).
    pub wifi_ssid: &'a str,
    /// LoRaWAN DevEUI default, 16 hex chars MSB-first (`LorawanFieldDevice`).
    pub dev_eui: &'a str,
    /// LoRaWAN JoinEUI/AppEUI default, 16 hex chars MSB-first
    /// (`LorawanFieldDevice`). Sourced from `LORAWAN_APP_EUI`.
    pub join_eui: &'a str,
    /// MQTT broker host default, recomposed with `mqtt_port` into
    /// `mqtt://host:port` (`WifiMqttDevice`).
    pub mqtt_host: &'a str,
    /// MQTT broker port default as a decimal string; `""` means "no default"
    /// (`WifiMqttDevice`).
    pub mqtt_port: &'a str,
    /// MQTT username default (`WifiMqttDevice`).
    pub mqtt_user: &'a str,
    /// MQTT client-ID default (`WifiMqttDevice`).
    pub mqtt_client: &'a str,
    /// OTA update URL default (both profiles).
    pub ota_url: &'a str,
}

impl<'a> PortalDefaults<'a> {
    /// An all-empty set of defaults (no field pre-filled).
    ///
    /// Equivalent to [`PortalDefaults::default()`]; provided as a `const` for
    /// use in `const` contexts.
    pub const fn none() -> Self {
        Self {
            wifi_ssid: "",
            dev_eui: "",
            join_eui: "",
            mqtt_host: "",
            mqtt_port: "",
            mqtt_user: "",
            mqtt_client: "",
            ota_url: "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_empty() {
        let d = PortalDefaults::default();
        assert_eq!(d.wifi_ssid, "");
        assert_eq!(d.dev_eui, "");
        assert_eq!(d.join_eui, "");
        assert_eq!(d.mqtt_host, "");
        assert_eq!(d.mqtt_port, "");
        assert_eq!(d.mqtt_user, "");
        assert_eq!(d.mqtt_client, "");
        assert_eq!(d.ota_url, "");
    }

    #[test]
    fn none_matches_default() {
        assert_eq!(PortalDefaults::none(), PortalDefaults::default());
    }
}
