//! Boot helper: load a provisioned [`WifiMqttBoot`] bundle or run the portal.
//!
//! Requires features `provisioning` + `mqtt`.
//!
//! This module owns the two top-level operations a `WifiMqttDevice` firmware
//! performs at startup:
//!
//! 1. [`WifiMqttBoot::load`] — read the NVS store (modem-free) and return
//!    ready-to-borrow [`WiFiConfig`] / [`MqttConfig`] when provisioned.
//! 2. [`run_wifi_mqtt_portal`] — start the SoftAP captive portal (consumes
//!    the modem) and return a [`PortalOutcome`] when it terminates.
//!
//! The two calls are deliberately split so the modem is only claimed in the
//! path that needs it.  The library never calls `restart()` or `erase()`; those
//! remain caller decisions.
//!
//! All items in this module are gated:
//! `#[cfg(all(feature = "provisioning", feature = "mqtt"))]`

#[cfg(all(feature = "provisioning", feature = "mqtt"))]
mod inner {
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context as _;

    use esp_idf_svc::eventloop::EspSystemEventLoop;
    use esp_idf_svc::hal::modem::Modem;
    use esp_idf_svc::nvs::EspDefaultNvsPartition;

    use juggler::mqtt::resolve_client_id;
    use juggler::provisioning::SchemaProfile;

    use crate::mqtt::MqttConfig;
    use crate::provisioning::store::{ProvisioningStore, StoredConfig};
    use crate::provisioning::{PortalConfig, ProvisioningBuilder, ProvisioningEvent, SessionWait};
    use crate::wifi::WiFiConfig;

    /// Experimental: API may change before 1.0.
    ///
    /// An owned bundle of resolved Wi-Fi and MQTT configuration strings read
    /// from the NVS provisioning store.
    ///
    /// Construct via [`WifiMqttBoot::load`] or [`WifiMqttBoot::load_with`];
    /// borrow configs via [`wifi_config`](Self::wifi_config) and
    /// [`mqtt_config`](Self::mqtt_config).  The borrowed configs are valid for
    /// as long as this struct is alive — no `Box::leak`, no `'static`
    /// gymnastics required in the consumer.
    ///
    /// # Debug output
    ///
    /// The `Debug` impl redacts credential fields (`wifi_password`, `mqtt_pass`,
    /// `mqtt_user`) by length only, e.g. `wifi_password: "<redacted, len=8>"`.
    /// Non-secret fields (`wifi_ssid`, `mqtt_host`, `mqtt_port`,
    /// `mqtt_client_id`) are shown as-is.
    pub struct WifiMqttBoot {
        // Wi-Fi
        wifi_ssid: String,
        wifi_password: String,
        // MQTT
        mqtt_host: String,
        mqtt_port: u16,
        mqtt_client_id: String,
        mqtt_user: Option<String>,
        mqtt_pass: Option<String>,
    }

    impl std::fmt::Debug for WifiMqttBoot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WifiMqttBoot")
                .field("wifi_ssid", &self.wifi_ssid)
                .field(
                    "wifi_password",
                    &format_args!("<redacted, len={}>", self.wifi_password.len()),
                )
                .field("mqtt_host", &self.mqtt_host)
                .field("mqtt_port", &self.mqtt_port)
                .field("mqtt_client_id", &self.mqtt_client_id)
                .field(
                    "mqtt_user",
                    &self
                        .mqtt_user
                        .as_deref()
                        .map(|u| format!("<redacted, len={}>", u.len())),
                )
                .field(
                    "mqtt_pass",
                    &self
                        .mqtt_pass
                        .as_deref()
                        .map(|p| format!("<redacted, len={}>", p.len())),
                )
                .finish()
        }
    }

    impl WifiMqttBoot {
        /// Experimental: API may change before 1.0.
        ///
        /// Loads a provisioned `WifiMqttDevice` record from NVS using the
        /// built-in client-ID policy.
        ///
        /// The client-ID is resolved by [`juggler::mqtt::resolve_client_id`]
        /// with the operator-supplied `mqtt_client`, the stored `device_name`,
        /// and the hard-coded fallback `"rustyfarian"`.
        ///
        /// # Errors
        ///
        /// Returns `Err` on NVS I/O errors or an invalid client-ID derived
        /// from the stored data.  A record under a non-`WifiMqttDevice` profile
        /// is **not** an error; it yields
        /// [`WifiMqttLoadOutcome::OtherProfile`].
        pub fn load(nvs: EspDefaultNvsPartition) -> anyhow::Result<WifiMqttLoadOutcome> {
            Self::load_with(nvs, |cfg| {
                let chosen =
                    resolve_client_id(cfg.mqtt_client.as_deref(), &cfg.device_name, "rustyfarian")
                        .map_err(|e| anyhow::anyhow!("client-id resolution failed: {}", e))?;
                Ok(chosen.to_owned())
            })
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Like [`load`](Self::load) but uses `client_id_fn` to derive the MQTT
        /// client ID instead of the built-in policy.
        ///
        /// `client_id_fn` receives the full [`StoredConfig`] (for access to any
        /// field, including extras) and returns an owned `String`.  The returned
        /// ID is validated via [`juggler::mqtt::validate_client_id`]; an invalid
        /// ID is surfaced as `Err`.
        ///
        /// # Example
        ///
        /// ```ignore
        /// WifiMqttBoot::load_with(nvs, |cfg| {
        ///     Ok(format!("my-{}", cfg.device_name.chars().take(8).collect::<String>()))
        /// })?;
        /// ```
        pub fn load_with(
            nvs: EspDefaultNvsPartition,
            client_id_fn: impl FnOnce(&StoredConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            let store = ProvisioningStore::open(nvs)?;
            let cfg = match store.load()? {
                None => return Ok(WifiMqttLoadOutcome::NotProvisioned),
                Some(c) => c,
            };

            if cfg.profile != SchemaProfile::WifiMqttDevice {
                return Ok(WifiMqttLoadOutcome::OtherProfile(cfg.profile));
            }

            let client_id = client_id_fn(&cfg)?;
            // Validate the override's output — reject early rather than letting
            // MqttConfig or the broker surface an opaque error later.
            juggler::mqtt::validate_client_id(&client_id)
                .map_err(|e| anyhow::anyhow!("client-id validation failed: {}", e))?;

            Ok(WifiMqttLoadOutcome::Ready(WifiMqttBoot {
                wifi_ssid: cfg.wifi_ssid,
                wifi_password: cfg.wifi_password,
                mqtt_host: cfg.mqtt_host,
                mqtt_port: cfg.mqtt_port,
                mqtt_client_id: client_id,
                mqtt_user: cfg.mqtt_user,
                mqtt_pass: cfg.mqtt_pass,
            }))
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Returns a borrowed [`WiFiConfig`] backed by strings owned by this struct.
        pub fn wifi_config(&self) -> WiFiConfig<'_> {
            WiFiConfig::new(&self.wifi_ssid, &self.wifi_password)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Returns a borrowed [`MqttConfig`] backed by strings owned by this struct.
        ///
        /// Auth mapping:
        /// - `(user, pass)` present → [`with_auth`](MqttConfig::with_auth)
        /// - `user` only → [`with_username_only`](MqttConfig::with_username_only)
        /// - neither → anonymous
        pub fn mqtt_config(&self) -> MqttConfig<'_> {
            let config = MqttConfig::new(&self.mqtt_host, self.mqtt_port, &self.mqtt_client_id);
            // Every auth shape is matched explicitly — nothing is folded into a
            // catch-all — so a malformed `(None, Some(pass))` cannot silently become
            // anonymous. That shape is in fact unreachable: a password without a
            // username is rejected at the form/parse boundary by
            // `juggler::provisioning::parse_form` (test
            // `auth_password_without_user_rejected_on_mqtt_pass`), and MQTT 3.1.1
            // forbids a password field without a username.
            match (self.mqtt_user.as_deref(), self.mqtt_pass.as_deref()) {
                (Some(user), Some(pass)) => config.with_auth(user, pass),
                (Some(user), None) => config.with_username_only(user),
                (None, None) => config,
                (None, Some(_)) => {
                    // Unreachable per the invariant above. Do not emit a malformed
                    // CONNECT (password without username); fall back to anonymous and
                    // trip in debug so a parser regression is caught by tests.
                    debug_assert!(
                        false,
                        "mqtt_pass without mqtt_user reached mqtt_config — parse_form should reject this"
                    );
                    config
                }
            }
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The outcome of [`WifiMqttBoot::load`] / [`WifiMqttBoot::load_with`].
    ///
    /// `Debug` output for the `Ready` variant redacts credential fields via
    /// [`WifiMqttBoot`]'s manual `Debug` impl.
    #[non_exhaustive]
    #[derive(Debug)]
    pub enum WifiMqttLoadOutcome {
        /// A provisioned `WifiMqttDevice` record was found; the bundle is ready.
        Ready(WifiMqttBoot),
        /// No provisioned record was found in NVS.
        NotProvisioned,
        /// A record was found but was provisioned under a different profile.
        OtherProfile(SchemaProfile),
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The outcome of [`run_wifi_mqtt_portal`].
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
        /// The caller should erase the NVS provisioning namespace (via
        /// [`ProvisioningStore::erase_all`]) and restart.
        FactoryResetRequested,
        /// The portal timed out without a committed submission.
        ///
        /// The caller decides whether to restart, retry, or take another action.
        PortalExitedWithoutCommit,
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Configuration for [`run_wifi_mqtt_portal`].
    pub struct BootConfig<'a> {
        /// Static configuration for the captive portal AP and schema.
        pub portal: PortalConfig<'a>,
        /// Optional wall-clock timeout for the portal.  `None` blocks until a
        /// terminal event (commit or factory-reset).
        pub portal_timeout: Option<Duration>,
        /// Optional lifecycle-event callback, shared with the portal httpd task.
        ///
        /// The callback runs on the `httpd` task; it must return quickly and
        /// never block.  Requires `Send + Sync` because multiple HTTP handlers
        /// may invoke it concurrently.
        pub on_event: Option<Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static>>,
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Runs the SoftAP captive portal until a terminal event occurs.
    ///
    /// Starts the portal, waits for a commit, a factory-reset request, or the
    /// optional `config.portal_timeout` to elapse, then shuts the portal down
    /// and returns a [`PortalOutcome`].
    ///
    /// # Shutdown durability
    ///
    /// On a successful commit the session is shut down on a best-effort basis:
    /// a shutdown error is logged as a warning but `JustProvisioned` is still
    /// returned (the caller will restart the device anyway).  On non-commit
    /// exits (`FactoryResetRequested`, `PortalExitedWithoutCommit`) a shutdown
    /// error is propagated as `Err`.
    ///
    /// # Library contract
    ///
    /// This function never calls `restart()` or `erase()`.  Those decisions
    /// belong to the caller.
    ///
    /// # Errors
    ///
    /// Returns `Err` on operational failures: SoftAP startup, store open, DNS
    /// or httpd startup, or (on non-commit exits) a shutdown failure.
    pub fn run_wifi_mqtt_portal(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        config: BootConfig<'_>,
    ) -> anyhow::Result<PortalOutcome> {
        let mut builder = ProvisioningBuilder::new(config.portal);
        if let Some(on_event) = config.on_event {
            builder = builder.on_event(move |e| on_event(e));
        }
        let session = builder
            .start(modem, sys_loop, nvs)
            .context("failed to start provisioning portal")?;

        match session.wait_outcome(config.portal_timeout) {
            SessionWait::Committed(_config) => {
                // Commit-durability rule: return JustProvisioned even if shutdown
                // errors; the caller restarts anyway.
                if let Err(e) = session.shutdown() {
                    log::warn!("[boot] commit_shutdown_degraded: portal shutdown after commit errored: {e:#}");
                }
                Ok(PortalOutcome::JustProvisioned)
            }
            SessionWait::FactoryResetRequested => {
                session
                    .shutdown()
                    .context("portal shutdown after factory-reset failed")?;
                Ok(PortalOutcome::FactoryResetRequested)
            }
            SessionWait::TimedOut => {
                session
                    .shutdown()
                    .context("portal shutdown after timeout failed")?;
                Ok(PortalOutcome::PortalExitedWithoutCommit)
            }
        }
    }
}

// Re-export all public items at the module level under the same cfg guard so
// callers see them directly under `crate::provisioning::boot::*`.
#[cfg(all(feature = "provisioning", feature = "mqtt"))]
pub use inner::{
    run_wifi_mqtt_portal, BootConfig, PortalOutcome, WifiMqttBoot, WifiMqttLoadOutcome,
};
