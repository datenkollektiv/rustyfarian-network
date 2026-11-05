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
//!
//! The module also holds the fixed response bodies and the OS probe paths both
//! tiers serve, so the two portals cannot drift apart in wording or routing.

use super::SchemaProfile;

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

/// Experimental: API may change before 1.0.
///
/// Hint appended to every error block rendered after a rejected or failed
/// `POST /save`.
///
/// Such re-renders show the stored / default values, never the rejected
/// submission (so no secret is ever echoed); this tells the user why their
/// input is gone. Plain text with no HTML-special characters, so tiers can
/// embed it without escaping.
pub const RESUBMIT_HINT: &str =
    "The form shows the saved or default values, not your submission. Re-enter your changes and any passwords.";

/// Experimental: API may change before 1.0.
///
/// The portal template for `profile`.
///
/// A tier that serves only some profiles rejects the others before the portal
/// starts (see [`plan_portal`](super::plan_portal)); this selection itself is
/// total.
pub fn template_for(profile: SchemaProfile) -> &'static str {
    match profile {
        SchemaProfile::LorawanFieldDevice => LORAWAN_PORTAL_HTML,
        SchemaProfile::WifiMqttDevice => WIFI_MQTT_PORTAL_HTML,
    }
}

/// Experimental: API may change before 1.0.
///
/// Well-known paths probed by phone and desktop OSes to detect a captive
/// portal.
///
/// The portal answers each with a `302` redirect to its root; the OS gets
/// unexpected content and opens the captive-portal browser. The list is
/// illustrative and expected to evolve as OS behaviour changes; it is not a
/// frozen contract.
pub const PROBE_PATHS: [&str; 6] = [
    "/generate_204",
    "/gen_204",
    "/hotspot-detect.html",
    "/ncsi.txt",
    "/connecttest.txt",
    "/canonical.html",
];

/// Experimental: API may change before 1.0.
///
/// Page shown after a successful commit.
pub const COMMITTED_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Provisioned</title></head><body style=\"font-family:system-ui,sans-serif;padding:1rem\">\
<h1>Provisioned</h1>\
<p>Credentials saved. The device will restart and join your network.</p>\
</body></html>";

/// Experimental: API may change before 1.0.
///
/// Page shown after a factory-reset request.
pub const FACTORY_RESET_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Factory reset</title></head><body style=\"font-family:system-ui,sans-serif;padding:1rem\">\
<h1>Factory reset requested</h1>\
<p>The host application will complete the reset.</p>\
</body></html>";

/// Experimental: API may change before 1.0.
///
/// `409` body for a `/save` or `/factory-reset` that arrives after the session
/// already reached a terminal state (or while a submission is being saved):
/// the first terminal event stands and nothing was changed.
pub const SESSION_FINISHED_BODY: &str =
    "Conflict: this provisioning session has already finished or is saving \
     another submission; nothing was changed. Wait for the device to restart.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_for_selects_each_profile() {
        assert_eq!(
            template_for(SchemaProfile::WifiMqttDevice),
            WIFI_MQTT_PORTAL_HTML
        );
        assert_eq!(
            template_for(SchemaProfile::LorawanFieldDevice),
            LORAWAN_PORTAL_HTML
        );
    }

    #[test]
    fn probe_paths_contains_the_known_android_apple_windows_paths() {
        for path in [
            "/generate_204",
            "/hotspot-detect.html",
            "/ncsi.txt",
            "/connecttest.txt",
        ] {
            assert!(PROBE_PATHS.contains(&path), "{path} missing");
        }
        assert!(PROBE_PATHS.iter().all(|p| p.starts_with('/')));
    }

    #[test]
    fn response_bodies_need_no_escaping_and_are_complete() {
        assert!(COMMITTED_HTML.ends_with("</body></html>"));
        assert!(FACTORY_RESET_HTML.ends_with("</body></html>"));
        assert!(SESSION_FINISHED_BODY.starts_with("Conflict:"));
        assert!(!SESSION_FINISHED_BODY.contains(['<', '>', '&']));
    }

    /// Tiers embed the hint verbatim inside HTML, so it must need no escaping.
    #[test]
    fn resubmit_hint_has_no_html_special_chars() {
        assert!(!RESUBMIT_HINT.is_empty());
        assert!(!RESUBMIT_HINT.contains(['<', '>', '&', '"', '\'']));
    }

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
