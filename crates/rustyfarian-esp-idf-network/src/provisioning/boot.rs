//! Boot helper: load a provisioned [`WifiMqttBoot`] bundle or run the portal.
//!
//! Requires features `provisioning` + `mqtt`.
//!
//! This module owns the two top-level operations a `WifiMqttDevice` firmware
//! performs at startup:
//!
//! 1. [`WifiMqttBoot::load`] — read the NVS store (modem-free) and return
//!    ready-to-borrow [`WiFiConfig`] / [`MqttConfig`] when provisioned.
//!    [`WifiMqttBoot::load_with_reentry`] first asks the consumer's re-entry
//!    hook, before the store is read, and reports a portal or factory-reset
//!    request as an outcome.
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
    use juggler::provisioning::{gate_reentry, ReentryGate, ReentryRequest};
    use juggler::provisioning::{ProvisioningConfig, SchemaProfile};

    use crate::mqtt::MqttConfig;
    use crate::provisioning::store::ProvisioningStore;
    use crate::provisioning::{PortalConfig, ProvisioningBuilder, ProvisioningEvent, SessionWait};
    use crate::wifi::WiFiConfig;

    /// Experimental: API may change before 1.0.
    ///
    /// The provisioned `WifiMqttDevice` record read from the NVS provisioning
    /// store, plus the resolved MQTT client ID.
    ///
    /// Construct via [`WifiMqttBoot::load`] or [`WifiMqttBoot::load_with`];
    /// borrow configs via [`wifi_config`](Self::wifi_config) and
    /// [`mqtt_config`](Self::mqtt_config).  The borrowed configs are valid for
    /// as long as this struct is alive — no `Box::leak`, no `'static`
    /// gymnastics required in the consumer.  The device identity is available
    /// without a second store read via [`device_name`](Self::device_name),
    /// [`ota_url`](Self::ota_url), and [`config`](Self::config) (extras and
    /// every other stored field).
    ///
    /// The stored record is a [`ProvisioningConfig`] moved out of the store,
    /// never cloned, and heap-allocated exactly once (in `load_with`), so the
    /// roughly 1.3 KB record is boxed for its lifetime; one by-value copy still
    /// crosses the boot task's stack during `load` and the `Box::new` (bug 002).
    /// Its secrets are scrubbed on drop through the `Box`.
    ///
    /// # Debug output
    ///
    /// The `Debug` impl prints `config` through [`ProvisioningConfig`]'s
    /// redacting `Debug`: `wifi_password`, `mqtt_pass`, and every extra value
    /// show as `"<redacted>"` with no length; the MQTT username, host, port,
    /// SSID, device name, and OTA URL are shown as-is.  `mqtt_client_id` is
    /// shown as-is.
    pub struct WifiMqttBoot {
        config: Box<ProvisioningConfig>,
        mqtt_client_id: String,
    }

    impl std::fmt::Debug for WifiMqttBoot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WifiMqttBoot")
                .field("config", &self.config)
                .field("mqtt_client_id", &self.mqtt_client_id)
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
            Self::load_with(nvs, Self::default_client_id)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The built-in client-ID policy used by [`load`](Self::load), for
        /// callers of [`load_with_reentry`](Self::load_with_reentry) that want
        /// the same policy.
        ///
        /// Resolves via [`juggler::mqtt::resolve_client_id`] with the
        /// operator-supplied `mqtt_client`, the stored `device_name`, and the
        /// hard-coded fallback `"rustyfarian"`.
        ///
        /// # Errors
        ///
        /// Returns `Err` when no valid client-ID can be derived.
        pub fn default_client_id(cfg: &ProvisioningConfig) -> anyhow::Result<String> {
            let operator_id = cfg.mqtt().and_then(|m| m.client_id());
            let chosen = resolve_client_id(operator_id, cfg.device_name(), "rustyfarian")
                .map_err(|e| anyhow::anyhow!("client-id resolution failed: {}", e))?;
            Ok(chosen.to_owned())
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Like [`load`](Self::load) but uses `client_id_fn` to derive the MQTT
        /// client ID instead of the built-in policy.
        ///
        /// `client_id_fn` receives the full [`ProvisioningConfig`] (for access
        /// to any field, including extras) and returns an owned `String`.  The returned
        /// ID is validated via [`juggler::mqtt::validate_client_id`]; an invalid
        /// ID is surfaced as `Err`.
        ///
        /// # Example
        ///
        /// ```ignore
        /// WifiMqttBoot::load_with(nvs, |cfg| {
        ///     Ok(format!("my-{}", cfg.device_name().chars().take(8).collect::<String>()))
        /// })?;
        /// ```
        pub fn load_with(
            nvs: EspDefaultNvsPartition,
            client_id_fn: impl FnOnce(&ProvisioningConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            let store = ProvisioningStore::open(nvs)?;
            Self::classify(&store, client_id_fn)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Like [`load_with`](Self::load_with), but asks `reentry` first,
        /// exactly once, before the store is opened.
        ///
        /// - [`ReentryRequest::None`] reproduces [`load_with`](Self::load_with).
        /// - [`ReentryRequest::Portal`] returns
        ///   [`WifiMqttLoadOutcome::PortalRequested`]; the store is neither
        ///   opened nor read.
        /// - [`ReentryRequest::FactoryReset`] returns
        ///   [`WifiMqttLoadOutcome::FactoryResetRequested`]; the store is
        ///   neither opened nor read.
        ///
        /// The library never erases and never restarts, and this call never
        /// claims the modem; the consumer acts on the outcome.
        ///
        /// # Errors
        ///
        /// As [`load_with`](Self::load_with).  The hook itself cannot fail.
        pub fn load_with_reentry(
            nvs: EspDefaultNvsPartition,
            reentry: impl FnOnce() -> ReentryRequest,
            client_id_fn: impl FnOnce(&ProvisioningConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            match gate_reentry(reentry, || ProvisioningStore::open(nvs))? {
                ReentryGate::Portal => Ok(WifiMqttLoadOutcome::PortalRequested),
                ReentryGate::FactoryReset => Ok(WifiMqttLoadOutcome::FactoryResetRequested),
                ReentryGate::Proceed(store) => Self::classify(&store, client_id_fn),
            }
        }

        fn classify(
            store: &ProvisioningStore,
            client_id_fn: impl FnOnce(&ProvisioningConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            let cfg = match store.load()? {
                None => return Ok(WifiMqttLoadOutcome::NotProvisioned),
                Some(c) => c,
            };

            if cfg.mqtt().is_none() {
                return Ok(WifiMqttLoadOutcome::OtherProfile(cfg.profile()));
            }

            let client_id = client_id_fn(&cfg)?;
            // Validate the override's output — reject early rather than letting
            // MqttConfig or the broker surface an opaque error later.
            juggler::mqtt::validate_client_id(&client_id)
                .map_err(|e| anyhow::anyhow!("client-id validation failed: {}", e))?;

            // `cfg` is moved, never cloned, and boxed exactly once here so the
            // ~1.3 KB record leaves the stack (bug 002).
            Ok(WifiMqttLoadOutcome::Ready(WifiMqttBoot {
                config: Box::new(cfg),
                mqtt_client_id: client_id,
            }))
        }

        /// Test-only constructor that bypasses NVS.
        #[cfg(test)]
        pub(super) fn from_parts_for_test(
            config: ProvisioningConfig,
            mqtt_client_id: String,
        ) -> Self {
            Self {
                config: Box::new(config),
                mqtt_client_id,
            }
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The stored device name.
        pub fn device_name(&self) -> &str {
            self.config.device_name()
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The stored OTA update URL, or `None` when no OTA URL is configured.
        pub fn ota_url(&self) -> Option<&str> {
            self.config.ota_url()
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The full stored record, for extras and any field without a
        /// dedicated accessor here.
        pub fn config(&self) -> &ProvisioningConfig {
            &self.config
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Returns a borrowed [`WiFiConfig`] backed by strings owned by this struct.
        pub fn wifi_config(&self) -> WiFiConfig<'_> {
            WiFiConfig::new(self.config.wifi_ssid(), self.config.wifi_password())
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
            // `load_with` only constructs a `WifiMqttBoot` from a record whose
            // MQTT group is present, so the `None` arm is unreachable.
            let Some(mqtt) = self.config.mqtt() else {
                debug_assert!(false, "WifiMqttBoot holds a record without an MQTT group");
                return MqttConfig::new("", 0, &self.mqtt_client_id);
            };
            let config = MqttConfig::new(mqtt.host(), mqtt.port(), &self.mqtt_client_id);
            // Every auth shape is matched explicitly — nothing is folded into a
            // catch-all — so a malformed `(None, Some(pass))` cannot silently become
            // anonymous. That shape is in fact unreachable: a password without a
            // username is rejected at the form/parse boundary by
            // `juggler::provisioning::parse_form` (test
            // `auth_password_without_user_rejected_on_mqtt_pass`), and MQTT 3.1.1
            // forbids a password field without a username.
            match (mqtt.username(), mqtt.password()) {
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
    /// The outcome of [`WifiMqttBoot::load`] / [`WifiMqttBoot::load_with`] /
    /// [`WifiMqttBoot::load_with_reentry`].
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
        /// The re-entry hook answered [`ReentryRequest::Portal`].
        ///
        /// The store was not read.  The consumer runs [`run_wifi_mqtt_portal`];
        /// the stored record is kept and the portal prefills from it.
        PortalRequested,
        /// The re-entry hook answered [`ReentryRequest::FactoryReset`].
        ///
        /// The store was not read and nothing was erased: the library never
        /// erases.  The consumer erases (via [`ProvisioningStore::erase_all`])
        /// and then decides whether to run the portal or restart.
        FactoryResetRequested,
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
    /// # Stack budget
    ///
    /// This function blocks the calling task for the portal's whole lifetime,
    /// but the wait itself is cheap: it resolves to a [`PortalOutcome`] enum,
    /// and the committed [`ProvisioningConfig`](crate::provisioning::ProvisioningConfig)
    /// is never copied onto the caller's stack — the portal's HTTP handlers run
    /// on their own `httpd` tasks. The workspace's own examples run with
    /// `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` (`sdkconfig.defaults`). Before
    /// this contract was fixed, a 0.5.0 consumer with an 8000-byte main task
    /// overflowed its stack right after logging `Provisioning event: Committed`
    /// (see `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md`).
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
            SessionWait::Committed => {
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

#[cfg(all(test, feature = "provisioning", feature = "mqtt"))]
mod tests {
    use super::inner::WifiMqttBoot;
    use crate::provisioning::portal::tests::stored_wifi_mqtt;

    #[test]
    fn debug_redacts_secrets_without_length() {
        let config = stored_wifi_mqtt("dev-01");
        let wifi_password = config.wifi_password().to_owned();
        let mqtt_pass = config
            .mqtt()
            .and_then(|m| m.password())
            .unwrap_or_default()
            .to_owned();
        let boot = WifiMqttBoot::from_parts_for_test(config, "dev-01".to_owned());

        let debug = format!("{boot:?}");
        assert!(!debug.contains(&wifi_password));
        assert!(!debug.contains(&mqtt_pass));
        assert!(!debug.contains("len="));
        assert!(debug.contains("dev-01"));
        assert_eq!(boot.device_name(), "dev-01");
        assert_eq!(boot.ota_url(), None);
    }
}

// Re-export all public items at the module level under the same cfg guard so
// callers see them directly under `crate::provisioning::boot::*`.
#[cfg(all(feature = "provisioning", feature = "mqtt"))]
pub use inner::{
    run_wifi_mqtt_portal, BootConfig, PortalOutcome, WifiMqttBoot, WifiMqttLoadOutcome,
};
