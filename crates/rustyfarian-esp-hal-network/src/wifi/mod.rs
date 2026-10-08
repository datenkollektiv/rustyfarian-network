//! Wi-Fi driver for ESP-HAL projects (bare-metal, `no_std`).
//!
//! Provides a thin async wrapper around `esp-radio 1.0.0-beta.1`'s Wi-Fi controller
//! (a pre-release, pinned exactly; see the crate README "Dependency pins").
//! Since `esp-radio 0.18` the bare-metal Wi-Fi controller is async-only — the
//! synchronous `connect`/`disconnect`/`start` methods that existed in 0.17
//! were removed, and direct `smoltcp` integration was deleted in favour of
//! `embassy-net`.  As a result this crate now exposes a single async entry
//! point: [`WiFiManager::init_async`], which returns an [`AsyncWifiHandle`]
//! wired into an `embassy-net` stack with automatic DHCPv4.
//!
//! The `embassy` Cargo feature is therefore effectively required for any
//! Wi-Fi use on bare-metal targets.
//!
//! # Quick start
//!
//! ```ignore
//! use rustyfarian_esp_hal_network::wifi::{AsyncWifiHandle, WiFiManager, WiFiConfig, WiFiConfigExt};
//!
//! let peripherals = esp_hal::init(esp_hal::Config::default());
//! esp_alloc::heap_allocator!(size: 72 * 1024);
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .with_peripherals(peripherals.TIMG0, peripherals.FROM_CPU_INTR0, peripherals.WIFI);
//! let AsyncWifiHandle { controller, stack, runner } = WiFiManager::init_async(config)?;
//! // spawn `runner.run().await` and a task that owns `controller`
//! ```

pub use juggler::wifi::{
    validate_password, validate_ssid, wifi_disconnect_reason_name, ApConfig, ConnectMode,
    TxPowerLevel, WiFiConfig, WifiDriver, WifiPowerSave, DEFAULT_TIMEOUT_SECS, PASSWORD_MAX_LEN,
    POLL_INTERVAL_MS, SSID_MAX_LEN,
};

/// `embassy-net` `StackResources<N>` size for the SoftAP scaffold.
///
/// The AP `embassy-net` stack opened by [`WiFiManager::init_softap_async`]
/// must reserve one socket slot per long-lived substrate task plus one
/// spare.  The three substrate modules in `rustyfarian-esp-hal-network`
/// each own one socket: `dhcp::run` (UDP), `dns_catchall::run` (UDP),
/// `portal::run_portal_dyn` (TCP).  Adding a fourth long-lived socket-owning
/// task in `rustyfarian-esp-hal-network` requires bumping this constant
/// in lockstep — `embassy-net` returns `SocketAlreadyOpen` /
/// `OutOfResources` on exhaustion rather than blocking.
///
/// Cross-crate coupling: this constant is declared here (in the Wi-Fi
/// crate) to avoid a circular dependency, but the socket count it encodes
/// is owned by the provisioning crate's substrate modules.  If you add a
/// fourth long-lived substrate task in `rustyfarian-esp-hal-network`,
/// bump this constant here too.
///
/// Promoted from a magic `4` in the second-pass PR review of #72 so the
/// invariant is enforced at the source rather than via parallel comments
/// in the example and the driver.
pub const SUBSTRATE_SOCKET_COUNT: usize = 4;

pub use pennant::{NoLed, SimpleLed, StatusLed};

// ─── ActiveLowLed ──────────────────────────────────────────────────────────

/// Active-low GPIO LED adapter for the [`StatusLed`] trait.
///
/// Identical to [`SimpleLed`] but inverts the polarity: the pin is driven
/// **low** to turn the LED on and **high** to turn it off.
///
/// Many dev boards (e.g. ESP32-C3 Super Mini) wire their onboard LED
/// between VCC and a GPIO pin, so pulling the pin low completes the
/// circuit and lights the LED.
pub struct ActiveLowLed<P: embedded_hal::digital::OutputPin> {
    pin: P,
    threshold: u8,
}

impl<P: embedded_hal::digital::OutputPin> ActiveLowLed<P> {
    /// Creates a new `ActiveLowLed` with the default brightness threshold (10).
    pub fn new(pin: P) -> Self {
        Self {
            pin,
            threshold: pennant::DEFAULT_BRIGHTNESS_THRESHOLD,
        }
    }

    /// Creates a new `ActiveLowLed` with a custom brightness threshold.
    pub fn with_threshold(pin: P, threshold: u8) -> Self {
        Self { pin, threshold }
    }
}

impl<P: embedded_hal::digital::OutputPin> StatusLed for ActiveLowLed<P> {
    type Error = P::Error;

    fn set_color(&mut self, color: rgb::RGB8) -> Result<(), Self::Error> {
        if pennant::exceeds_threshold(color, self.threshold) {
            self.pin.set_low()
        } else {
            self.pin.set_high()
        }
    }
}

// ─── Feature-gate guard ─────────────────────────────────────────────────────
//
// `esp-radio 0.18` made the bare-metal Wi-Fi controller async-only, so the
// chip features only produce a working driver in combination with `embassy`.
// Surface that as a compile-time error rather than silently falling through to
// the host stub when a user enables a chip feature without `embassy`.
#[cfg(all(
    any(feature = "esp32c6", feature = "esp32c3"),
    not(feature = "embassy")
))]
compile_error!(
    "rustyfarian-esp-hal-network on bare-metal requires the `embassy` feature \
     (esp-radio is async-only since 0.18). Enable both: --features <chip>,embassy"
);

// ─── Real implementation (behind chip + embassy feature gates) ──────────────

#[cfg(all(feature = "embassy", any(feature = "esp32c6", feature = "esp32c3")))]
mod driver {
    use embassy_net::{
        Config as NetConfig, DhcpConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
        StaticConfigV4,
    };
    use esp_hal::timer::timg::TimerGroup;
    use esp_radio::wifi::ap::AccessPointConfig;
    use esp_radio::wifi::sta::StationConfig;
    use esp_radio::wifi::{
        AuthenticationMethodConfig, Config, ControllerConfig, Interface, Password, PowerSaveMode,
        Ssid, WifiController,
    };
    use juggler::wifi::{
        validate_ap_config, validate_password, validate_ssid, ApConfig, TxPowerLevel, WiFiConfig,
        WifiPowerSave,
    };
    use static_cell::StaticCell;

    /// Wi-Fi configuration bundled with the hardware peripherals needed for init.
    ///
    /// Built from a [`WiFiConfig`] via [`with_peripherals`][WiFiConfigExt::with_peripherals],
    /// then passed to [`WiFiManager::init_async`].
    pub struct HalWifiConfig<'a> {
        ssid: &'a str,
        password: &'a str,
        power_save: WifiPowerSave,
        tx_power: TxPowerLevel,
        timg0: esp_hal::peripherals::TIMG0<'static>,
        from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
        wifi: esp_hal::peripherals::WIFI<'static>,
    }

    /// Extension trait that adds [`with_peripherals`][WiFiConfigExt::with_peripherals]
    /// to [`WiFiConfig`], producing a [`HalWifiConfig`] ready for
    /// [`WiFiManager::init_async`].
    pub trait WiFiConfigExt<'a> {
        /// Bundles this configuration with the ESP32 peripherals required for
        /// bare-metal Wi-Fi.
        fn with_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalWifiConfig<'a>;
    }

    impl<'a> WiFiConfigExt<'a> for WiFiConfig<'a> {
        fn with_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalWifiConfig<'a> {
            HalWifiConfig {
                ssid: self.ssid,
                password: self.password,
                power_save: self.power_save,
                tx_power: self.tx_power,
                timg0,
                from_cpu_intr0,
                wifi,
            }
        }
    }

    /// Error type for [`WiFiManager`] operations.
    #[derive(Debug)]
    pub enum WifiError {
        /// SSID or password validation failed before reaching the radio.
        ConfigureFailed,
        /// An underlying `esp-radio` driver error.
        Driver(esp_radio::wifi::WifiError),
    }

    impl core::fmt::Display for WifiError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::ConfigureFailed => write!(f, "Wi-Fi configuration failed"),
                Self::Driver(inner) => write!(f, "Wi-Fi driver error: {:?}", inner),
            }
        }
    }

    fn map_power_save(ps: WifiPowerSave) -> PowerSaveMode {
        match ps {
            WifiPowerSave::None => PowerSaveMode::None,
            WifiPowerSave::MinModem => PowerSaveMode::Minimum,
            WifiPowerSave::MaxModem => PowerSaveMode::Maximum,
        }
    }

    /// Handle returned by [`WiFiManager::init_async`] carrying the components
    /// needed to drive Wi-Fi from async tasks.
    ///
    /// The caller spawns two tasks: one that owns the [`WifiController`] and
    /// runs reconnection logic, and one that owns the [`embassy_net::Runner`]
    /// and calls `runner.run().await`.  The [`embassy_net::Stack`] is `Copy`
    /// and can be shared with any number of socket tasks.
    pub struct AsyncWifiHandle {
        /// Wi-Fi controller for the reconnection task — owns association state.
        pub controller: WifiController<'static>,
        /// Network stack handle for opening sockets; `Copy`able.
        pub stack: Stack<'static>,
        /// Runner for the network task — `runner.run().await` must be polled
        /// continuously in a dedicated task.
        pub runner: Runner<'static, Interface>,
    }

    /// Bare-metal Wi-Fi manager namespace.
    ///
    /// Since `esp-radio 0.18` the controller is async-only, so this type is a
    /// unit struct that exposes the [`init_async`][WiFiManager::init_async]
    /// constructor.  All useful work happens on the returned
    /// [`AsyncWifiHandle`] and the spawned tasks driving it.
    pub struct WiFiManager;

    impl WiFiManager {
        /// Initialises the scheduler and the Wi-Fi radio, applies the station
        /// credentials immediately after controller construction (which is what
        /// starts the radio), and builds the `embassy-net` stack — but does
        /// **not** initiate association.
        ///
        /// # Readiness
        ///
        /// This function returns as soon as the controller has been configured
        /// and the `embassy-net` stack has been built.  No connection attempt
        /// has been made yet.  The caller is responsible for both the initial
        /// association and all subsequent reconnects: spawn a `wifi_task` that
        /// calls `controller.connect_async()` in a loop, then
        /// `controller.wait_for_disconnect_async()` to block until the link
        /// drops.  Callers that need to know when DHCP has completed must
        /// `await` [`AsyncWifiHandle::wait_for_ip`] (or poll
        /// `Stack::wait_config_up` directly).
        ///
        /// # Heap requirement
        ///
        /// The caller must set up the heap via `esp_alloc::heap_allocator!`
        /// **before** calling this method.  On ESP32-C3 a single 72 KiB region
        /// suffices; on ESP32-C6 two regions are needed (64 KiB reclaimed IRAM
        /// for Wi-Fi DMA + 36 KiB DRAM).  See the chip-specific async examples.
        ///
        /// # Socket budget
        ///
        /// The `embassy-net` stack is sized with `StackResources<3>`, which
        /// covers DHCP plus one TCP and one UDP socket — the baseline used by
        /// `embassy-net`'s own examples.  Applications that need more
        /// concurrent sockets must build their own stack on top of the
        /// station `Interface` singleton:
        ///
        /// ```ignore
        /// let controller =
        ///     esp_radio::wifi::WifiController::new(peripherals.WIFI, ControllerConfig::default())?;
        /// // configure `controller` as in `init_async` above ...
        /// static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
        /// let resources = RESOURCES.init(StackResources::<8>::new());
        /// let (stack, runner) = embassy_net::new(
        ///     esp_radio::wifi::Interface::station(),
        ///     NetConfig::dhcpv4(DhcpConfig::default()),
        ///     resources,
        ///     seed,
        /// );
        /// ```
        ///
        /// # Authentication
        ///
        /// The station is always configured as WPA2-Personal, including when
        /// `password` is empty.  An empty password does **not** select an open
        /// network and will not associate with an open AP — it is accepted (with
        /// a `warn` log) rather than rejected, so that validation stays
        /// symmetric with
        /// [`validate_password`][juggler::wifi::validate_password], which bounds
        /// only the maximum length, and so that STA behaviour stays identical to
        /// the ESP-IDF tier.  Joining an open network is not supported by this
        /// constructor.
        ///
        /// # TX-power policy
        ///
        /// When `tx_power` is left at [`TxPowerLevel::Medium`] (the default),
        /// `init_async` sets no TX power at all and defers to whatever default
        /// `esp-radio` itself applied — 5 dBm as of `esp-radio 1.0.0-beta.1`,
        /// which `WifiController::new` sets internally.  Only an explicitly
        /// requested [`TxPowerLevel`] is applied.
        ///
        /// The reason is the auth-frame corruption pathology on PCB-antenna
        /// boards such as the ESP32-C3/C6 Super Mini, which reflect RF energy
        /// back into the chip at high power and make every WPA2 AP deauth with
        /// `AuthenticationExpired` (reason 2).  Against `esp-radio 0.18`, whose
        /// default was ~20 dBm, this crate forced 8.5 dBm to stay below that
        /// threshold.  `esp-radio 1.0.0-beta.1` already defaults to 5 dBm, which
        /// is lower still, so forcing 8.5 dBm would only *raise* power — the
        /// conservative choice is to leave the driver default alone while the
        /// pathology is unre-validated under beta.1's ESP-IDF 6.1 Wi-Fi blob.
        ///
        /// This couples the effective default to `esp-radio`'s own: **any
        /// `esp-radio` bump must re-check it**, because a driver default above
        /// ~8.5 dBm would silently reintroduce the pathology on those boards.
        /// See `docs/features/archive/esp-hal-stack-upgrade-september-2026-v1.md`.
        ///
        /// Callers can set power explicitly — in either direction — by passing a
        /// [`TxPowerLevel`] via
        /// [`WiFiConfig::with_tx_power`][juggler::wifi::WiFiConfig::with_tx_power].
        /// Note that an explicit [`TxPowerLevel::Medium`] is indistinguishable
        /// from the unset default and is therefore *not* applied.
        ///
        /// # One-shot
        ///
        /// Call at most once per boot — a `static` `StackResources` is
        /// initialised via [`StaticCell`] and the station [`Interface`] is a
        /// singleton; a second call will panic.
        pub fn init_async(config: HalWifiConfig<'_>) -> Result<AsyncWifiHandle, WifiError> {
            validate_ssid(config.ssid).map_err(|_| WifiError::ConfigureFailed)?;
            validate_password(config.password).map_err(|_| WifiError::ConfigureFailed)?;

            // 1. Start the scheduler (esp-radio requires a running scheduler).
            let timg = TimerGroup::new(config.timg0);
            esp_rtos::start(timg.timer0, config.from_cpu_intr0);

            // 2. Construct the Wi-Fi controller with a default ControllerConfig
            //    (empty station config).  Credentials are applied via an explicit
            //    `set_config` call immediately after construction (step 3).
            let mut controller = WifiController::new(config.wifi, ControllerConfig::default())
                .map_err(WifiError::Driver)?;

            // 3. Apply station credentials.  `WifiController::new` already applied
            //    an empty StationConfig (which starts the radio driver via
            //    esp_wifi_start).  This call updates the SSID/password so
            //    wifi_task's first connect_async uses the real credentials.
            //    `Ssid`/`Password` reject oversized values instead of truncating;
            //    `validate_ssid`/`validate_password` above already enforce the
            //    same limits, so a failure here is a driver-level surprise.
            //    WPA2-Personal is kept even for an empty password: that is what
            //    `StationConfig::default()` selected in 0.18 (and still does), it
            //    doubles as the minimum security level for scanning, and it keeps
            //    STA behaviour identical to the ESP-IDF tier.
            let ssid = Ssid::try_from(config.ssid).map_err(WifiError::Driver)?;
            let authentication = AuthenticationMethodConfig::Wpa2Personal(
                Password::try_from(config.password).map_err(WifiError::Driver)?,
            );
            let station = StationConfig::default()
                .with_ssid(ssid)
                .with_authentication(authentication);
            controller
                .set_config(&Config::Station(station))
                .map_err(WifiError::Driver)?;

            // 4. TX power: apply ONLY an explicitly requested level.
            //
            // ESP32-C3/C6 Super Mini and similar PCB-antenna boards reflect RF energy
            // back into the chip at high power, corrupting WPA2 auth frames and causing
            // every AP to deauth with reason 2 (AuthenticationExpired).  ESP-IDF limits
            // TX power internally for regulatory compliance; the bare-metal blob does
            // not.  Against esp-radio 0.18, whose default was ~20 dBm, this crate forced
            // 8.5 dBm to stay under that threshold.
            //
            // esp-radio 1.0.0-beta.1 applies a 5 dBm default of its own inside
            // `WifiController::new` (`esp_wifi_set_max_tx_power(20)`), which is LOWER
            // than 8.5 dBm — forcing 8.5 would only raise it.  So when the caller left
            // tx_power at the juggler default we now set nothing and inherit the driver
            // default, which is the conservative choice while the pathology is
            // unre-validated under beta.1's ESP-IDF 6.1 blob (maintainer's call,
            // 2026-09-26).
            //
            // WARNING: this couples our effective default to esp-radio's. Any esp-radio
            // bump MUST re-check that default — anything above ~8.5 dBm silently
            // reintroduces the pathology on PCB-antenna boards.
            //
            // An explicit level is still applied verbatim. The call is only valid after
            // esp_wifi_start() (before it returns ESP_ERR_WIFI_NOT_STARTED, 0x3002);
            // since esp-radio 1.0.0-beta.1 the initial station config inside
            // `WifiController::new` (step 2) is what starts the radio — `set_config`
            // only restarts it on a mode change — so both steps 2 and 3 precede it.
            //
            // The symbol is already in the linked binary via esp-radio's dependency on
            // esp-wifi-sys; no extra crate dependency is needed.
            //
            // Upstream: esp-rs/esp-hal #3488, espressif/arduino-esp32 #6767.
            if config.tx_power != TxPowerLevel::default() {
                set_tx_power_or_log(config.tx_power.to_quarter_dbm());
            }

            // 5. Power save (non-fatal if it fails).
            let ps = map_power_save(config.power_save);
            if let Err(e) = controller.set_power_saving(ps) {
                log::warn!("Failed to set power save mode (non-fatal): {:?}", e);
            }

            if config.password.is_empty() {
                log::warn!(
                    "Wi-Fi password is empty — auth will fail on WPA2/WPA3 networks; \
                     set WIFI_PASS at build time (option_env! captures at compile time, not runtime)"
                );
            }

            log::info!(
                "Wi-Fi configured (SSID len={}, password: {}), power save: {:?}",
                config.ssid.len(),
                if config.password.is_empty() {
                    "absent"
                } else {
                    "present"
                },
                config.power_save,
            );

            // 5. Build the embassy-net stack on top of the STA interface.
            //    DHCP + one TCP + one UDP baseline (matches embassy-net examples).
            static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
            let resources = RESOURCES.init(StackResources::<3>::new());

            // Seed the stack's local-port RNG from the monotonic clock.
            // This is not cryptographic — it is used only by `embassy-net` for
            // ephemeral source-port randomization.  Upgrade to the `esp-hal`
            // RNG peripheral if `init_async` ever gains access to it.
            let seed = esp_hal::time::Instant::now()
                .duration_since_epoch()
                .as_micros();

            let (stack, runner) = embassy_net::new(
                Interface::station(),
                NetConfig::dhcpv4(DhcpConfig::default()),
                resources,
                seed,
            );

            Ok(AsyncWifiHandle {
                controller,
                stack,
                runner,
            })
        }
    }

    impl AsyncWifiHandle {
        /// Awaits until the `embassy-net` stack has an IPv4 configuration
        /// (either via DHCP or static) and returns the full configuration:
        /// CIDR address, default gateway, and DNS servers.
        ///
        /// For more control (custom timeout, concurrent LED animation),
        /// poll [`embassy_net::Stack::config_v4`] directly alongside other
        /// futures with `embassy_futures::select`.
        pub async fn wait_for_ip(&self) -> embassy_net::StaticConfigV4 {
            self.stack.wait_config_up().await;
            self.stack
                .config_v4()
                .expect("stack reports config up but has no IPv4 config")
        }
    }

    // ─── SoftAP (AP-mode) lifecycle ────────────────────────────────────────────

    /// Fixed IP address of the SoftAP interface (`192.168.4.1`).
    ///
    /// Clients obtain addresses in the `192.168.4.0/24` subnet via DHCP (when a
    /// DHCP server is running on top of the AP stack).  The provisioning crate
    /// references this constant so the AP IP is never restated as a magic literal.
    pub const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);

    /// SoftAP configuration bundled with the hardware peripherals needed for init.
    ///
    /// Built from an [`ApConfig`] via
    /// [`with_ap_peripherals`][ApConfigExt::with_ap_peripherals], then passed to
    /// [`WiFiManager::init_softap_async`].
    pub struct HalApConfig<'a> {
        ap: ApConfig<'a>,
        timg0: esp_hal::peripherals::TIMG0<'static>,
        from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
        wifi: esp_hal::peripherals::WIFI<'static>,
    }

    /// Extension trait that adds
    /// [`with_ap_peripherals`][ApConfigExt::with_ap_peripherals] to [`ApConfig`],
    /// producing a [`HalApConfig`] ready for
    /// [`WiFiManager::init_softap_async`].
    pub trait ApConfigExt<'a> {
        /// Bundles this AP configuration with the ESP32 peripherals required for
        /// bare-metal Wi-Fi.
        fn with_ap_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalApConfig<'a>;
    }

    impl<'a> ApConfigExt<'a> for ApConfig<'a> {
        fn with_ap_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            from_cpu_intr0: esp_hal::peripherals::FROM_CPU_INTR0<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalApConfig<'a> {
            HalApConfig {
                ap: self,
                timg0,
                from_cpu_intr0,
                wifi,
            }
        }
    }

    /// Handle returned by [`WiFiManager::init_softap_async`] carrying the
    /// components needed to drive the SoftAP from async tasks.
    ///
    /// The caller must spawn two tasks:
    ///
    /// - A `net_task` that owns the [`Runner`] and calls `runner.run().await`.
    /// - A `wifi_task` that owns the [`WifiController`] and waits on station
    ///   events directly via
    ///   [`WifiController::wait_for_access_point_connected_event_async`] —
    ///   **the AP radio is already started by the time `init_softap_async`
    ///   returns** (`set_config(Config::AccessPoint(_))` triggers
    ///   `esp_wifi_start()` internally since `esp-radio 0.18`).  There is no
    ///   separate `controller.start_async()` call on either the STA or AP
    ///   side; the `wifi_task` goes straight into the event loop.
    ///
    /// The [`Stack`] is `Copy` and can be shared with any number of socket tasks.
    pub struct SoftApHandle {
        /// Wi-Fi controller for the AP task.
        pub controller: WifiController<'static>,
        /// Network stack handle for opening sockets; `Copy`able.
        pub stack: Stack<'static>,
        /// Runner for the network task — `runner.run().await` must be polled
        /// continuously in a dedicated task.
        pub runner: Runner<'static, Interface>,
    }

    // Separate StaticCell for the AP stack so calling both `init_async` and
    // `init_softap_async` in one boot does not panic on the second `.init()`.
    static AP_RESOURCES: StaticCell<StackResources<{ super::SUBSTRATE_SOCKET_COUNT }>> =
        StaticCell::new();

    impl WiFiManager {
        /// Initialises the scheduler and the Wi-Fi radio in SoftAP mode, applies
        /// the AP configuration, sets TX power, and builds the `embassy-net`
        /// stack with a static IPv4 address (`192.168.4.1/24`).
        ///
        /// # AP configuration
        ///
        /// The access point is WPA2-protected when `config.ap.password` is
        /// `Some(_)`, or open when it is `None`.  Open APs emit a log warning
        /// at `warn` level because anyone in radio range can reach the portal
        /// endpoints.
        ///
        /// # TX-power policy
        ///
        /// The AP path applies the `tx_power` from [`ApConfig`] directly,
        /// including [`TxPowerLevel::Medium`].  This differs from the STA path,
        /// which applies nothing when `tx_power` is left at the default and
        /// inherits `esp-radio`'s own — a caution against the auth-frame
        /// corruption pathology (`AuthenticationExpired`, reason 2) that affects
        /// only a station associating with a remote AP; an AP is unaffected by
        /// that PCB-antenna reflection mode.  Callers that need lower power
        /// (e.g. a captive-portal restricted to a small room) can pass an
        /// explicit [`TxPowerLevel`] via
        /// [`ApConfig::with_tx_power`][juggler::wifi::ApConfig::with_tx_power].
        ///
        /// The TX-power call must happen **after** `set_config()`, which is
        /// what triggers `esp_wifi_start()`.  Failure is non-fatal and logged
        /// at `warn`.
        ///
        /// # Static IP
        ///
        /// The AP stack is wired to `192.168.4.1/24` with the AP itself as
        /// the default gateway.  No DNS servers are configured here; a
        /// captive-portal DNS catch-all is the provisioning crate's concern.
        /// The `AP_IP` const in this crate holds the same address for
        /// callers that need to reference it without restating the literal.
        ///
        /// # One-shot per boot
        ///
        /// Call at most once per boot — a `static` `StackResources` is
        /// initialised via [`StaticCell`] and the access-point [`Interface`]
        /// is a singleton; a second call will panic.
        /// STA and AP in one firmware are not supported through these two
        /// entry points: each consumes `TIMG0`, `FROM_CPU_INTR0` and `WIFI`,
        /// and a second `set_config` would switch the radio out of the first
        /// mode.  Pick one per boot.
        pub fn init_softap_async(config: HalApConfig<'_>) -> Result<SoftApHandle, WifiError> {
            validate_ap_config(&config.ap).map_err(|_| WifiError::ConfigureFailed)?;

            if config.ap.password.is_none() {
                log::warn!(
                    "SoftAP is open (no WPA2 password) — anyone in radio range can reach \
                     the portal endpoints; set a password for any deployment outside a lab"
                );
            }

            // 1. Start the scheduler.
            let timg = TimerGroup::new(config.timg0);
            esp_rtos::start(timg.timer0, config.from_cpu_intr0);

            // 2. Construct the Wi-Fi controller with default ControllerConfig.
            //    `WifiController::new` starts the radio driver; AP credentials
            //    are applied via `set_config` immediately after (step 3).
            let mut controller = WifiController::new(config.wifi, ControllerConfig::default())
                .map_err(WifiError::Driver)?;

            // 3. Build the esp-radio AP config and apply it.
            let ssid = Ssid::try_from(config.ap.ssid).map_err(WifiError::Driver)?;
            let authentication = match config.ap.password {
                Some(pw) => AuthenticationMethodConfig::Wpa2Personal(
                    Password::try_from(pw).map_err(WifiError::Driver)?,
                ),
                None => AuthenticationMethodConfig::Open,
            };
            let ap_cfg = AccessPointConfig::default()
                .with_ssid(ssid)
                .with_channel(config.ap.channel)
                .with_max_connections(config.ap.max_connections as u16)
                .with_authentication(authentication);

            controller
                .set_config(&Config::AccessPoint(ap_cfg))
                .map_err(WifiError::Driver)?;

            // 4. Set TX power.
            //
            // AP mode is not affected by the STA-specific PCB-reflection pathology,
            // so we apply the configured level directly rather than overriding it.
            // Must be called after set_config(): the STA→AP mode change is what
            // triggers esp_wifi_start() here.
            //
            // SAFETY: identical to the STA path — `esp_wifi_set_max_tx_power` is
            // provided by esp-wifi-sys, always linked by esp-radio, and only valid
            // after `esp_wifi_start()`.  `to_quarter_dbm()` returns values in
            // [8, 78], within the SDK-documented valid range [8, 84].
            set_tx_power_or_log(config.ap.tx_power.to_quarter_dbm());

            log::info!(
                "SoftAP configured (ssid len={}, auth={}, channel={}, max_conn={})",
                config.ap.ssid.len(),
                if config.ap.password.is_some() {
                    "WPA2"
                } else {
                    "open"
                },
                config.ap.channel,
                config.ap.max_connections,
            );

            // 5. Build the embassy-net stack with a static AP IP.
            //    The capacity is `SUBSTRATE_SOCKET_COUNT` (DHCP UDP + DNS UDP +
            //    HTTP TCP + one spare); see the crate-root constant for the full
            //    coupling rationale.  A future fourth substrate task that opens
            //    a socket must bump the constant in lockstep.
            let resources =
                AP_RESOURCES.init(StackResources::<{ super::SUBSTRATE_SOCKET_COUNT }>::new());

            let seed = esp_hal::time::Instant::now()
                .duration_since_epoch()
                .as_micros();

            let static_cfg = StaticConfigV4 {
                address: Ipv4Cidr::new(AP_IP, 24),
                gateway: Some(AP_IP),
                dns_servers: Default::default(),
            };

            let (stack, runner) = embassy_net::new(
                Interface::access_point(),
                NetConfig::ipv4_static(static_cfg),
                resources,
                seed,
            );

            Ok(SoftApHandle {
                controller,
                stack,
                runner,
            })
        }
    }

    /// Sets the maximum TX power to `quarter_dbm` (units of 0.25 dBm).
    ///
    /// Shared by the STA and AP init paths so the extern declaration and
    /// SAFETY comment live in one place.
    ///
    /// # Safety invariant
    ///
    /// Must be called after `esp_wifi_start()`, which is triggered by the
    /// `set_config()` call in both `init_async` and `init_softap_async`.
    /// Calling it before start returns `ESP_ERR_WIFI_NOT_STARTED` (0x3002),
    /// which this function logs at `warn` and ignores.
    fn set_tx_power_or_log(quarter_dbm: i8) {
        extern "C" {
            fn esp_wifi_set_max_tx_power(power: i8) -> i32;
        }
        // SAFETY: `esp_wifi_set_max_tx_power` is provided by esp-wifi-sys,
        // always linked by every esp-radio build.  It is only valid after
        // `esp_wifi_start()`.  `to_quarter_dbm()` returns values in [8, 78],
        // within the SDK-documented valid range [8, 84].
        let rc = unsafe { esp_wifi_set_max_tx_power(quarter_dbm) };
        if rc != 0 {
            log::warn!(
                "esp_wifi_set_max_tx_power({}) failed with code {:#010x} (non-fatal)",
                quarter_dbm,
                rc
            );
        }
    }
}

#[cfg(all(feature = "embassy", any(feature = "esp32c6", feature = "esp32c3")))]
pub use driver::{
    ApConfigExt, AsyncWifiHandle, HalApConfig, HalWifiConfig, SoftApHandle, WiFiConfigExt,
    WiFiManager, WifiError, AP_IP,
};

// ─── Stub fallback (no chip feature — host / doc / test builds) ─────────────
//
// The chip-without-embassy combination is rejected by the `compile_error!`
// above, so this branch only fires when no chip feature is selected.

#[cfg(not(any(feature = "esp32c6", feature = "esp32c3")))]
mod stub {
    /// Wi-Fi error placeholder for host builds.
    #[derive(Debug)]
    pub enum WifiError {
        /// Wi-Fi requires a chip feature (`esp32c3` or `esp32c6`) on
        /// bare-metal targets.
        NotSupported,
    }

    impl core::fmt::Display for WifiError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "Wi-Fi not supported on this build configuration")
        }
    }

    /// Wi-Fi manager placeholder for host builds.
    pub struct WiFiManager;

    impl WiFiManager {
        /// Stub constructor that mirrors the real type's surface.
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for WiFiManager {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(not(any(feature = "esp32c6", feature = "esp32c3")))]
pub use stub::{WiFiManager, WifiError};
