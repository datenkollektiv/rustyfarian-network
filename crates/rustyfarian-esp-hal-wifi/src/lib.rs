//! Wi-Fi driver for ESP-HAL projects (bare-metal, `no_std`).
//!
//! Provides a thin async wrapper around `esp-radio 0.18`'s Wi-Fi controller.
//! In `esp-radio 0.18` the bare-metal Wi-Fi controller is async-only — the
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
//! use rustyfarian_esp_hal_wifi::{AsyncWifiHandle, WiFiManager, WiFiConfig, WiFiConfigExt};
//!
//! let peripherals = esp_hal::init(esp_hal::Config::default());
//! esp_alloc::heap_allocator!(size: 72 * 1024);
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .with_peripherals(peripherals.TIMG0, peripherals.SW_INTERRUPT, peripherals.WIFI);
//! let AsyncWifiHandle { controller, stack, runner } = WiFiManager::init_async(config)?;
//! // spawn `runner.run().await` and a task that owns `controller`
//! ```

#![no_std]

/// Minimal DHCP server — Phase 2 spike code (ADR 015 §3 fallback).
///
/// Gated behind the `provisioning-spike` feature flag.  Not API-stable; see
/// the module-level documentation for the migration path.
#[cfg(feature = "provisioning-spike")]
pub mod dhcp;

/// DNS catch-all server — Phase 2 spike code (ADR 015 §3 fallback).
///
/// Gated behind the `provisioning-spike` feature flag.  Not API-stable; see
/// the module-level documentation for the migration path.
#[cfg(feature = "provisioning-spike")]
pub mod dns_catchall;

/// Minimal HTTP/1.1 server — Phase 2 spike code (ADR 015 §3 fallback).
///
/// Gated behind the `provisioning-spike` feature flag.  Not API-stable; see
/// the module-level documentation for the migration path.
#[cfg(feature = "provisioning-spike")]
pub mod http_server;

pub use wifi_pure::{
    validate_password, validate_ssid, wifi_disconnect_reason_name, ApConfig, ConnectMode,
    TxPowerLevel, WiFiConfig, WifiDriver, WifiPowerSave, DEFAULT_TIMEOUT_SECS, PASSWORD_MAX_LEN,
    POLL_INTERVAL_MS, SSID_MAX_LEN,
};

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
    "rustyfarian-esp-hal-wifi on bare-metal requires the `embassy` feature \
     (esp-radio 0.18 is async-only). Enable both: --features <chip>,embassy"
);

// ─── Real implementation (behind chip + embassy feature gates) ──────────────

#[cfg(all(feature = "embassy", any(feature = "esp32c6", feature = "esp32c3")))]
mod driver {
    use embassy_net::{
        Config as NetConfig, DhcpConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
        StaticConfigV4,
    };
    use esp_hal::interrupt::software::SoftwareInterruptControl;
    use esp_hal::timer::timg::TimerGroup;
    use esp_radio::wifi::ap::AccessPointConfig;
    use esp_radio::wifi::sta::StationConfig;
    use esp_radio::wifi::{
        AuthenticationMethod, Config, ControllerConfig, Interface, PowerSaveMode, WifiController,
    };
    use static_cell::StaticCell;
    use wifi_pure::{
        validate_ap_config, validate_password, validate_ssid, ApConfig, TxPowerLevel, WiFiConfig,
        WifiPowerSave,
    };

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
        sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
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
            sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalWifiConfig<'a>;
    }

    impl<'a> WiFiConfigExt<'a> for WiFiConfig<'a> {
        fn with_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalWifiConfig<'a> {
            HalWifiConfig {
                ssid: self.ssid,
                password: self.password,
                power_save: self.power_save,
                tx_power: self.tx_power,
                timg0,
                sw_interrupt,
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
        pub runner: Runner<'static, Interface<'static>>,
    }

    /// Bare-metal Wi-Fi manager namespace.
    ///
    /// In `esp-radio 0.18` the controller is async-only, so this type is a
    /// unit struct that exposes the [`init_async`][WiFiManager::init_async]
    /// constructor.  All useful work happens on the returned
    /// [`AsyncWifiHandle`] and the spawned tasks driving it.
    pub struct WiFiManager;

    impl WiFiManager {
        /// Initialises the scheduler and the Wi-Fi radio, sets the station
        /// credentials before the radio starts, and builds the `embassy-net`
        /// stack — but does **not** initiate association.
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
        /// `Interface` returned by `esp_radio::wifi::new(..)`:
        ///
        /// ```ignore
        /// let (controller, interfaces) =
        ///     esp_radio::wifi::new(peripherals.WIFI, ControllerConfig::default())?;
        /// // configure `controller` as in `init_async` above ...
        /// static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
        /// let resources = RESOURCES.init(StackResources::<8>::new());
        /// let (stack, runner) = embassy_net::new(
        ///     interfaces.station,
        ///     NetConfig::dhcpv4(DhcpConfig::default()),
        ///     resources,
        ///     seed,
        /// );
        /// ```
        ///
        /// # TX-power policy
        ///
        /// When `tx_power` is left at [`TxPowerLevel::Medium`] (the default),
        /// `init_async` silently overrides it to [`TxPowerLevel::Low`] (8.5 dBm).
        ///
        /// This is a deliberate library-wide default: PCB-antenna boards such as
        /// the ESP32-C3/C6 Super Mini reflect RF energy at full power (~20 dBm),
        /// corrupting WPA2 auth frames.  8.5 dBm is a safe baseline for all
        /// bare-metal builds; callers with external antennas can raise it by
        /// passing an explicit [`TxPowerLevel`] via
        /// [`WiFiConfig::with_tx_power`][wifi_pure::WiFiConfig::with_tx_power].
        ///
        /// # One-shot
        ///
        /// Call at most once per boot — a `static` `StackResources` is
        /// initialised via [`StaticCell`] and a second call will panic.
        pub fn init_async(config: HalWifiConfig<'_>) -> Result<AsyncWifiHandle, WifiError> {
            validate_ssid(config.ssid).map_err(|_| WifiError::ConfigureFailed)?;
            validate_password(config.password).map_err(|_| WifiError::ConfigureFailed)?;

            // 1. Start the scheduler (esp-radio requires a running scheduler).
            let timg = TimerGroup::new(config.timg0);
            let sw_ints = SoftwareInterruptControl::new(config.sw_interrupt);
            esp_rtos::start(timg.timer0, sw_ints.software_interrupt0);

            // 2. Construct the Wi-Fi controller with a default ControllerConfig
            //    (empty station config).  Credentials are applied via an explicit
            //    `set_config` call immediately after `wifi::new` returns (step 3).
            let (mut controller, interfaces) =
                esp_radio::wifi::new(config.wifi, ControllerConfig::default())
                    .map_err(WifiError::Driver)?;

            // 3. Apply station credentials.  esp_radio::wifi::new already called
            //    set_config internally with an empty StationConfig (which starts the
            //    radio driver via esp_wifi_start).  This call updates the SSID/password
            //    so wifi_task's first connect_async uses the real credentials.
            let station = StationConfig::default()
                .with_ssid(config.ssid)
                .with_password(config.password.into());
            controller
                .set_config(&Config::Station(station))
                .map_err(WifiError::Driver)?;

            // 4. Limit TX power to 8.5 dBm (34 × 0.25 dBm).
            //
            // ESP32-C3/C6 Super Mini and similar PCB-antenna boards reflect RF energy
            // back into the chip at full power (~20 dBm), corrupting WPA2 auth frames
            // and causing every AP to deauth with reason 2 (AuthenticationExpired).
            // ESP-IDF limits TX power internally for regulatory compliance; the
            // bare-metal blob does not.  This call must come after set_config() (step 3)
            // because that is what triggers esp_wifi_start() — calling it before returns
            // ESP_ERR_WIFI_NOT_STARTED (0x3002).
            //
            // The symbol is already in the linked binary via esp-radio's dependency on
            // esp-wifi-sys; no extra crate dependency is needed.
            //
            // Upstream: esp-rs/esp-hal #3488, espressif/arduino-esp32 #6767.
            //
            // Default to Low (8.5 dBm) if the caller left tx_power at Medium (the
            // wifi_pure default). Medium (~13 dBm) still causes auth failures on
            // PCB-antenna boards; Low is the safe baseline for bare-metal.
            let quarter_dbm = if config.tx_power == TxPowerLevel::default() {
                TxPowerLevel::Low.to_quarter_dbm()
            } else {
                config.tx_power.to_quarter_dbm()
            };
            set_tx_power_or_log(quarter_dbm);

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
                interfaces.station,
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
        sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
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
            sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalApConfig<'a>;
    }

    impl<'a> ApConfigExt<'a> for ApConfig<'a> {
        fn with_ap_peripherals(
            self,
            timg0: esp_hal::peripherals::TIMG0<'static>,
            sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
            wifi: esp_hal::peripherals::WIFI<'static>,
        ) -> HalApConfig<'a> {
            HalApConfig {
                ap: self,
                timg0,
                sw_interrupt,
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
    /// - A `wifi_task` that owns the [`WifiController`] and calls
    ///   `controller.start_async().await` once, then waits on station events via
    ///   [`WifiController::wait_for_access_point_connected_event_async`].
    ///
    /// The [`Stack`] is `Copy` and can be shared with any number of socket tasks.
    pub struct SoftApHandle {
        /// Wi-Fi controller for the AP task.
        pub controller: WifiController<'static>,
        /// Network stack handle for opening sockets; `Copy`able.
        pub stack: Stack<'static>,
        /// Runner for the network task — `runner.run().await` must be polled
        /// continuously in a dedicated task.
        pub runner: Runner<'static, Interface<'static>>,
    }

    // Separate StaticCell for the AP stack so calling both `init_async` and
    // `init_softap_async` in one boot does not panic on the second `.init()`.
    static AP_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

    impl WiFiManager {
        /// Initialises the scheduler and the Wi-Fi radio in SoftAP mode, applies
        /// the AP configuration, clamps TX power, and builds the `embassy-net`
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
        /// Unlike the STA path, the AP path applies the `tx_power` from
        /// [`ApConfig`] directly, without overriding `Medium` to `Low`.  The
        /// STA Medium-to-Low override defends against an auth-frame corruption
        /// pathology (`AuthenticationExpired`, reason 2) that exists only for
        /// clients connecting to a remote AP; the AP itself is unaffected by
        /// that PCB-antenna reflection mode.  Callers that need lower power
        /// (e.g. a captive-portal restricted to a small room) can pass an
        /// explicit [`TxPowerLevel`] via
        /// [`ApConfig::with_tx_power`][wifi_pure::ApConfig::with_tx_power].
        ///
        /// TX-power clamping must happen **after** `set_config()`, which is
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
        /// initialised via [`StaticCell`] and a second call will panic.
        /// If you need both STA and AP in the same firmware, call
        /// [`WiFiManager::init_async`] first for STA, then
        /// `init_softap_async` for AP; the scheduler is started by
        /// `init_async`, so `init_softap_async` must be called afterwards.
        /// Calling `init_softap_async` before `init_async` also works, but
        /// calling either twice is not supported.
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
            let sw_ints = SoftwareInterruptControl::new(config.sw_interrupt);
            esp_rtos::start(timg.timer0, sw_ints.software_interrupt0);

            // 2. Construct the Wi-Fi controller with default ControllerConfig.
            //    `esp_radio::wifi::new` starts the radio driver; AP credentials
            //    are applied via `set_config` immediately after (step 3).
            let (mut controller, interfaces) =
                esp_radio::wifi::new(config.wifi, ControllerConfig::default())
                    .map_err(WifiError::Driver)?;

            // 3. Build the esp-radio AP config and apply it.
            let mut ap_cfg = AccessPointConfig::default()
                .with_ssid(config.ap.ssid)
                .with_channel(config.ap.channel)
                .with_max_connections(config.ap.max_connections as u16);

            match config.ap.password {
                Some(pw) => {
                    ap_cfg = ap_cfg
                        .with_auth_method(AuthenticationMethod::Wpa2Personal)
                        .with_password(pw.into());
                }
                None => {
                    ap_cfg = ap_cfg.with_auth_method(AuthenticationMethod::None);
                }
            }

            controller
                .set_config(&Config::AccessPoint(ap_cfg))
                .map_err(WifiError::Driver)?;

            // 4. Clamp TX power.
            //
            // AP mode is not affected by the STA-specific PCB-reflection pathology,
            // so we apply the configured level directly rather than overriding it.
            // Must be called after set_config() (which triggers esp_wifi_start()).
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
            //    `StackResources::<4>` — DHCP server (UDP) + DNS (UDP) + HTTP (TCP)
            //    + one spare — covers the Phase 2 edge-net substrate without needing
            //    the caller to rebuild the stack.
            let resources = AP_RESOURCES.init(StackResources::<4>::new());

            let seed = esp_hal::time::Instant::now()
                .duration_since_epoch()
                .as_micros();

            let static_cfg = StaticConfigV4 {
                address: Ipv4Cidr::new(AP_IP, 24),
                gateway: Some(AP_IP),
                dns_servers: Default::default(),
            };

            let (stack, runner) = embassy_net::new(
                interfaces.access_point,
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

    /// Clamps the TX power to `quarter_dbm` (units of 0.25 dBm).
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
