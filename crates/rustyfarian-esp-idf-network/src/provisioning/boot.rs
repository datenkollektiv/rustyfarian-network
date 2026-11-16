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
//! # Portal fallback (opt-in)
//!
//! [`WifiMqttBoot::load_with_fallback`] adds the portal fallback after repeated
//! failed Wi-Fi joins (`docs/features/wifi-portal-fallback-v1.md`):
//!
//! 1. The load reads the two failure counters from `rf_prov` and returns
//!    [`WifiMqttLoadOutcome::FallbackPortal`] once one reached its
//!    [`FallbackPolicy`] threshold; otherwise `Ready`, with a non-blocking
//!    [`wifi_config`](WifiMqttBoot::wifi_config).
//! 2. After `WiFiManager::new_without_led`, [`WifiMqttBoot::wait_for_join`]
//!    waits one join window, classifies the boot's failed attempts from their
//!    disconnect reasons, and updates the counters; it never fails, so a Wi-Fi
//!    timeout never ends the boot.
//!    The consumer either keeps running and calls it again until it returns
//!    `Joined` (the link worker retries; only this call clears the counters
//!    after a late join), or restarts so the next boot retries.
//! 3. On `FallbackPortal` the consumer runs [`run_wifi_mqtt_fallback_portal`]:
//!    a Wi-Fi-only form that keeps the stored record until a commit, clears the
//!    counters when it opens, and closes after its window so the stored config
//!    is retried.
//!
//! The counter path never erases and never restarts.
//!
//! The two calls are deliberately split so the modem is only claimed in the
//! path that needs it.  The library never calls `restart()` or `erase()`; those
//! remain caller decisions.
//!
//! All items in this module are gated:
//! `#[cfg(all(feature = "provisioning", feature = "mqtt"))]`

#[cfg(all(feature = "provisioning", feature = "mqtt"))]
mod inner {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Context as _;

    use esp_idf_svc::eventloop::EspSystemEventLoop;
    use esp_idf_svc::hal::modem::Modem;
    use esp_idf_svc::nvs::EspDefaultNvsPartition;

    use juggler::mqtt::resolve_client_id;
    use juggler::provisioning::{
        classify_wifi_mqtt, gate_reentry, portal_outcome, ClassifyError, FailureCounts,
        FallbackDecision, FallbackPolicy, FallbackReason, JoinRecorder, ReentryGate,
        ReentryRequest, WifiMqttClass, FALLBACK_PORTAL_WINDOW_DEFAULT,
    };
    use juggler::provisioning::{ProvisioningConfig, SchemaProfile};
    use juggler::wifi::JoinFailure;

    use crate::mqtt::MqttConfig;
    use crate::provisioning::store::ProvisioningStore;
    use crate::provisioning::{PortalConfig, ProvisioningBuilder, ProvisioningEvent};
    use crate::wifi::{WiFiConfig, WiFiManager, POLL_INTERVAL_MS};

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
    /// crosses the boot task's stack during `load` and the `Box::new`.
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
        /// Set by [`load_with_fallback`](Self::load_with_fallback) only.
        fallback: Option<FallbackJoin>,
    }

    /// The portal-fallback bookkeeping of a bundle loaded with
    /// [`WifiMqttBoot::load_with_fallback`].
    struct FallbackJoin {
        nvs: EspDefaultNvsPartition,
        recorder: Mutex<JoinRecorder>,
    }

    impl FallbackJoin {
        /// Persists `counts`, best-effort: a failed write is logged and the boot
        /// goes on, at worst one boot early or late to the fallback.
        fn persist(&self, counts: FailureCounts) {
            if let Err(e) = ProvisioningStore::open(self.nvs.clone())
                .and_then(|mut s| s.save_failure_counts(counts))
            {
                log::warn!("Portal fallback: failed to save the failure counters: {e:#}");
            }
        }
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

        /// Experimental: API may change before 1.0.
        ///
        /// Like [`load_with_reentry`](Self::load_with_reentry), with the portal
        /// fallback after repeated failed Wi-Fi joins (the opt-in for the
        /// `WifiMqttDevice` profile).
        ///
        /// The re-entry hook runs first, exactly once, before the store is
        /// opened, with the same outcomes; pass `|| ReentryRequest::None`
        /// without a hook.
        /// Then, for a `WifiMqttDevice` record only, the failure counters are
        /// read and decided with `policy`:
        ///
        /// - a counter at its threshold returns
        ///   [`WifiMqttLoadOutcome::FallbackPortal`]; the counters are kept
        ///   until [`run_wifi_mqtt_fallback_portal`] opens;
        /// - otherwise `Ready`, with a bundle whose
        ///   [`wifi_config`](Self::wifi_config) connects non-blocking and whose
        ///   [`wait_for_join`](Self::wait_for_join) updates the counters.
        ///
        /// `NotProvisioned` and `OtherProfile` are unchanged, and the counters
        /// are not read for them.
        /// A counter read error is logged and counts from zero, so it never
        /// ends the boot.
        /// The library never erases and never restarts.
        ///
        /// # Errors
        ///
        /// As [`load_with`](Self::load_with).
        pub fn load_with_fallback(
            nvs: EspDefaultNvsPartition,
            policy: FallbackPolicy,
            reentry: impl FnOnce() -> ReentryRequest,
            client_id_fn: impl FnOnce(&ProvisioningConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            let store = match gate_reentry(reentry, || ProvisioningStore::open(nvs.clone()))? {
                ReentryGate::Portal => return Ok(WifiMqttLoadOutcome::PortalRequested),
                ReentryGate::FactoryReset => return Ok(WifiMqttLoadOutcome::FactoryResetRequested),
                ReentryGate::Proceed(store) => store,
            };
            let mut boot = match Self::classify(&store, client_id_fn)? {
                WifiMqttLoadOutcome::Ready(boot) => boot,
                other => return Ok(other),
            };
            let counts = store.load_failure_counts().unwrap_or_else(|e| {
                log::warn!("Portal fallback: failed to read the failure counters, counting from zero: {e:#}");
                FailureCounts::ZERO
            });
            if let FallbackDecision::FallbackPortal { reason, failures } = policy.decide(counts) {
                log::warn!(
                    "Portal fallback: {failures} failed Wi-Fi boots ({}), fallback portal due",
                    reason.as_str()
                );
                return Ok(WifiMqttLoadOutcome::FallbackPortal { reason, failures });
            }
            boot.fallback = Some(FallbackJoin {
                nvs,
                recorder: Mutex::new(JoinRecorder::new(policy, counts)),
            });
            Ok(WifiMqttLoadOutcome::Ready(boot))
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Waits up to `window` for `wifi` to join (associated, with its IP)
        /// and, for a bundle from [`load_with_fallback`](Self::load_with_fallback),
        /// records the result in the failure counters.
        ///
        /// Call it right after `WiFiManager::new_without_led` with this
        /// bundle's non-blocking [`wifi_config`](Self::wifi_config).
        /// A join clears the counters.
        /// With no join, the boot's failed attempts are classified from the
        /// disconnect reasons the link worker saw before the first join
        /// ([`JoinFailure`]) and the matching counter grows by one; a boot
        /// counts at most once, however often this is called.
        ///
        /// It never fails and never ends the boot: a counter write error is
        /// logged.
        /// After `NotJoined` the link worker keeps retrying with its back-off.
        /// A consumer that keeps running must call this again, with any
        /// window, until it returns `Joined`: only this call clears the
        /// counters, so a station that joins later with no further call keeps
        /// its count, and enough such boots open the fallback portal on a
        /// healthy network.
        /// A consumer that restarts instead lets the next boot retry; each boot
        /// counts once, so its restart cadence sets how soon a counter reaches
        /// its threshold.
        /// At the threshold the consumer restarts, and the next load returns
        /// [`WifiMqttLoadOutcome::FallbackPortal`]: the station holds the modem,
        /// so the portal cannot open in this boot.
        ///
        /// A bundle from the other loads records nothing and always reports
        /// [`FallbackDecision::CarryOn`].
        ///
        /// Blocks the calling task, polling every
        /// [`POLL_INTERVAL_MS`](crate::wifi::POLL_INTERVAL_MS).
        /// A `WiFiManager` built with an LED waits blocking in its constructor
        /// and returns `Err` on a timeout, so use `new_without_led` here and
        /// drive the LED from the outcome.
        pub fn wait_for_join(&self, wifi: &WiFiManager, window: Duration) -> JoinOutcome {
            let start = Instant::now();
            let attempts = loop {
                let (joined, attempts) = wifi.first_join();
                if joined || wifi.is_ready() {
                    if let Some(fallback) = &self.fallback {
                        let write = fallback
                            .recorder
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .joined();
                        if let Some(counts) = write {
                            log::info!("Portal fallback: Wi-Fi joined, failure counters cleared");
                            fallback.persist(counts);
                        }
                    }
                    return JoinOutcome::Joined;
                }
                if start.elapsed() >= window {
                    break attempts;
                }
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            };

            let class = attempts.class();
            let decision = match &self.fallback {
                Some(fallback) => {
                    let mut recorder = fallback.recorder.lock().unwrap_or_else(|e| e.into_inner());
                    let (write, decision) = recorder.not_joined(class);
                    let counts = recorder.stored();
                    let policy = recorder.policy();
                    drop(recorder);
                    if let Some(counts) = write {
                        fallback.persist(counts);
                    }
                    log::warn!(
                        "Portal fallback: no Wi-Fi join within {} s, {} failed attempt(s), class {}; \
                         credentials {}/{}, network absent {}/{}",
                        window.as_secs(),
                        attempts.attempts(),
                        class.as_str(),
                        counts.credentials,
                        policy.credential_threshold(),
                        counts.network_absent,
                        policy.network_absent_threshold(),
                    );
                    decision
                }
                None => FallbackDecision::CarryOn,
            };
            JoinOutcome::NotJoined { class, decision }
        }

        fn classify(
            store: &ProvisioningStore,
            client_id_fn: impl FnOnce(&ProvisioningConfig) -> anyhow::Result<String>,
        ) -> anyhow::Result<WifiMqttLoadOutcome> {
            let stored = store.load()?;
            let class = classify_wifi_mqtt(stored.as_ref(), client_id_fn).map_err(|e| match e {
                ClassifyError::ClientId(e) => e,
                invalid @ ClassifyError::InvalidClientId(_) => {
                    anyhow::anyhow!("{invalid}")
                }
            })?;
            match (class, stored) {
                (WifiMqttClass::NotProvisioned, _) => Ok(WifiMqttLoadOutcome::NotProvisioned),
                (WifiMqttClass::OtherProfile(p), _) => Ok(WifiMqttLoadOutcome::OtherProfile(p)),
                // `cfg` is moved, never cloned, and boxed exactly once here so
                // the ~1.3 KB record leaves the stack.
                (WifiMqttClass::Ready(client_id), Some(cfg)) => {
                    Ok(WifiMqttLoadOutcome::Ready(WifiMqttBoot {
                        config: Box::new(cfg),
                        mqtt_client_id: client_id,
                        fallback: None,
                    }))
                }
                (WifiMqttClass::Ready(_), None) => {
                    anyhow::bail!("provisioning record classified ready but not loaded")
                }
            }
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
                fallback: None,
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
        ///
        /// For a bundle from [`load_with_fallback`](Self::load_with_fallback)
        /// it connects non-blocking, the profile default with the fallback, so
        /// [`wait_for_join`](Self::wait_for_join) sees every failed attempt;
        /// otherwise it keeps the [`WiFiConfig::new`] default.
        pub fn wifi_config(&self) -> WiFiConfig<'_> {
            let config = WiFiConfig::new(self.config.wifi_ssid(), self.config.wifi_password());
            if self.fallback.is_some() {
                config.connect_nonblocking()
            } else {
                config
            }
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
    /// [`WifiMqttBoot::load_with_reentry`] / [`WifiMqttBoot::load_with_fallback`].
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
        /// A failure counter reached its [`FallbackPolicy`] threshold
        /// ([`WifiMqttBoot::load_with_fallback`] only).
        ///
        /// The stored record is kept and nothing was erased.  The consumer runs
        /// [`run_wifi_mqtt_fallback_portal`], which clears the counters when it
        /// opens, then restarts; or it carries on.
        ///
        /// A consumer may ignore this outcome, for example while an OTA image
        /// is pending verification (the library cannot know the OTA state):
        /// loading again with a policy whose thresholds are both `0` returns
        /// `Ready` with the counters still tracked, so a join clears them and
        /// the next boot decides afresh.
        FallbackPortal {
            /// The counter that reached its threshold.
            reason: FallbackReason,
            /// Its value, the failed boots that led here.
            failures: u8,
        },
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The result of [`WifiMqttBoot::wait_for_join`].
    #[non_exhaustive]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JoinOutcome {
        /// The station joined within the window; the failure counters are
        /// cleared.
        Joined,
        /// No join within the window; the link worker keeps retrying.
        ///
        /// Call [`WifiMqttBoot::wait_for_join`] again until it returns
        /// `Joined`, so a late join clears the counters, or restart.
        NotJoined {
            /// The class of this boot's failed attempts.
            class: JoinFailure,
            /// [`FallbackDecision::FallbackPortal`] once a counter reached its
            /// threshold: restart, and the next load returns
            /// [`WifiMqttLoadOutcome::FallbackPortal`].
            /// [`FallbackDecision::CarryOn`] below every threshold, and always
            /// for a bundle loaded without the fallback.
            decision: FallbackDecision,
        },
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The outcome of [`run_wifi_mqtt_portal`]; the shared
    /// [`juggler::provisioning::PortalOutcome`]. On `FactoryResetRequested`
    /// erase the store via [`ProvisioningStore::erase_all`].
    pub use juggler::provisioning::PortalOutcome;

    /// Experimental: API may change before 1.0.
    ///
    /// Configuration for [`run_wifi_mqtt_portal`].
    pub struct BootConfig<'a> {
        /// Static configuration for the captive portal AP and schema.
        pub portal: PortalConfig<'a>,
        /// Optional wall-clock timeout for the portal.  `None` blocks until a
        /// terminal event (commit or factory-reset).
        ///
        /// For [`run_wifi_mqtt_fallback_portal`] this is the fallback window,
        /// and `None` means [`FALLBACK_PORTAL_WINDOW_DEFAULT`] (10 min): the
        /// fallback portal always closes so the stored config is retried.
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
    /// overflowed its stack right after logging `Provisioning event: Committed`.
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

        let outcome = session.wait_outcome(config.portal_timeout);
        // Stopping the server waits for a request still being handled, so a
        // save or reset accepted at the deadline has finished once `shutdown`
        // returns; the final state then settles the outcome.
        let state = session.shared_state();
        let shutdown = session.shutdown();
        let outcome = outcome.settled(state.current());
        // Commit-durability rule (in `portal_outcome`): a shutdown error after
        // a commit is logged and `JustProvisioned` returned; on every other
        // outcome it is propagated.
        portal_outcome(outcome, shutdown).context("portal shutdown failed")
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Runs the fallback portal after [`WifiMqttLoadOutcome::FallbackPortal`]:
    /// the SoftAP captive portal with a Wi-Fi-only form.
    ///
    /// - The form offers only the Wi-Fi SSID, prefilled from the stored
    ///   record, and the password, never prefilled; the MQTT broker, OTA URL,
    ///   device name and extras stay as stored, and no factory reset is
    ///   offered, so an open fallback portal cannot change the broker.
    /// - The stored record is kept until a commit replaces its Wi-Fi fields,
    ///   so an aborted or timed-out portal changes nothing.
    /// - The failure counters are cleared just before the portal starts, so
    ///   after a timeout, or a portal that failed to start, the stored config
    ///   is retried for a full streak.
    /// - It closes after `config.portal_timeout`, or
    ///   [`FALLBACK_PORTAL_WINDOW_DEFAULT`] when `None`, and returns
    ///   [`PortalOutcome::PortalExitedWithoutCommit`]; restart to retry the
    ///   stored config.
    ///
    /// `config.portal.profile` must be [`SchemaProfile::WifiMqttDevice`]; its
    /// `defaults` are not used.
    /// The library emits no LED signal of its own: the consumer knows it called
    /// the fallback runner and can show a state distinct from first-boot
    /// provisioning, for example from `config.on_event`.
    ///
    /// The shutdown rules, the library contract (no `restart()`, no erase) and
    /// the stack budget are those of [`run_wifi_mqtt_portal`].
    ///
    /// # Errors
    ///
    /// As [`run_wifi_mqtt_portal`], and when the profile is not
    /// `WifiMqttDevice` or no `WifiMqttDevice` record is stored.
    pub fn run_wifi_mqtt_fallback_portal(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        config: BootConfig<'_>,
    ) -> anyhow::Result<PortalOutcome> {
        if config.portal.profile != SchemaProfile::WifiMqttDevice {
            anyhow::bail!("the fallback portal serves only the WifiMqttDevice profile");
        }
        let mut store = ProvisioningStore::open(nvs.clone())?;
        let has_record = store
            .load()?
            .is_some_and(|stored| stored.profile() == SchemaProfile::WifiMqttDevice);
        if !has_record {
            anyhow::bail!("no stored WifiMqttDevice record for the fallback portal to keep");
        }

        // Cleared before the start: a timed-out portal retries the stored
        // config for a full streak instead of reopening at once, and a portal
        // that fails to start does the same instead of failing on every boot.
        match store.clear_failure_counts() {
            Ok(()) => log::info!("Portal fallback: opening the portal, failure counters cleared"),
            Err(e) => log::warn!("Portal fallback: failed to clear the failure counters: {e:#}"),
        }
        drop(store);

        let mut builder = ProvisioningBuilder::new(config.portal).wifi_only();
        if let Some(on_event) = config.on_event {
            builder = builder.on_event(move |e| on_event(e));
        }
        let session = builder
            .start(modem, sys_loop, nvs)
            .context("failed to start the fallback portal")?;

        let window = config
            .portal_timeout
            .unwrap_or(FALLBACK_PORTAL_WINDOW_DEFAULT);
        let outcome = session.wait_outcome(Some(window));
        // As in `run_wifi_mqtt_portal`: a save accepted at the deadline has
        // finished once `shutdown` returns, and the final state decides.
        let state = session.shared_state();
        let shutdown = session.shutdown();
        let outcome = outcome.settled(state.current());
        portal_outcome(outcome, shutdown).context("portal shutdown failed")
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
    run_wifi_mqtt_fallback_portal, run_wifi_mqtt_portal, BootConfig, JoinOutcome, PortalOutcome,
    WifiMqttBoot, WifiMqttLoadOutcome,
};
