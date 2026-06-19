//! Portal HTML templates shared by all provisioning tiers.
//!
//! Both templates are included verbatim at compile time via [`include_str!`].
//! They live in `src/provisioning/assets/` alongside this module and are the
//! single source of truth for the captive-portal HTML rendered by both the
//! ESP-IDF and bare-metal (`esp-hal`) provisioning crates.
//!
//! # Placeholders
//!
//! Templates use `{{KEY}}` tokens substituted at render time by the
//! platform-specific portal layer.  Only non-secret fields carry
//! placeholders — secret inputs (`wifi_pass`, `app_key`, `mqtt_pass`)
//! are always rendered empty and must be re-entered on every submission.
//!
//! Common placeholders (present in both templates):
//! - `{{NONCE}}` — per-session CSRF token
//! - `{{ERRORS}}` — rendered error block (or empty string)
//! - `{{WIFI_SSID}}` — pre-fill for the Wi-Fi SSID field
//! - `{{OTA_URL}}` — pre-fill for the OTA URL field
//! - `{{DEV_NAME}}` — pre-fill for the device-name field
//! - `{{FW_VER}}` — firmware version string (from `PortalRenderConfig`)
//!
//! Wi-Fi + MQTT profile additional placeholders:
//! - `{{MQTT_URI}}`, `{{MQTT_USER}}`, `{{MQTT_CLIENT}}`
//!
//! LoRaWAN profile additional placeholders:
//! - `{{DEV_EUI}}`, `{{JOIN_EUI}}`

/// The Wi-Fi + MQTT profile portal HTML template.
///
/// Includes fields for Wi-Fi credentials, MQTT broker URI, optional MQTT
/// username / password / client ID, OTA URL, and device name.  No
/// `{{MQTT_PASS}}` or `{{WIFI_PASS}}` placeholder exists — secrets are
/// never pre-filled.
pub const WIFI_MQTT_PORTAL_HTML: &str = include_str!("assets/portal_wifi_mqtt.html");

/// The LoRaWAN profile portal HTML template.
///
/// Includes fields for Wi-Fi credentials, LoRaWAN DevEUI / JoinEUI /
/// AppKey (password input — never pre-filled), OTA URL, and device name.
/// No `{{APP_KEY}}` placeholder exists — the AppKey is never pre-filled.
pub const LORAWAN_PORTAL_HTML: &str = include_str!("assets/portal_lorawan.html");

#[cfg(test)]
mod tests {
    use super::*;

    /// The Wi-Fi + MQTT template must contain the CSRF nonce placeholder and
    /// the SSID pre-fill placeholder so the portal can be secured and
    /// pre-populated.
    #[test]
    fn wifi_mqtt_template_contains_required_placeholders() {
        assert!(
            WIFI_MQTT_PORTAL_HTML.contains("{{NONCE}}"),
            "wifi_mqtt template must contain {{NONCE}} placeholder"
        );
        assert!(
            WIFI_MQTT_PORTAL_HTML.contains("{{WIFI_SSID}}"),
            "wifi_mqtt template must contain {{WIFI_SSID}} placeholder"
        );
        assert!(
            WIFI_MQTT_PORTAL_HTML.contains("{{FW_VER}}"),
            "wifi_mqtt template must contain {{FW_VER}} placeholder"
        );
    }

    /// The Wi-Fi + MQTT template must NEVER contain secret-value placeholders.
    /// Passwords and other secrets must never be pre-filled into HTML — they
    /// are re-entered on every submission.
    #[test]
    fn wifi_mqtt_template_carries_no_password_placeholder() {
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains("{{WIFI_PASS}}"),
            "wifi_mqtt template must not contain {{WIFI_PASS}} placeholder"
        );
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains("{{MQTT_PASS}}"),
            "wifi_mqtt template must not contain {{MQTT_PASS}} placeholder"
        );
        // Also verify the rendered HTML does not accidentally pre-fill the
        // password input via a value= attribute on the password field.
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains("name=\"wifi_pass\" value="),
            "wifi_pass input must not carry a value= attribute"
        );
    }

    /// The LoRaWAN template must contain the CSRF nonce and LoRaWAN-specific
    /// field placeholders so the portal can be secured and pre-populated.
    #[test]
    fn lorawan_template_contains_required_placeholders() {
        assert!(
            LORAWAN_PORTAL_HTML.contains("{{NONCE}}"),
            "lorawan template must contain {{NONCE}} placeholder"
        );
        assert!(
            LORAWAN_PORTAL_HTML.contains("{{DEV_EUI}}"),
            "lorawan template must contain {{DEV_EUI}} placeholder"
        );
        assert!(
            LORAWAN_PORTAL_HTML.contains("{{JOIN_EUI}}"),
            "lorawan template must contain {{JOIN_EUI}} placeholder"
        );
        assert!(
            LORAWAN_PORTAL_HTML.contains("{{FW_VER}}"),
            "lorawan template must contain {{FW_VER}} placeholder"
        );
    }
}
