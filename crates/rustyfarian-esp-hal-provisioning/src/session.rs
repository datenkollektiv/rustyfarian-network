//! Public `ProvisioningBuilder` / `ProvisioningSession` API for bare-metal
//! SoftAP captive-portal provisioning (ADR 015 §3).
//!
//! The builder accepts a [`PortalConfig`], an optional lifecycle-event callback,
//! and a call to [`ProvisioningBuilder::start`] that spawns the four substrate
//! tasks (net, wifi, DHCP, DNS, HTTP) and returns a [`ProvisioningSession`].
//!
//! # no_std / embassy
//!
//! The public types compile unconditionally.  The `start` body and the embassy
//! tasks are gated on `#[cfg(all(feature = "embassy", any(feature = "esp32c3",
//! feature = "esp32c6")))]`.
//!
//! # The library never reboots or erases
//!
//! On a committed submission the session signals
//! [`ProvisioningOutcome::Committed`]; on a factory-reset button press it
//! signals [`ProvisioningOutcome::FactoryResetRequested`].  Rebooting and
//! destructive flash erasure are the caller's decisions.

use heapless::String as HS;

use juggler::provisioning::{ProvisioningConfig, SchemaProfile};

#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
use crate::store::ProvisioningStore;

// ── Public types ──────────────────────────────────────────────────────────────

/// Static configuration for a provisioning portal session.
///
/// All string fields are borrowed slices that must live at least as long as the
/// call to [`ProvisioningBuilder::start`].  They are copied into `'static`
/// storage inside `start`, so the caller is free to drop `PortalConfig`
/// afterwards.
#[derive(Clone, Copy)]
pub struct PortalConfig<'a> {
    /// SoftAP SSID prefix.  The last two bytes of the AP MAC are appended by
    /// [`juggler::provisioning::derive_softap_ssid`] to form the full SSID.
    pub ssid_prefix: &'a str,
    /// Optional WPA2 password for the AP.  `None` opens an unprotected AP and
    /// emits a `warn!` log; `Some(pw)` where `pw.len() <
    /// juggler::wifi::AP_PASSWORD_MIN_LEN` causes `start` to return
    /// [`ProvisioningError::PasswordTooShort`].
    pub ap_password: Option<&'a str>,
    /// 2.4 GHz channel (1–13).
    pub channel: u8,
    /// Human-readable device name surfaced in the portal header.
    pub device_name: &'a str,
    /// Firmware version string surfaced in the portal header.
    pub firmware_version: &'a str,
    /// Provisioning schema profile — selects the form template and the
    /// canonical field set validated by `parse_form`.
    pub profile: SchemaProfile,
}

/// Lifecycle event delivered to the `on_event` callback registered via
/// [`ProvisioningBuilder::on_event`].
///
/// The callback runs on the HTTP task and must not block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningEvent {
    /// A station (phone) associated with the provisioning AP.
    ///
    /// `mac` is `Option<[u8; 6]>` rather than `[u8; 6]` because v1 does not
    /// yet extract the MAC from the `esp-radio 0.18`
    /// `AccessPointStationEventInfo` payload (the field name has not been
    /// confirmed and the placeholder zero-bytes would otherwise look like a
    /// real address).  The event itself fires reliably on every association;
    /// `mac` will become `Some(_)` in a follow-up once the upstream field is
    /// verified.
    ClientConnected {
        /// Client hardware (MAC) address, or `None` if not yet extractable in
        /// the current `esp-radio` surface (v1 behaviour).
        mac: Option<[u8; 6]>,
    },
    /// A station disassociated from the provisioning AP.
    ///
    /// **Reserved for a follow-up release.**  v1 does not subscribe to the
    /// disassociation event from `esp-radio 0.18`, so this variant is part
    /// of the public surface for forward compatibility but is never
    /// constructed by the current `wifi_task`.
    ClientDisconnected {
        /// Client hardware (MAC) address, or `None` if not yet extractable.
        mac: Option<[u8; 6]>,
    },
    /// A `POST /save` passed nonce and form validation.
    SubmissionAccepted,
    /// A `POST /save` failed nonce or form validation.
    SubmissionRejected,
    /// A valid submission was persisted to flash; credentials are committed.
    Committed,
    /// The factory-reset button was pressed in the portal; the caller must act.
    FactoryResetRequested,
}

/// Outcome of a provisioning session as returned by
/// [`ProvisioningSession::wait_outcome`].
///
/// `ProvisioningConfig` is ~1300 B; boxing it would impose a heap requirement
/// in non-`alloc` host test contexts.  Suppress the lint here — the `Signal`
/// that holds this value is only ever populated from the embassy HTTP task,
/// and the size asymmetry is intentional (the heavy variant carries the
/// fully-parsed credential set).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ProvisioningOutcome {
    /// A valid credential set was committed to flash.
    Committed(ProvisioningConfig),
    /// The portal's factory-reset button was pressed.
    FactoryResetRequested,
    /// The host aborted the session programmatically (reserved for future use).
    HostAborted,
}

/// Errors returned by [`ProvisioningBuilder::start`].
///
/// `SpawnFailed` is `#[doc(hidden)]` and currently unreachable; clippy's
/// `manual_non_exhaustive` lint would suggest replacing the pattern with
/// `#[non_exhaustive]`, but doing so would force every caller's `match`
/// to add a `_` arm — a breaking change for the published v1 surface.
/// The hidden-variant approach is the deliberate alternative.
#[allow(clippy::manual_non_exhaustive)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningError {
    /// The AP password is present but shorter than the minimum required length.
    PasswordTooShort {
        /// The minimum acceptable password length.
        min: usize,
    },
    /// Reserved for future embassy versions where `spawner.spawn()` may become
    /// fallible again (precedent: embassy 0.7's `SpawnError`).  Currently
    /// unreachable — embassy 0.10 panics on pool exhaustion rather than
    /// returning an error, so this variant cannot be constructed by `start`.
    #[doc(hidden)]
    SpawnFailed,
    /// `start` was called a second time in the same boot.
    ///
    /// The session uses `StaticCell` for its shared state, which panics on a
    /// second `init` call.  `start` detects this via `try_init` where available
    /// and converts the failure to this error variant rather than panicking.
    AlreadyStarted,
    /// The requested provisioning profile is not yet supported on the bare-metal tier.
    ///
    /// v1 of `rustyfarian-esp-hal-provisioning` only implements the `WifiMqttDevice`
    /// profile.
    /// The `LorawanFieldDevice` profile (and any future profiles added to
    /// `SchemaProfile`) requires routing, form parsing, validation, and template
    /// substitution that v2 will add.
    /// Selecting an unsupported profile in `PortalConfig::profile` causes `start()`
    /// to fail fast rather than silently render the wrong form.
    ProfileNotSupported {
        /// The profile that was requested but is not implemented in v1.
        profile: SchemaProfile,
    },
}

impl core::fmt::Display for ProvisioningError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProvisioningError::PasswordTooShort { min } => {
                write!(f, "AP password too short (minimum {} characters)", min)
            }
            ProvisioningError::SpawnFailed => write!(f, "embassy task spawn failed"),
            ProvisioningError::AlreadyStarted => {
                write!(
                    f,
                    "provisioning session already started (call start at most once per boot)"
                )
            }
            ProvisioningError::ProfileNotSupported { profile } => {
                write!(
                    f,
                    "provisioning profile {:?} is not supported in v1 \
                     (only WifiMqttDevice is implemented; LoRaWAN portal logic is a v2 expansion)",
                    profile
                )
            }
        }
    }
}

// ── Profile validation ────────────────────────────────────────────────────────

/// Validates that `profile` is supported by this v1 implementation.
///
/// Returns `Ok(())` for [`SchemaProfile::WifiMqttDevice`].
/// Returns [`ProvisioningError::ProfileNotSupported`] for any other profile.
///
/// Extracted as a pure function gated on `cfg(any(test, all(embassy + chip)))`
/// so it is host-testable independently of the embassy + chip feature gates.
/// Called at the top of [`ProvisioningBuilder::start`] — before any peripheral
/// is consumed, before the `StaticCell` is initialised, and before the nonce
/// is generated — so the rejection is cheap and leaves the system state
/// unchanged.
#[cfg(any(
    test,
    all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))
))]
pub(crate) fn validate_profile(profile: SchemaProfile) -> Result<(), ProvisioningError> {
    match profile {
        SchemaProfile::WifiMqttDevice => Ok(()),
        other => Err(ProvisioningError::ProfileNotSupported { profile: other }),
    }
}

// ── Internal shared state ─────────────────────────────────────────────────────

/// State shared between the HTTP portal task and the `ProvisioningSession`
/// handle held by the caller.
///
/// `SharedState` is allocated once per boot in a `StaticCell` by
/// [`ProvisioningBuilder::start`] and lives for the remainder of the program.
// Fields are accessed only in embassy+chip builds; the cfg_attr suppresses
// dead-code warnings in host (no-default-features) builds.
#[cfg_attr(
    not(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))),
    allow(dead_code)
)]
pub(crate) struct SharedState {
    /// Current provisioning state, protected by a critical-section mutex so the
    /// HTTP task and the caller can read/write it concurrently without an async
    /// executor.
    pub state: embassy_sync::blocking_mutex::CriticalSectionMutex<
        core::cell::Cell<juggler::provisioning::ProvisioningState>,
    >,
    /// Signals the session outcome once, then clears.  Consumed by
    /// [`ProvisioningSession::wait_outcome`].
    pub outcome: embassy_sync::signal::Signal<
        embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
        ProvisioningOutcome,
    >,
    /// Per-session CSRF nonce — 8 lowercase hex characters generated from the
    /// hardware RNG at `start` time.
    pub nonce: HS<16>,
    /// IPv4 address of the SoftAP netif in network byte order (big-endian),
    /// read once during `start`.
    pub ap_ip: [u8; 4],
    /// Optional lifecycle-event callback.  `fn` pointer (not `Arc<dyn Fn>`)
    /// because `no_std` forbids heap-allocated closures.
    pub on_event: Option<fn(ProvisioningEvent)>,
}

// ── ProvisioningSession ───────────────────────────────────────────────────────

/// A running provisioning session.
///
/// Returned by [`ProvisioningBuilder::start`].  Holds a reference to the
/// `'static` shared state and the AP IP for use by the caller's main loop.
pub struct ProvisioningSession {
    shared: &'static SharedState,
    ap_ip: [u8; 4],
}

impl ProvisioningSession {
    /// The current provisioning state.
    ///
    /// Reads from the critical-section mutex without blocking.
    pub fn state(&self) -> juggler::provisioning::ProvisioningState {
        self.shared.state.lock(|cell| cell.get())
    }

    /// The IPv4 address of the SoftAP netif in network byte order, read once
    /// at session start.
    ///
    /// Use this for user-facing log lines (`"open http://{}.{}.{}.{}/"`,
    /// `ip[0]`, `ip[1]`, `ip[2]`, `ip[3]`) rather than hard-coding the
    /// default.
    pub fn ap_ip(&self) -> [u8; 4] {
        self.ap_ip
    }

    /// Waits for the provisioning session to reach a terminal outcome.
    ///
    /// Returns once a [`ProvisioningOutcome`] is signalled by the HTTP portal
    /// task.  The signal clears after it is consumed, so calling `wait_outcome`
    /// a second time (or after `wait_committed` already consumed it) will block
    /// forever.
    ///
    /// # Single-caller restriction
    ///
    /// Call at most **one** of `wait_outcome` or `wait_committed`, from at most
    /// **one** task.  `embassy_sync::signal::Signal::wait` clears the signal
    /// after waking; a second caller will block indefinitely.
    pub async fn wait_outcome(&self) -> ProvisioningOutcome {
        self.shared.outcome.wait().await
    }

    /// IDF-parity convenience that narrows the outcome to the commit case.
    ///
    /// Returns `Ok(cfg)` when the session terminates with
    /// [`ProvisioningOutcome::Committed`], and `Err(other)` carrying the
    /// alternative terminal outcome (`FactoryResetRequested` or
    /// `HostAborted`).  A session only ever signals one terminal outcome —
    /// `Signal::wait` is destructive — so this **must not** loop: looping
    /// would consume the only signal then block forever waiting for a
    /// second one that never arrives.
    ///
    /// Callers that need to handle the non-commit terminals (e.g. wipe
    /// flash on `FactoryResetRequested`) should prefer
    /// [`wait_outcome`](Self::wait_outcome), which is strictly more
    /// expressive.
    ///
    /// # Single-caller restriction
    ///
    /// Call at most **one** of `wait_committed` or `wait_outcome`, from at most
    /// **one** task.  See [`wait_outcome`](Self::wait_outcome) for the full
    /// rationale.
    pub async fn wait_committed(&self) -> Result<ProvisioningConfig, ProvisioningOutcome> {
        match self.shared.outcome.wait().await {
            ProvisioningOutcome::Committed(cfg) => Ok(cfg),
            other => Err(other),
        }
    }
}

// ── ProvisioningBuilder ───────────────────────────────────────────────────────

/// Builder for a [`ProvisioningSession`].
///
/// # Example
///
/// ```ignore
/// let session = ProvisioningBuilder::new(PortalConfig {
///     ssid_prefix: "Rustyfarian",
///     ap_password: Some("provision-me"),
///     channel: 1,
///     device_name: "hive-01",
///     firmware_version: env!("CARGO_PKG_VERSION"),
///     profile: SchemaProfile::WifiMqttDevice,
/// })
/// .on_event(|e| log::info!("event: {:?}", e))
/// .start(spawner, ap_handle, store, rng)?;
/// ```
// `config` is only read inside the embassy+chip-gated `start` method.
#[cfg_attr(
    not(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))),
    allow(dead_code)
)]
pub struct ProvisioningBuilder<'a> {
    config: PortalConfig<'a>,
    on_event: Option<fn(ProvisioningEvent)>,
}

impl<'a> ProvisioningBuilder<'a> {
    /// Creates a builder from a [`PortalConfig`].
    pub fn new(config: PortalConfig<'a>) -> Self {
        Self {
            config,
            on_event: None,
        }
    }

    /// Registers the lifecycle-event callback.
    ///
    /// The callback is a bare `fn` pointer (not a closure) because `no_std`
    /// forbids heap-allocated closures.  It runs synchronously inside the HTTP
    /// task and must return quickly without blocking.
    pub fn on_event(mut self, callback: fn(ProvisioningEvent)) -> Self {
        self.on_event = Some(callback);
        self
    }

    /// Starts the substrate tasks and returns a [`ProvisioningSession`].
    ///
    /// # Parameters
    ///
    /// - `spawner` — the embassy task spawner for the current executor.
    /// - `ap` — [`SoftApHandle`](rustyfarian_esp_hal_wifi::SoftApHandle) from
    ///   `WiFiManager::init_softap_async`.
    /// - `store` — opened [`ProvisioningStore`] for credential persistence.
    /// - `rng` — hardware RNG used to generate the per-session CSRF nonce (8 hex
    ///   chars) on every `start()`.  The TRNG entropy depends on the radio being
    ///   active, so `start()` must be called AFTER
    ///   `WiFiManager::init_softap_async` has brought the SoftAP up (the
    ///   `SoftApHandle` parameter already implies this).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the AP password is too short, if a task spawn fails,
    /// or if `start` was already called this boot.
    ///
    /// # Panics
    ///
    /// Panics if `start` is called a second time in the same boot and the
    /// `StaticCell::try_init` path is unavailable.  Prefer checking
    /// [`ProvisioningError::AlreadyStarted`] at runtime.
    #[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
    pub fn start<F>(
        self,
        spawner: embassy_executor::Spawner,
        ap: rustyfarian_esp_hal_wifi::SoftApHandle,
        store: ProvisioningStore<F>,
        rng: esp_hal::rng::Rng,
    ) -> Result<ProvisioningSession, ProvisioningError>
    where
        F: embedded_storage::nor_flash::NorFlash + Send + Sync + 'static,
    {
        use static_cell::StaticCell;

        // ── Step 0: reject unsupported profiles before consuming any resource ─
        // Profile check must come first — before the AP password check, before
        // the nonce is generated, and before the StaticCell is initialised — so
        // that a caller selecting an unsupported profile gets a clean error
        // without any side effects.
        validate_profile(self.config.profile)?;

        // ── Step 1: validate AP password ───────────────────────────────────
        if let Some(pw) = self.config.ap_password {
            if pw.len() < juggler::wifi::AP_PASSWORD_MIN_LEN {
                return Err(ProvisioningError::PasswordTooShort {
                    min: juggler::wifi::AP_PASSWORD_MIN_LEN,
                });
            }
        } else {
            warn_if_open_ap(self.config.ap_password);
        }

        // ── Step 2: generate per-session nonce ─────────────────────────────
        let nonce = generate_nonce(rng);

        // ── Step 3: initialise SharedState in a StaticCell ─────────────────
        static SHARED: StaticCell<SharedState> = StaticCell::new();
        let shared: &'static SharedState = SHARED
            .try_init(SharedState {
                state: embassy_sync::blocking_mutex::CriticalSectionMutex::new(
                    core::cell::Cell::new(
                        juggler::provisioning::ProvisioningState::AwaitingSubmission,
                    ),
                ),
                outcome: embassy_sync::signal::Signal::new(),
                nonce,
                ap_ip: AP_IP,
                on_event: self.on_event,
            })
            .ok_or(ProvisioningError::AlreadyStarted)?;

        // Embassy tasks cannot be generic.  Type-erase the store behind the
        // `PortalStore` trait object using `Box::leak` so the HTTP task can
        // hold a `&'static dyn PortalStore` without a type parameter.
        let store_ref: &'static dyn PortalStore = store_cell_init(store);

        // ── Step 5: derive the portal render config from PortalConfig ───────
        let mut fw_ver = HS::<{ portal::RENDER_FW_VERSION_MAX }>::new();
        // Truncate silently to cap — a firmware version that is too long is
        // a build-time misconfiguration; prefer a truncated display over a
        // panic at runtime.  Same policy applies to `device_name` below.
        let fw_src = &self.config.firmware_version[..self
            .config
            .firmware_version
            .len()
            .min(portal::RENDER_FW_VERSION_MAX)];
        let _ = fw_ver.push_str(fw_src);
        let mut dev_name = HS::<{ portal::RENDER_DEVICE_NAME_MAX }>::new();
        let dn_src = &self.config.device_name[..self
            .config
            .device_name
            .len()
            .min(portal::RENDER_DEVICE_NAME_MAX)];
        let _ = dev_name.push_str(dn_src);
        let portal_config = portal::PortalRenderConfig {
            firmware_version: fw_ver,
            device_name: dev_name,
            profile: self.config.profile,
        };

        // ── Step 6: spawn tasks ─────────────────────────────────────────────
        let rustyfarian_esp_hal_wifi::SoftApHandle {
            controller,
            stack,
            runner,
        } = ap;

        // `Spawner::spawn()` in embassy-executor 0.10 takes a `SpawnToken` and
        // returns `()`.  It panics internally if the task pool is exhausted
        // (a build-time misconfiguration rather than a runtime condition).
        // This matches the pattern `spawner.spawn(task_fn(args).unwrap())` used
        // in the hal_c3_connect_async example where the task function itself may
        // return a Result (for generic tasks) and the `.unwrap()` surfaces
        // pool exhaustion.
        // Task functions decorated with `#[embassy_executor::task]` in this
        // version return `Result<SpawnToken<_>, SpawnError>`; `.unwrap()` gives
        // the `SpawnToken` that `Spawner::spawn()` accepts.
        //
        // NOTE: pool exhaustion intentionally **panics** here rather than
        // mapping to `ProvisioningError::SpawnFailed`.  The variant is reserved
        // (`#[doc(hidden)]`) for a future embassy version that re-introduces
        // a fallible spawn API; today's panic is the correct behaviour because
        // pool sizing is a compile-time integrator decision, not a runtime
        // condition `start()` can recover from.
        spawner.spawn(tasks::net_task(runner).unwrap());
        spawner.spawn(tasks::wifi_task(controller, shared).unwrap());
        spawner.spawn(tasks::dhcp_task(stack).unwrap());
        spawner.spawn(tasks::dns_task(stack).unwrap());
        spawner.spawn(tasks::http_task(stack, shared, store_ref, portal_config).unwrap());

        // ── Step 7: return session ──────────────────────────────────────────
        Ok(ProvisioningSession {
            shared,
            ap_ip: AP_IP,
        })
    }
}

/// Emit a `warn!` log when the provisioning AP has no WPA2 password.
///
/// Extracted as a pure, always-compiled function so the security-checklist
/// item 8 host test (`open_ap_emits_warning`) can capture the log without
/// requiring the embassy + chip feature flags.
///
/// # Security — item 8
///
/// An open AP lets any device in radio range reach the portal.  The warning
/// is emitted unconditionally every time `start` is called with no password,
/// so the integrator cannot accidentally silence it by mistake.
#[cfg(any(
    test,
    all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))
))]
pub(crate) fn warn_if_open_ap(ap_password: Option<&str>) {
    if ap_password.is_none() {
        log::warn!(
            "Provisioning AP is open — no WPA2 password set. \
             Anyone in radio range can reach the portal endpoints."
        );
    }
}

/// The static AP IP address — `192.168.4.1` in network byte order.
///
/// Matches the address configured by `WiFiManager::init_softap_async`.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
const AP_IP: [u8; 4] = [192, 168, 4, 1];

/// Generates an 8-hex-character session nonce from the hardware RNG.
///
/// Reads 4 bytes from `rng` and formats them as 8 lowercase hex characters.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
fn generate_nonce(rng: esp_hal::rng::Rng) -> HS<16> {
    let value = rng.random();
    let mut nonce = HS::<16>::new();
    // Format 4 bytes as 8 lowercase hex characters using the nibble-at-a-time
    // approach (no `format!` macro in no_std without `alloc`).
    for shift in [28u32, 24, 20, 16, 12, 8, 4, 0] {
        let nibble = ((value >> shift) & 0xF) as u8;
        let ch = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        // SAFETY: the string is always 8 chars < capacity 16; push never fails.
        let _ = nonce.push(ch as char);
    }
    nonce
}

/// Places `store` in a heap-allocated mutex and leaks it as a
/// `&'static dyn PortalStore` suitable for passing to the embassy HTTP task.
///
/// Embassy tasks cannot be generic, so the concrete `ProvisioningStore<F>` is
/// erased behind the [`PortalStore`] trait object.
///
/// # Why `Box::leak`
///
/// Embassy tasks require all arguments to be `'static`.  The bare-metal Wi-Fi
/// stack (`esp-radio`) requires a global allocator, so `Box::leak` is
/// available and is the idiomatic single-owner-to-`'static` pattern.
///
/// # Intentional memory leak
///
/// The leaked allocation is never freed — the provisioning session runs for
/// the lifetime of the device.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
fn store_cell_init<F>(store: ProvisioningStore<F>) -> &'static dyn PortalStore
where
    F: embedded_storage::nor_flash::NorFlash + Send + Sync + 'static,
{
    use core::cell::RefCell;

    use embassy_sync::blocking_mutex::CriticalSectionMutex;
    extern crate alloc;

    let boxed: alloc::boxed::Box<dyn PortalStore> =
        alloc::boxed::Box::new(CriticalSectionMutex::new(RefCell::new(store)));
    alloc::boxed::Box::leak(boxed)
}

// ── Internal portal render config ─────────────────────────────────────────────

/// Render-only config forwarded from [`PortalConfig`] to the HTTP portal task.
///
/// Contains the firmware version string needed to substitute `{{FW_VER}}`
/// in the HTML template, plus the schema profile that selects the template.
/// Derived from `PortalConfig` at `start()` time and passed to the HTTP task
/// as an owned value; `heapless::String` fields ensure the struct is
/// `'static`-compatible for embassy task parameters.
///
/// `device_name` is the caller-supplied default that the portal substitutes
/// into `{{DEV_NAME}}` when no stored configuration is present
/// (`Prefill.dev_name` is empty).  Once a previous provisioning cycle has
/// committed a configuration, the stored `dev_name` takes precedence — the
/// renderer prefers `Prefill.dev_name` when non-empty, falling back to
/// `device_name` here so a fresh device still surfaces the integrator's
/// intended name in the portal header.
#[cfg(any(
    test,
    all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))
))]
pub(crate) mod portal {
    use heapless::String as HS;
    use juggler::provisioning::SchemaProfile;

    /// Maximum length for `firmware_version` in the render config.
    pub(crate) const RENDER_FW_VERSION_MAX: usize = 32;
    /// Maximum length for `device_name` in the render config.
    pub(crate) const RENDER_DEVICE_NAME_MAX: usize = 24;

    /// Owned, `'static`-compatible render configuration for the portal template.
    pub(crate) struct PortalRenderConfig {
        pub firmware_version: HS<RENDER_FW_VERSION_MAX>,
        pub device_name: HS<RENDER_DEVICE_NAME_MAX>,
        pub profile: SchemaProfile,
    }
}

// ── Embassy tasks ─────────────────────────────────────────────────────────────

/// Object-safe abstraction over the provisioning flash store.
///
/// Used to pass the store into the HTTP task without a generic type parameter.
/// Embassy tasks cannot be generic, so the concrete `ProvisioningStore<F>` is
/// hidden behind this trait and leaked as a `Box<dyn PortalStore>`.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
pub(crate) trait PortalStore: Send + Sync {
    /// Save `config` to flash.
    fn save(
        &self,
        config: &juggler::provisioning::ProvisioningConfig,
    ) -> Result<(), crate::store::StoreError>;
    /// Load the stored config, if any.
    fn load(
        &self,
    ) -> Result<Option<juggler::provisioning::ProvisioningConfig>, crate::store::StoreError>;
}

/// Blanket impl of [`PortalStore`] for a `Mutex<RefCell<ProvisioningStore<F>>>`.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
impl<F> PortalStore
    for embassy_sync::blocking_mutex::CriticalSectionMutex<
        core::cell::RefCell<ProvisioningStore<F>>,
    >
where
    F: embedded_storage::nor_flash::NorFlash + Send + Sync,
{
    fn save(
        &self,
        config: &juggler::provisioning::ProvisioningConfig,
    ) -> Result<(), crate::store::StoreError> {
        self.lock(|cell| cell.borrow_mut().save(config))
    }

    fn load(
        &self,
    ) -> Result<Option<juggler::provisioning::ProvisioningConfig>, crate::store::StoreError> {
        self.lock(|cell| cell.borrow_mut().load())
    }
}

#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
pub(crate) mod tasks {
    use embassy_net::{Runner, Stack};
    use esp_radio::wifi::{Interface, WifiController};

    use crate::dhcp::{run as dhcp_run, DhcpServerConfig};
    use crate::dns_catchall::{run as dns_run, DnsCatchallConfig};
    use crate::portal::run_portal_dyn;

    use super::portal::PortalRenderConfig;
    use super::{PortalStore, ProvisioningEvent, SharedState};

    /// Drives the embassy-net stack — must be polled continuously.
    #[embassy_executor::task]
    pub(crate) async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
        runner.run().await
    }

    /// Owns the Wi-Fi controller and forwards AP-association events to the
    /// session callback.
    ///
    /// In SoftAP mode the radio is already started when `init_softap_async`
    /// returns.  This task loops on
    /// `wait_for_access_point_connected_event_async` and fires the
    /// `on_event(ClientConnected)` callback.
    ///
    /// Note: `WifiController` in `esp-radio 0.18` does not expose a
    /// `wait_for_access_point_disconnected_event_async` method; only the
    /// connect event is observable via this API.  `ClientDisconnected` events
    /// are therefore not fired in v1.
    #[embassy_executor::task]
    pub(crate) async fn wifi_task(
        controller: WifiController<'static>,
        shared: &'static SharedState,
    ) {
        loop {
            // Wait for a station to connect.
            match controller
                .wait_for_access_point_connected_event_async()
                .await
            {
                Ok(_info) => {
                    // `AccessPointStationEventInfo` — the MAC field name has
                    // not been verified for esp-radio 0.18, so the event
                    // carries `None` rather than a synthetic zero-bytes
                    // placeholder that callers might mistake for a real
                    // address.  When the field name is confirmed in a
                    // follow-up, pass `Some(_info.<field>)` here.
                    log::info!("AP: station connected");
                    if let Some(cb) = shared.on_event {
                        (cb)(ProvisioningEvent::ClientConnected { mac: None });
                    }
                }
                Err(e) => {
                    log::warn!("AP: wait_for_access_point_connected error: {:?}", e);
                    // Yield before retrying so we do not spin on a persistent error.
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Runs the DHCP server on the AP stack.
    #[embassy_executor::task]
    pub(crate) async fn dhcp_task(stack: Stack<'static>) -> ! {
        dhcp_run(stack, DhcpServerConfig::default()).await
    }

    /// Runs the DNS catch-all server on the AP stack.
    #[embassy_executor::task]
    pub(crate) async fn dns_task(stack: Stack<'static>) -> ! {
        dns_run(stack, DnsCatchallConfig::default()).await
    }

    /// Runs the captive-portal HTTP server.
    ///
    /// The store is passed as a `&'static dyn PortalStore` to avoid a generic
    /// type parameter (embassy tasks cannot be generic).
    #[embassy_executor::task]
    pub(crate) async fn http_task(
        stack: Stack<'static>,
        shared: &'static SharedState,
        store: &'static dyn PortalStore,
        config: PortalRenderConfig,
    ) -> ! {
        run_portal_dyn(stack, shared, store, config).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    extern crate std;

    // ── Capturing log sink ────────────────────────────────────────────────────
    //
    // `log::set_logger` can only succeed once per process; wrap it in an
    // atomic flag + `OnceLock` so multiple tests in one binary run do not
    // fight over it.

    use core::sync::atomic::{AtomicBool, Ordering};

    static LOGGER_INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Minimal capturing log implementation.  Messages are appended to a
    /// process-global `Vec<String>` protected by a `std::sync::Mutex`.
    ///
    /// Tests that care about log output must call `install_test_logger` (once)
    /// and then `drain_log_messages` to read and clear the buffer.
    struct CapturingLogger;

    /// Process-global message store.
    static LOG_MESSAGES: std::sync::OnceLock<
        std::sync::Mutex<alloc::vec::Vec<alloc::string::String>>,
    > = std::sync::OnceLock::new();

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let store = LOG_MESSAGES.get_or_init(|| std::sync::Mutex::new(alloc::vec::Vec::new()));
            if let Ok(mut msgs) = store.lock() {
                msgs.push(alloc::format!("{}", record.args()));
            }
        }

        fn flush(&self) {}
    }

    static CAPTURING_LOGGER: CapturingLogger = CapturingLogger;

    /// Install the capturing logger (idempotent — safe to call from multiple
    /// tests in the same binary run).
    fn install_test_logger() {
        if !LOGGER_INSTALLED.swap(true, Ordering::SeqCst) {
            let _ = log::set_logger(&CAPTURING_LOGGER)
                .map(|()| log::set_max_level(log::LevelFilter::Warn));
        }
    }

    /// Take and clear all captured log messages.
    fn drain_log_messages() -> alloc::vec::Vec<alloc::string::String> {
        let store = LOG_MESSAGES.get_or_init(|| std::sync::Mutex::new(alloc::vec::Vec::new()));
        let mut msgs = store.lock().expect("log mutex poisoned");
        let drained: alloc::vec::Vec<_> = msgs.drain(..).collect();
        drained
    }

    // ── Security-checklist item 8 test ────────────────────────────────────────

    /// Security-checklist item 8 lock: when the provisioning AP has no WPA2
    /// password, a `Warn`-level log message containing "open" must be emitted.
    ///
    /// The library never silences this warning — an integrator who accidentally
    /// opens the AP cannot miss it.
    #[test]
    fn open_ap_emits_warning() {
        install_test_logger();
        // Clear any messages from previous tests in the same process run.
        let _ = drain_log_messages();

        // Call the pure warning helper with no password.
        warn_if_open_ap(None);

        let messages = drain_log_messages();
        let combined = messages.join("\n");

        assert!(
            combined.to_lowercase().contains("open"),
            "open-AP warning must contain 'open' (got: {combined:?})"
        );

        // A password-set call must NOT emit the warning.
        warn_if_open_ap(Some("my-secure-pass"));
        let messages_after = drain_log_messages();
        assert!(
            messages_after.is_empty(),
            "warn_if_open_ap(Some(...)) must not log anything (got: {messages_after:?})"
        );
    }

    // ── validate_profile tests ────────────────────────────────────────────────

    /// `validate_profile` accepts `WifiMqttDevice` — the only v1-supported profile.
    #[test]
    fn validate_profile_accepts_wifi_mqtt() {
        assert_eq!(
            validate_profile(SchemaProfile::WifiMqttDevice),
            Ok(()),
            "WifiMqttDevice must be accepted by validate_profile"
        );
    }

    /// `validate_profile` rejects `LorawanFieldDevice` with `ProfileNotSupported`.
    ///
    /// Locks the behaviour that the pure `validate_profile` check (which
    /// `start()` invokes before any peripheral consumption) rejects v2-only
    /// profiles rather than letting them silently render the wrong form.
    #[test]
    fn validate_profile_rejects_lorawan() {
        let result = validate_profile(SchemaProfile::LorawanFieldDevice);
        assert_eq!(
            result,
            Err(ProvisioningError::ProfileNotSupported {
                profile: SchemaProfile::LorawanFieldDevice
            }),
            "LorawanFieldDevice must be rejected by validate_profile with ProfileNotSupported"
        );
    }

    /// `ProfileNotSupported` carries only a `SchemaProfile` discriminant — no
    /// credential bytes.
    ///
    /// Security exhaustiveness lock: the `Debug` representation of
    /// `ProvisioningError::ProfileNotSupported` must not contain any input
    /// credential bytes (it only carries an enum discriminant).
    #[test]
    fn profile_not_supported_carries_no_input_bytes() {
        const SENTINEL: &str = "CREDENTIAL-SENTINEL-7F3B";
        let err = ProvisioningError::ProfileNotSupported {
            profile: SchemaProfile::LorawanFieldDevice,
        };
        let debug_str = alloc::format!("{err:?}");
        assert!(
            !debug_str.contains(SENTINEL),
            "ProfileNotSupported Debug must not contain credential sentinel: {debug_str}"
        );
        // Confirm the Display message also carries no credential bytes.
        let display_str = alloc::format!("{err}");
        assert!(
            !display_str.contains(SENTINEL),
            "ProfileNotSupported Display must not contain credential sentinel: {display_str}"
        );
    }
}
