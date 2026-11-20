//! The identity rule a provisioning portal runs under.
//!
//! [`IdentityRule::Open`] is the portal as it was: a free-form device name, MQTT username and client id, and opaque extras.
//! [`IdentityRule::Ddi`] applies the fleet DDI profile (§2) through the shared grammar in [`crate::identity`]:
//!
//! - the device name is a device id (`^[a-z0-9][a-z0-9-]{2,19}$`), the one string that is also the DDI controller id, the MQTT username and the MQTT client id;
//! - at submit, `mqtt_user` must equal the device name and `mqtt_client` must be blank or equal it, so the identity cannot drift between the three uses;
//! - the form is pre-filled with `mqtt_user` from the device name and `mqtt_client` blank, whatever the portal defaults say;
//! - a `DeviceName::MacSuffixed` prefix that is longer than 15 bytes or outside `[a-z0-9-]` is refused when the portal is planned, never truncated, and a `DeviceName::Fixed` name must itself be a device id;
//! - the reserved extra [`DDI_TOKEN_KEY`] (`ddi_token`, stored as `x_ddi_token`) is required and must be 32 to 64 lowercase hex characters, re-entered on every full submission like the passwords.
//!
//! `Ddi` applies only to the `WifiMqttDevice` profile, the one whose form carries the token input and whose device reaches a DDI server ([`IdentityRule::applies_to`]); a `LorawanFieldDevice` portal and the Wi-Fi-only fallback form run as under `Open`.
//!
//! Under either rule a submitted `ddi_token` is validated when present; only its presence is optional under `Open`.
//! The token is never pre-filled, never echoed into HTML, JSON or logs, and read back through
//! [`ProvisioningConfig::ddi_token`](crate::provisioning::ProvisioningConfig::ddi_token), which re-validates it.
//! Storage is the tier's provisioning store as for every extra (plaintext NVS or flash); a release image keeps it in encrypted storage through the device repo's partition settings.
//!
//! The ESP-IDF tier defaults to `Ddi` when its `ota-ddi` feature is on and to `Open` otherwise; the esp-hal tier defaults to `Open`.
//! There is no `Default`: a portal names its rule.

/// Experimental: API may change before 1.0.
///
/// The form key and, with the `x_` prefix, the NVS key of the DDI target token extra.
pub const DDI_TOKEN_KEY: &str = "ddi_token";

/// Experimental: API may change before 1.0.
///
/// Which identity rules the portal applies; see the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityRule {
    /// Free-form device name, MQTT username and client id; a `ddi_token` extra is validated only when present.
    Open,
    /// The fleet DDI profile's identity: device id grammar, one id for the three uses, a required `ddi_token`.
    Ddi,
}

impl IdentityRule {
    /// Experimental: API may change before 1.0.
    ///
    /// `true` for [`IdentityRule::Ddi`].
    pub const fn is_ddi(self) -> bool {
        matches!(self, IdentityRule::Ddi)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// `true` when the DDI rules bind a portal of `profile`: [`IdentityRule::Ddi`] on the `WifiMqttDevice` profile.
    /// A LoRaWAN field device has no HTTP path to a DDI server and its form no token input, so the rule never applies to it.
    pub const fn applies_to(self, profile: super::SchemaProfile) -> bool {
        self.is_ddi() && matches!(profile, super::SchemaProfile::WifiMqttDevice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_key_fits_an_extra_key_with_its_nvs_prefix() {
        assert!(DDI_TOKEN_KEY.len() <= crate::provisioning::EXTRA_KEY_MAX_LEN);
        assert!("x_".len() + DDI_TOKEN_KEY.len() <= 15);
    }

    #[test]
    fn only_ddi_is_ddi() {
        assert!(IdentityRule::Ddi.is_ddi());
        assert!(!IdentityRule::Open.is_ddi());
    }

    #[test]
    fn ddi_applies_to_the_wifi_mqtt_profile_only() {
        use crate::provisioning::SchemaProfile;
        assert!(IdentityRule::Ddi.applies_to(SchemaProfile::WifiMqttDevice));
        assert!(!IdentityRule::Ddi.applies_to(SchemaProfile::LorawanFieldDevice));
        assert!(!IdentityRule::Open.applies_to(SchemaProfile::WifiMqttDevice));
    }
}
