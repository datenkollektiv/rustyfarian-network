//! ESP-IDF SoftAP captive-portal provisioning driver.
//!
//! Brings up a Wi-Fi access point, a wildcard DNS responder, and an embedded
//! HTTP captive portal so a field operator can enter Wi-Fi credentials,
//! LoRaWAN OTAA keys, an OTA URL, and a device name with no toolchain present.
//! A valid submission is persisted to NVS via [`ProvisioningStore`] and the
//! host is notified through the [`on_event`](ProvisioningBuilder::on_event)
//! callback; the host then reboots into normal STA boot.
//!
//! The host-testable form/state/SSID logic lives in [`provisioning_pure`]; this
//! crate is the ESP-IDF binding around it, mirroring the
//! `rustyfarian-esp-idf-ota` facade shape.
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
//! use rustyfarian_esp_idf_provisioning::{PortalConfig, ProvisioningBuilder};
//!
//! let config = PortalConfig {
//!     ssid_prefix: "Rustyfarian",
//!     ap_password: Some("provision-me"),
//!     channel: 1,
//!     device_name: "hive-01",
//!     firmware_version: env!("CARGO_PKG_VERSION"),
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

mod dns;
mod portal;
mod store;

pub use store::{ProvisioningStore, StoredConfig};

pub use provisioning_pure::{
    derive_softap_ssid, Field, FieldError, ProvisioningConfig, ProvisioningState, ValidationError,
};

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use esp_idf_svc::eventloop::{EspSystemEventLoop, EspSystemSubscription};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::WifiEvent;

use provisioning_pure::ProvisioningInput;
use rustyfarian_esp_idf_wifi::{softap_mac, ApConfig, SoftApManager};

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
    fn wait_committed(&self, timeout: Option<Duration>) -> Option<ProvisioningConfig> {
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().ok()?;
        loop {
            if let Some(config) = guard.committed.clone() {
                return Some(config);
            }
            match timeout {
                None => {
                    guard = cvar.wait(guard).ok()?;
                }
                Some(t) => {
                    let (g, result) = cvar.wait_timeout(guard, t).ok()?;
                    guard = g;
                    if result.timed_out() && guard.committed.is_none() {
                        return None;
                    }
                }
            }
        }
    }
}

/// Default maximum AP connections (mirrors `wifi_pure::AP_MAX_CONNECTIONS_DEFAULT`).
const DEFAULT_MAX_CONNECTIONS: u8 = wifi_pure::AP_MAX_CONNECTIONS_DEFAULT;

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
                if pw.len() < wifi_pure::AP_PASSWORD_MIN_LEN {
                    anyhow::bail!(
                        "AP password too short (len={}, min {})",
                        pw.len(),
                        wifi_pure::AP_PASSWORD_MIN_LEN
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

        let server = portal::start(
            ap_ip,
            nonce,
            state.clone(),
            store.clone(),
            on_event.clone(),
            device_name,
            firmware_version,
            status_entries,
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
