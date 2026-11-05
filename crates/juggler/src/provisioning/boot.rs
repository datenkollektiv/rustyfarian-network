//! Flat boot-time decisions of a `WifiMqttDevice` firmware (ADR 023 rung
//! "flat logic").
//!
//! - [`classify_wifi_mqtt`]: is the loaded record usable for a Wi-Fi + MQTT
//!   boot, and with which client ID;
//! - [`portal_outcome`]: what a finished portal session means for the caller,
//!   including the policy for a failed portal shutdown.
//!
//! The tier keeps the store I/O, the portal itself and its error type.

use core::fmt;

use super::{ProvisioningConfig, SchemaProfile};

/// Experimental: API may change before 1.0.
///
/// The boot classification of a loaded provisioning record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiMqttClass<C> {
    /// No record is stored.
    NotProvisioned,
    /// A record is stored but carries no MQTT group (another profile).
    OtherProfile(SchemaProfile),
    /// A `WifiMqttDevice` record with a valid MQTT client ID.
    Ready(C),
}

/// Experimental: API may change before 1.0.
///
/// Why [`classify_wifi_mqtt`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyError<E> {
    /// The caller's client-ID policy failed.
    ClientId(E),
    /// The client ID the policy returned is not a valid MQTT client ID.
    InvalidClientId(&'static str),
}

impl<E: fmt::Display> fmt::Display for ClassifyError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassifyError::ClientId(e) => write!(f, "client-id resolution failed: {e}"),
            ClassifyError::InvalidClientId(reason) => {
                write!(f, "client-id validation failed: {reason}")
            }
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Classifies a loaded record for a Wi-Fi + MQTT boot.
///
/// `None` is [`WifiMqttClass::NotProvisioned`]; a record without an MQTT
/// group is [`WifiMqttClass::OtherProfile`] (not an error); otherwise
/// `client_id` derives the client ID, which is then validated with
/// [`validate_client_id`](crate::mqtt::validate_client_id) so an invalid ID
/// fails here instead of at the broker.
pub fn classify_wifi_mqtt<'a, C: AsRef<str>, E>(
    stored: Option<&'a ProvisioningConfig>,
    client_id: impl FnOnce(&'a ProvisioningConfig) -> Result<C, E>,
) -> Result<WifiMqttClass<C>, ClassifyError<E>> {
    let Some(cfg) = stored else {
        return Ok(WifiMqttClass::NotProvisioned);
    };
    if cfg.mqtt().is_none() {
        return Ok(WifiMqttClass::OtherProfile(cfg.profile()));
    }
    let id = client_id(cfg).map_err(ClassifyError::ClientId)?;
    crate::mqtt::validate_client_id(id.as_ref()).map_err(ClassifyError::InvalidClientId)?;
    Ok(WifiMqttClass::Ready(id))
}

/// Experimental: API may change before 1.0.
///
/// The outcome of a finished provisioning portal run.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalOutcome {
    /// The portal received a valid submission; the device is now provisioned.
    ///
    /// The session has been shut down.  The caller should restart into the
    /// normal boot path.
    JustProvisioned,
    /// The portal's factory-reset button was pressed.
    ///
    /// The caller should erase the provisioning store and restart.
    FactoryResetRequested,
    /// The portal timed out without a committed submission.
    ///
    /// The caller decides whether to restart, retry, or take another action.
    PortalExitedWithoutCommit,
}

/// Experimental: API may change before 1.0.
///
/// Maps a finished session and the result of its shutdown to the caller's
/// outcome.
///
/// Commit durability: after a commit a shutdown error is logged at `warn` and
/// swallowed, because the credentials are stored and the caller restarts
/// anyway. On every other outcome a shutdown error is returned.
#[cfg(feature = "std")]
pub fn portal_outcome<E: fmt::Display>(
    outcome: super::SessionOutcome,
    shutdown: Result<(), E>,
) -> Result<PortalOutcome, E> {
    use super::SessionOutcome;
    match outcome {
        SessionOutcome::Committed => {
            if let Err(e) = shutdown {
                log::warn!(
                    "[boot] commit_shutdown_degraded: portal shutdown after commit errored: {e}"
                );
            }
            Ok(PortalOutcome::JustProvisioned)
        }
        SessionOutcome::FactoryResetRequested => {
            shutdown.map(|()| PortalOutcome::FactoryResetRequested)
        }
        SessionOutcome::TimedOut => shutdown.map(|()| PortalOutcome::PortalExitedWithoutCommit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::{parse_form, MqttFields};
    use crate::test_support::test_psk;
    use alloc::format;

    fn wifi_mqtt_record(client: Option<&str>) -> ProvisioningConfig {
        let mqtt = MqttFields::from_storage_parts(
            "broker.local".try_into().unwrap(),
            1883,
            None,
            None,
            client.map(|c| c.try_into().unwrap()),
        );
        ProvisioningConfig::from_storage_parts(
            "home".try_into().unwrap(),
            test_psk().try_into().unwrap(),
            "".try_into().unwrap(),
            "dev-01".try_into().unwrap(),
            None,
            Some(mqtt),
            Default::default(),
        )
    }

    fn device_name_policy(cfg: &ProvisioningConfig) -> Result<&str, &'static str> {
        Ok(cfg.device_name())
    }

    #[test]
    fn none_is_not_provisioned() {
        assert_eq!(
            classify_wifi_mqtt(None, device_name_policy),
            Ok(WifiMqttClass::NotProvisioned)
        );
    }

    #[test]
    fn record_without_mqtt_is_other_profile() {
        let body = format!(
            "wifi_ssid=home&wifi_pass={}&dev_eui=0011223344556677\
             &join_eui=70B3D57ED005ABCD&app_key=00112233445566778899AABBCCDDEEFF\
             &ota_url=http://example.com/fw.bin&dev_name=hive",
            test_psk()
        );
        let cfg = parse_form(&body, SchemaProfile::LorawanFieldDevice).expect("valid body");
        let mut called = false;
        let class = classify_wifi_mqtt(Some(&cfg), |c| {
            called = true;
            device_name_policy(c)
        });
        assert_eq!(
            class,
            Ok(WifiMqttClass::OtherProfile(
                SchemaProfile::LorawanFieldDevice
            ))
        );
        assert!(!called, "the client-ID policy runs only for an MQTT record");
    }

    #[test]
    fn client_id_error_propagates() {
        let cfg = wifi_mqtt_record(None);
        assert_eq!(
            classify_wifi_mqtt::<&str, _>(Some(&cfg), |_| Err("no id")),
            Err(ClassifyError::ClientId("no id"))
        );
    }

    #[test]
    fn invalid_client_id_is_rejected() {
        let cfg = wifi_mqtt_record(None);
        assert!(matches!(
            classify_wifi_mqtt::<_, ()>(Some(&cfg), |_| Ok("")),
            Err(ClassifyError::InvalidClientId(_))
        ));
    }

    #[test]
    fn valid_record_classifies() {
        let cfg = wifi_mqtt_record(Some("c3-01"));
        let class = classify_wifi_mqtt(Some(&cfg), |c| {
            Ok::<_, ()>(c.mqtt().and_then(|m| m.client_id()).unwrap_or("x"))
        });
        assert_eq!(class, Ok(WifiMqttClass::Ready("c3-01")));
    }

    #[cfg(feature = "std")]
    #[test]
    fn shutdown_error_swallowed_on_commit() {
        use crate::provisioning::SessionOutcome;
        assert_eq!(
            portal_outcome(SessionOutcome::Committed, Err("httpd")),
            Ok(PortalOutcome::JustProvisioned)
        );
        assert_eq!(
            portal_outcome::<&str>(SessionOutcome::Committed, Ok(())),
            Ok(PortalOutcome::JustProvisioned)
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn shutdown_error_propagated_on_cancel() {
        use crate::provisioning::SessionOutcome;
        for outcome in [
            SessionOutcome::FactoryResetRequested,
            SessionOutcome::TimedOut,
        ] {
            assert_eq!(portal_outcome(outcome, Err("httpd")), Err("httpd"));
        }
        assert_eq!(
            portal_outcome::<&str>(SessionOutcome::FactoryResetRequested, Ok(())),
            Ok(PortalOutcome::FactoryResetRequested)
        );
        assert_eq!(
            portal_outcome::<&str>(SessionOutcome::TimedOut, Ok(())),
            Ok(PortalOutcome::PortalExitedWithoutCommit)
        );
    }
}
