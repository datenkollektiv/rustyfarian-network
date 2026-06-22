//! ESP-IDF SoftAP captive-portal provisioning driver.
//!
//! Brings up a Wi-Fi access point, a wildcard DNS responder, and an embedded
//! HTTP captive portal so a field operator can provision a device with no
//! toolchain present. The [`PortalConfig::profile`] selects one of two schemas
//! ([`SchemaProfile`]): `LorawanFieldDevice` collects Wi-Fi credentials,
//! LoRaWAN OTAA keys, an OTA URL, and a device name; `WifiMqttDevice` collects
//! Wi-Fi credentials, an MQTT broker URI with optional auth and client ID, an
//! OTA URL, and a device name. A valid submission is persisted to NVS via
//! [`ProvisioningStore`] and the host is notified through the
//! [`on_event`](ProvisioningBuilder::on_event) callback; the host then reboots
//! into normal STA boot.
//!
//! The host-testable form/state/SSID logic lives in [`juggler::provisioning`]; this
//! crate is the ESP-IDF binding around it, mirroring the
//! sibling `ota` module facade shape.
//!
//! All public APIs are experimental.
//!
//! # The library never reboots or erases on its own
//!
//! On a committed submission it emits [`ProvisioningEvent::Committed`] and
//! returns the config from [`ProvisioningSession::wait_committed`]; on a portal
//! factory-reset button press it emits
//! [`ProvisioningEvent::FactoryResetRequested`]. Rebooting and destructive
//! erasure are the host's decisions — call [`ProvisioningStore::erase_all`] and
//! `esp_idf_svc::hal::reset::restart()` from the host as appropriate.
//!
//! # Quick start
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::provisioning::{PortalConfig, ProvisioningBuilder, SchemaProfile};
//!
//! let config = PortalConfig {
//!     ssid_prefix: "Rustyfarian",
//!     ap_password: Some("provision-me"),
//!     channel: 1,
//!     device_name: "hive-01",
//!     firmware_version: env!("CARGO_PKG_VERSION"),
//!     profile: SchemaProfile::LorawanFieldDevice,
//! };
//!
//! let session = ProvisioningBuilder::new(config)
//!     .with_status_entry("battery", "88")
//!     .on_event(|event| log::info!("provisioning event: {event:?}"))
//!     .start(modem, sys_loop, nvs)?;
//!
//! if let Some(cfg) = session.wait_committed(None) {
//!     log::info!("provisioned SSID len={}", cfg.wifi_ssid().len());
//!     session.shutdown()?;
//! }
//! ```

mod boot;
mod dns;
mod portal;
mod store;

pub use store::{ProvisioningStore, StoredConfig};

#[cfg(all(feature = "provisioning", feature = "mqtt"))]
pub use boot::{
    run_wifi_mqtt_portal, BootConfig, PortalOutcome, WifiMqttBoot, WifiMqttLoadOutcome,
};

pub use juggler::provisioning::{
    derive_softap_ssid, Field, FieldError, LoraFields, MqttFields, ProvisioningConfig,
    ProvisioningState, SchemaProfile, ValidationError,
};

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use esp_idf_svc::eventloop::{EspSystemEventLoop, EspSystemSubscription};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::WifiEvent;

use crate::wifi::{softap_mac, ApConfig, SoftApManager};
use juggler::provisioning::ProvisioningInput;

use dns::DnsResponder;

/// Experimental: API may change before 1.0.
///
/// Static configuration for a provisioning portal.
#[derive(Debug, Clone)]
pub struct PortalConfig<'a> {
    /// SoftAP SSID prefix; the AP MAC's last two bytes are appended as
    /// `{prefix}-XXXX` (see [`derive_softap_ssid`]).
    pub ssid_prefix: &'a str,
    /// Optional WPA2 password for the AP. `None` runs an open AP.
    pub ap_password: Option<&'a str>,
    /// 2.4 GHz channel (1–13).
    pub channel: u8,
    /// Human-readable device name surfaced on `/status`.
    pub device_name: &'a str,
    /// Firmware version surfaced on `/status`.
    pub firmware_version: &'a str,
    /// The provisioning schema profile this portal serves.
    ///
    /// Selects the form template, the canonical field set
    /// [`parse_form`](juggler::provisioning::parse_form) validates against, and the
    /// `profile` discriminator the [`ProvisioningStore`] persists. Existing
    /// devices provisioned before the second profile landed read as
    /// [`SchemaProfile::LorawanFieldDevice`] (the NVS v1 → v2 migration in
    /// the [`store`] module), so a beekeeper device upgraded to v2 firmware is
    /// never re-provisioned. The `mqtt_pass` secret follows the same rules as
    /// `wifi_pass` and `app_key`: redacted, never pre-filled, re-entered on
    /// every submission.
    pub profile: SchemaProfile,
}

/// Experimental: API may change before 1.0.
///
/// Lifecycle events delivered to the builder's `on_event` callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningEvent {
    /// The portal AP, DNS responder, and HTTP server are up.
    PortalStarted,
    /// A station associated with the AP.
    ClientConnected,
    /// A `POST /save` failed validation.
    SubmissionRejected,
    /// A valid submission was persisted; credentials are committed.
    Committed,
    /// The portal's factory-reset button was pressed (the host must act).
    FactoryResetRequested,
}

/// The three ways a provisioning session can terminate.
///
/// Returned by [`ProvisioningSession::wait_outcome`] and used internally by
/// [`run_wifi_mqtt_portal`](boot::run_wifi_mqtt_portal) to map into
/// [`PortalOutcome`](boot::PortalOutcome).
///
/// Only consumed by the `mqtt`-gated `boot` module; gated accordingly.
#[cfg(feature = "mqtt")]
#[derive(Debug)]
pub(crate) enum SessionWait {
    /// A valid submission was committed; carries the parsed config.
    Committed(ProvisioningConfig),
    /// The factory-reset button was pressed.
    FactoryResetRequested,
    /// The optional timeout elapsed with no terminal event.
    TimedOut,
}

/// Shared session state behind an `Arc<Mutex<…>>` plus a [`Condvar`].
///
/// `std` `Mutex`/`Condvar` are available and correct under ESP-IDF `std`.
struct StateInner {
    state: ProvisioningState,
    committed: Option<ProvisioningConfig>,
}

/// Handle to the shared session state, cloned into every HTTP handler.
#[derive(Clone)]
pub(crate) struct SharedState {
    inner: Arc<(Mutex<StateInner>, Condvar)>,
    start: Instant,
}

impl SharedState {
    fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(StateInner {
                    state: ProvisioningState::AwaitingSubmission,
                    committed: None,
                }),
                Condvar::new(),
            )),
            start: Instant::now(),
        }
    }

    /// The current provisioning state.
    pub(crate) fn current(&self) -> ProvisioningState {
        self.inner
            .0
            .lock()
            .map(|g| g.state)
            .unwrap_or(ProvisioningState::AwaitingSubmission)
    }

    /// Drives the state machine by `input`, logging (but not failing on) an
    /// invalid transition.
    pub(crate) fn apply(&self, input: ProvisioningInput) {
        if let Ok(mut guard) = self.inner.0.lock() {
            match guard.state.apply(input) {
                Ok(next) => guard.state = next,
                Err(t) => log::warn!("provisioning state machine: {t}"),
            }
        }
    }

    /// Applies `input` to the state machine AND notifies any condvar waiters.
    ///
    /// Use this instead of [`apply`](Self::apply) when the transition reaches a
    /// terminal state that a [`wait_outcome`](Self::wait_outcome) caller must
    /// observe — specifically the `FactoryReset → FactoryResetPending` path.
    pub(crate) fn apply_and_notify(&self, input: ProvisioningInput) {
        if let Ok(mut guard) = self.inner.0.lock() {
            match guard.state.apply(input) {
                Ok(next) => guard.state = next,
                Err(t) => log::warn!("provisioning state machine: {t}"),
            }
            self.inner.1.notify_all();
        }
    }

    /// Stores the committed config and wakes any `wait_committed` waiter.
    pub(crate) fn set_committed(&self, config: ProvisioningConfig) {
        if let Ok(mut guard) = self.inner.0.lock() {
            guard.committed = Some(config);
            self.inner.1.notify_all();
        }
    }

    /// Seconds since the session started, for `/status` `uptime_s`.
    pub(crate) fn uptime_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    /// Blocks until the config is committed or the optional timeout elapses.
    ///
    /// A `Some(timeout)` is treated as a wall-clock deadline computed once at
    /// entry; spurious wakeups consume the elapsed slice instead of restarting
    /// the timer, so the total wait never exceeds the caller's requested
    /// duration.
    fn wait_committed(&self, timeout: Option<Duration>) -> Option<ProvisioningConfig> {
        let (lock, cvar) = &*self.inner;
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!(
                    "provisioning wait_committed: state mutex poisoned, \
                     treating as timeout: {e}"
                );
                return None;
            }
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(config) = guard.committed.clone() {
                return Some(config);
            }
            match deadline {
                None => match cvar.wait(guard) {
                    Ok(g) => guard = g,
                    Err(e) => {
                        log::warn!(
                            "provisioning wait_committed: condvar poisoned, \
                             treating as timeout: {e}"
                        );
                        return None;
                    }
                },
                Some(d) => {
                    let remaining = d.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    match cvar.wait_timeout(guard, remaining) {
                        Ok((g, _)) => guard = g,
                        Err(e) => {
                            log::warn!(
                                "provisioning wait_committed: condvar poisoned, \
                                 treating as timeout: {e}"
                            );
                            return None;
                        }
                    }
                }
            }
        }
    }

    /// Blocks until the session reaches a terminal state or the optional timeout
    /// elapses.
    ///
    /// Terminal states are:
    /// - A committed config → [`SessionWait::Committed`]
    /// - `FactoryResetPending` state → [`SessionWait::FactoryResetRequested`]
    /// - Timeout elapsed → [`SessionWait::TimedOut`]
    ///
    /// Unlike [`wait_committed`](Self::wait_committed), this method also wakes
    /// on the factory-reset path, provided the factory-reset handler calls
    /// [`apply_and_notify`](Self::apply_and_notify) rather than bare
    /// [`apply`](Self::apply).
    #[cfg(feature = "mqtt")]
    pub(crate) fn wait_outcome(&self, timeout: Option<Duration>) -> SessionWait {
        let (lock, cvar) = &*self.inner;
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!(
                    "provisioning wait_outcome: state mutex poisoned, \
                     treating as timeout: {e}"
                );
                return SessionWait::TimedOut;
            }
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // Check terminal states before any wait.
            if let Some(config) = guard.committed.clone() {
                return SessionWait::Committed(config);
            }
            if guard.state == ProvisioningState::FactoryResetPending {
                return SessionWait::FactoryResetRequested;
            }
            match deadline {
                None => match cvar.wait(guard) {
                    Ok(g) => guard = g,
                    Err(e) => {
                        log::warn!(
                            "provisioning wait_outcome: condvar poisoned, \
                             treating as timeout: {e}"
                        );
                        return SessionWait::TimedOut;
                    }
                },
                Some(d) => {
                    let remaining = d.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return SessionWait::TimedOut;
                    }
                    match cvar.wait_timeout(guard, remaining) {
                        Ok((g, _)) => guard = g,
                        Err(e) => {
                            log::warn!(
                                "provisioning wait_outcome: condvar poisoned, \
                                 treating as timeout: {e}"
                            );
                            return SessionWait::TimedOut;
                        }
                    }
                }
            }
        }
    }
}

/// Default maximum AP connections (mirrors `juggler::wifi::AP_MAX_CONNECTIONS_DEFAULT`).
const DEFAULT_MAX_CONNECTIONS: u8 = juggler::wifi::AP_MAX_CONNECTIONS_DEFAULT;

/// Experimental: API may change before 1.0.
///
/// Builder for a [`ProvisioningSession`].
pub struct ProvisioningBuilder<'a> {
    config: PortalConfig<'a>,
    status_entries: Vec<(String, String)>,
    on_event: Option<Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static>>,
}

impl<'a> ProvisioningBuilder<'a> {
    /// Experimental: API may change before 1.0.
    ///
    /// Creates a builder from a [`PortalConfig`].
    pub fn new(config: PortalConfig<'a>) -> Self {
        Self {
            config,
            status_entries: Vec::new(),
            on_event: None,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Adds a `key`/`value` pair rendered under the `/status` `extra` object.
    ///
    /// These values are served unauthenticated to anyone on the AP and must
    /// never contain secrets.
    pub fn with_status_entry(mut self, key: &'a str, value: &'a str) -> Self {
        self.status_entries
            .push((key.to_string(), value.to_string()));
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Registers the lifecycle-event callback.
    ///
    /// The callback runs on the `httpd` task and must return quickly and never
    /// block (the same rule as `MqttBuilder::on_connect`). It requires `Sync`
    /// in addition to `Send` — unlike `MqttBuilder`'s `Send`-only callbacks —
    /// because it is shared via [`Arc`] across multiple HTTP handlers that may
    /// run concurrently.
    pub fn on_event<F>(mut self, f: F) -> Self
    where
        F: Fn(ProvisioningEvent) + Send + Sync + 'static,
    {
        self.on_event = Some(Arc::new(f));
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Starts the SoftAP, DNS responder, and captive-portal HTTP server.
    ///
    /// Reads the SoftAP MAC from efuse (before the radio starts), derives the
    /// SSID, brings up the AP, reads the AP IP, spawns the DNS catch-all,
    /// starts the HTTP server, subscribes to AP-association events, and emits
    /// [`ProvisioningEvent::PortalStarted`].
    pub fn start(
        self,
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> anyhow::Result<ProvisioningSession> {
        let on_event: Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static> =
            self.on_event.unwrap_or_else(|| Arc::new(|_| {}));

        let mac = softap_mac().context("failed to read SoftAP MAC")?;
        let ssid = derive_softap_ssid(self.config.ssid_prefix, &mac);
        log::info!("Provisioning SSID derived (len={})", ssid.len());

        let ap_config = match self.config.ap_password {
            Some(pw) => {
                if pw.len() < juggler::wifi::AP_PASSWORD_MIN_LEN {
                    anyhow::bail!(
                        "AP password too short (len={}, min {})",
                        pw.len(),
                        juggler::wifi::AP_PASSWORD_MIN_LEN
                    );
                }
                ApConfig::wpa2(ssid.as_str(), pw)
            }
            None => ApConfig::open(ssid.as_str()),
        }
        .with_channel(self.config.channel)
        .with_max_connections(DEFAULT_MAX_CONNECTIONS);

        let softap = SoftApManager::start(modem, sys_loop.clone(), Some(nvs.clone()), ap_config)
            .context("failed to start SoftAP")?;

        let ap_ip = softap.ap_ip().context("failed to read AP IP")?;
        log::info!("Provisioning AP up at {ap_ip}");

        let dns = DnsResponder::start(ap_ip).context("failed to start DNS responder")?;

        let store = Arc::new(Mutex::new(
            ProvisioningStore::open(nvs).context("failed to open provisioning store")?,
        ));
        let state = SharedState::new();
        let nonce = Arc::new(generate_nonce());
        let device_name = Arc::new(self.config.device_name.to_string());
        let firmware_version = Arc::new(self.config.firmware_version.to_string());
        let status_entries = Arc::new(self.status_entries);
        let profile = self.config.profile;

        let server = portal::start(
            ap_ip,
            nonce,
            state.clone(),
            store.clone(),
            on_event.clone(),
            device_name,
            firmware_version,
            status_entries,
            profile,
        )?;

        let subscription = subscribe_ap_events(&sys_loop, on_event.clone())?;

        (on_event)(ProvisioningEvent::PortalStarted);

        Ok(ProvisioningSession {
            softap: Some(softap),
            server: Some(server),
            dns: Some(dns),
            state,
            ap_ip,
            _subscription: subscription,
        })
    }
}

/// Subscribes to AP-association events, emitting [`ProvisioningEvent::ClientConnected`]
/// on each `ApStaConnected`.
///
/// The returned subscription must be stored for the session's lifetime: a
/// dropped `EspSubscription` fires zero times (known lore).
fn subscribe_ap_events(
    sys_loop: &EspSystemEventLoop,
    on_event: Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static>,
) -> anyhow::Result<EspSystemSubscription<'static>> {
    sys_loop
        .subscribe::<WifiEvent, _>(move |event: WifiEvent<'_>| {
            if matches!(event, WifiEvent::ApStaConnected(_)) {
                (on_event)(ProvisioningEvent::ClientConnected);
            }
        })
        .map_err(|e| anyhow::anyhow!("AP event subscription failed: {e:?}"))
}

/// Generates an 8-hex-character session nonce from the hardware RNG.
fn generate_nonce() -> String {
    let value = unsafe { esp_idf_svc::sys::esp_random() };
    format!("{value:08x}")
}

/// Experimental: API may change before 1.0.
///
/// A running provisioning session owning the AP, DNS, HTTP server, the
/// AP-event subscription, and the shared state.
pub struct ProvisioningSession {
    softap: Option<SoftApManager>,
    server: Option<esp_idf_svc::http::server::EspHttpServer<'static>>,
    dns: Option<DnsResponder>,
    state: SharedState,
    ap_ip: std::net::Ipv4Addr,
    /// Stored so AP-association events keep firing (dropped subscriptions fire
    /// zero times — known lore).
    _subscription: EspSystemSubscription<'static>,
}

impl ProvisioningSession {
    /// Experimental: API may change before 1.0.
    ///
    /// The current provisioning state.
    pub fn state(&self) -> ProvisioningState {
        self.state.current()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The IPv4 address of the SoftAP netif, read once at session start.
    /// Use this for user-facing log lines (`"open http://{ip}/"`) instead of
    /// hardcoding the ESP-IDF default — the netif may report a non-default
    /// subnet if NVS retained an alternate config from prior firmware.
    pub fn ap_ip(&self) -> std::net::Ipv4Addr {
        self.ap_ip
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Blocks until a valid submission is committed, returning the parsed and
    /// persisted [`ProvisioningConfig`].
    ///
    /// With `timeout = None` it blocks indefinitely; with `Some(d)` it returns
    /// `None` if no commit occurs within `d`. This is the blocking convenience
    /// the host's provisioning-mode main loop sits in.
    pub fn wait_committed(&self, timeout: Option<Duration>) -> Option<ProvisioningConfig> {
        self.state.wait_committed(timeout)
    }

    /// Blocks until the session reaches one of three terminal states or the
    /// optional timeout elapses.
    ///
    /// Unlike [`wait_committed`](Self::wait_committed), this method also returns
    /// when a factory-reset is requested via the portal, so an indefinite
    /// (`timeout = None`) wait does not hang when the user presses the
    /// factory-reset button. Used internally by
    /// [`run_wifi_mqtt_portal`](crate::provisioning::run_wifi_mqtt_portal).
    #[cfg(feature = "mqtt")]
    pub(crate) fn wait_outcome(&self, timeout: Option<Duration>) -> SessionWait {
        self.state.wait_outcome(timeout)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Shuts the session down in dependency order: the HTTP server first (so
    /// nothing answers on a netif about to disappear), then the DNS thread, then
    /// the SoftAP. The AP-event subscription is dropped with `self`.
    pub fn shutdown(mut self) -> anyhow::Result<()> {
        drop(self.server.take());
        if let Some(dns) = self.dns.take() {
            dns.stop();
        }
        if let Some(softap) = self.softap.take() {
            softap.stop().context("failed to stop SoftAP")?;
        }
        Ok(())
    }
}
