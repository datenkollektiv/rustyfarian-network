//! Wi-Fi driver for ESP-HAL projects (bare-metal, no_std).
//!
//! Provides [`WiFiManager`], a [`WifiDriver`] implementation that
//! connects to a WPA2 access point using `esp-radio` on bare-metal targets.
//!
//! # Quick start
//!
//! The caller only needs to initialize the HAL with maximum CPU clock.
//! `WiFiManager::init` handles the heap, scheduler, radio, and Wi-Fi:
//!
//! ```ignore
//! use esp_hal::clock::CpuClock;
//! use rustyfarian_esp_hal_wifi::{WiFiManager, WiFiConfig, WiFiConfigExt};
//!
//! let peripherals = esp_hal::init(
//!     esp_hal::Config::default().with_cpu_clock(CpuClock::max()),
//! );
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .with_peripherals(peripherals.TIMG0, peripherals.SW_INTERRUPT, peripherals.WIFI);
//! let wifi = WiFiManager::init(config).unwrap();
//! ```
//!
//! After association, call [`WiFiManager::take_sta_device`] to obtain the
//! `WifiDevice` for use with `smoltcp` or `embassy-net`.

#![no_std]

// Re-export all pure types from wifi-pure (matching rustyfarian-esp-idf-wifi parity)
pub use wifi_pure::{
    validate_password, validate_ssid, wifi_disconnect_reason_name, ConnectMode, WiFiConfig,
    WifiDriver, WifiPowerSave, DEFAULT_TIMEOUT_SECS, PASSWORD_MAX_LEN, POLL_INTERVAL_MS,
    SSID_MAX_LEN,
};

// Re-export StatusLed and SimpleLed from led_effects for convenience
pub use led_effects::{SimpleLed, StatusLed};

// ─── Real implementation (behind chip feature gates) ────────────────────────

#[cfg(any(feature = "esp32c6", feature = "esp32c3"))]
mod driver {
    extern crate alloc;

    use esp_hal::interrupt::software::SoftwareInterruptControl;
    use esp_hal::timer::timg::TimerGroup;
    use esp_radio::wifi::{
        ClientConfig, Interfaces, ModeConfig, PowerSaveMode, WifiController, WifiDevice,
    };
    use wifi_pure::{validate_password, validate_ssid, WiFiConfig, WifiDriver, WifiPowerSave};

    /// Minimum heap size for the Wi-Fi radio stack (bytes).
    const WIFI_HEAP_SIZE: usize = 72 * 1024;

    /// Heap backing store for the Wi-Fi radio stack.
    ///
    /// Placed in the library so the application does not need to call
    /// `esp_alloc::heap_allocator!()` when using [`WiFiManager::init`].
    static mut WIFI_HEAP: core::mem::MaybeUninit<[u8; WIFI_HEAP_SIZE]> =
        core::mem::MaybeUninit::uninit();

    fn smoltcp_now() -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_millis(
            esp_hal::time::Instant::now()
                .duration_since_epoch()
                .as_millis() as i64,
        )
    }

    /// Wi-Fi configuration bundled with the hardware peripherals needed for init.
    ///
    /// Built from a [`WiFiConfig`] via [`with_peripherals`][WiFiConfig::with_peripherals],
    /// then passed to [`WiFiManager::init`].
    pub struct HalWifiConfig<'a> {
        ssid: &'a str,
        password: &'a str,
        power_save: WifiPowerSave,
        timg0: esp_hal::peripherals::TIMG0<'static>,
        sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
        wifi: esp_hal::peripherals::WIFI<'static>,
    }

    /// Extension trait that adds [`with_peripherals`][WiFiConfigExt::with_peripherals]
    /// to [`WiFiConfig`], producing a [`HalWifiConfig`] ready for
    /// [`WiFiManager::init`].
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
                timg0,
                sw_interrupt,
                wifi,
            }
        }
    }

    /// Error type for [`WiFiManager`] operations.
    #[derive(Debug)]
    pub enum WifiError {
        /// Validation or configuration of the Wi-Fi driver failed.
        ConfigureFailed,
        /// Wi-Fi hardware failed to start.
        StartFailed,
        /// Association with the AP failed or was refused.
        ConnectFailed,
        /// Disconnect request failed.
        DisconnectFailed,
        /// Radio initialization failed.
        RadioInitFailed,
        /// An underlying `esp-radio` driver error.
        Driver(esp_radio::wifi::WifiError),
    }

    impl core::fmt::Display for WifiError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::ConfigureFailed => write!(f, "Wi-Fi configuration failed"),
                Self::StartFailed => write!(f, "Wi-Fi start failed"),
                Self::ConnectFailed => write!(f, "Wi-Fi connect failed"),
                Self::DisconnectFailed => write!(f, "Wi-Fi disconnect failed"),
                Self::RadioInitFailed => write!(f, "radio initialization failed"),
                Self::Driver(inner) => write!(f, "Wi-Fi driver error: {:?}", inner),
            }
        }
    }

    /// Bare-metal Wi-Fi manager for ESP-HAL targets.
    ///
    /// Wraps an `esp-radio` [`WifiController`] and implements the [`WifiDriver`] trait.
    ///
    /// # Quick start
    ///
    /// ```ignore
    /// let peripherals = esp_hal::init(
    ///     esp_hal::Config::default().with_cpu_clock(CpuClock::max()),
    /// );
    ///
    /// let config = WiFiConfig::new("MyNetwork", "password123")
    ///     .with_peripherals(peripherals.TIMG0, peripherals.SW_INTERRUPT, peripherals.WIFI);
    /// let mut wifi = WiFiManager::init(config)?;
    /// let ip = wifi.wait_connected(30_000)?;
    /// ```
    ///
    /// [`init`][Self::init] handles heap allocation, the RTOS scheduler, radio
    /// init, STA configuration, and connection initiation.
    /// The only prerequisite is `esp_hal::init` with `CpuClock::max()`.
    ///
    /// For advanced use (e.g. sharing the scheduler with other subsystems),
    /// [`new`][Self::new] accepts pre-initialized `esp-radio` objects.
    ///
    /// # Network stack
    ///
    /// [`wait_connected`][Self::wait_connected] runs a DHCP client internally
    /// and returns the assigned IP.
    /// For custom network stacks, call [`take_sta_device`][Self::take_sta_device]
    /// instead and drive `smoltcp` or `embassy-net` yourself.
    pub struct WiFiManager<'d> {
        controller: WifiController<'d>,
        sta_device: Option<WifiDevice<'d>>,
        power_save: WifiPowerSave,
    }

    impl<'d> WiFiManager<'d> {
        /// Initializes the heap, scheduler, radio, and Wi-Fi — then configures,
        /// starts, and begins association in a single call.
        ///
        /// # Prerequisites
        ///
        /// The application must call `esp_hal::init` before this method.
        /// `CpuClock::max()` is **required** — the Wi-Fi radio blob needs
        /// at least 80 MHz and works reliably only at the maximum clock.
        ///
        /// ```ignore
        /// let peripherals = esp_hal::init(
        ///     esp_hal::Config::default().with_cpu_clock(CpuClock::max()),
        /// );
        /// ```
        ///
        /// # What `init` does
        ///
        /// 1. Allocates a 72 KiB heap for the Wi-Fi radio stack
        /// 2. Starts the `esp-rtos` scheduler
        /// 3. Initializes the radio via `esp_radio::init()`
        /// 4. Creates the Wi-Fi controller and network interfaces
        /// 5. Configures STA mode with the SSID/password from `config`
        /// 6. Starts the Wi-Fi hardware and applies power-save settings
        /// 7. Initiates association (non-blocking)
        ///
        /// After `init` returns, call [`wait_connected`][Self::wait_connected]
        /// or poll [`is_connected`][WifiDriver::is_connected] to wait for
        /// association to complete.
        pub fn init(config: HalWifiConfig<'_>) -> Result<WiFiManager<'static>, WifiError> {
            // Set up the heap for Wi-Fi radio buffers.
            unsafe {
                esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
                    core::ptr::addr_of_mut!(WIFI_HEAP) as *mut u8,
                    WIFI_HEAP_SIZE,
                    esp_alloc::MemoryCapability::Internal.into(),
                ));
            }

            let timg = TimerGroup::new(config.timg0);
            let sw_ints = SoftwareInterruptControl::new(config.sw_interrupt);
            esp_rtos::start(timg.timer0, sw_ints.software_interrupt0);

            let radio_init = esp_radio::init().map_err(|e| {
                log::error!("Radio init failed: {:?}", e);
                WifiError::RadioInitFailed
            })?;

            // Leak the radio controller — it's a one-time init that must outlive
            // the WifiController, and bare-metal firmware never deallocates it.
            let radio_ref: &'static _ = alloc::boxed::Box::leak(alloc::boxed::Box::new(radio_init));

            let (controller, interfaces) =
                esp_radio::wifi::new(radio_ref, config.wifi, Default::default())
                    .map_err(WifiError::Driver)?;

            let mut manager = WiFiManager {
                controller,
                sta_device: Some(interfaces.sta),
                power_save: config.power_save,
            };

            manager.configure(config.ssid, config.password)?;
            manager.start()?;
            manager.connect()?;

            Ok(manager)
        }

        /// Creates a Wi-Fi manager from pre-initialized `esp-radio` objects.
        ///
        /// Use this when you need to manage the scheduler or radio lifecycle
        /// separately (e.g. sharing `esp-rtos` with other subsystems).
        /// For the common case, prefer [`init`][Self::init].
        pub fn new(
            controller: WifiController<'d>,
            interfaces: Interfaces<'d>,
            power_save: WifiPowerSave,
        ) -> Self {
            Self {
                controller,
                sta_device: Some(interfaces.sta),
                power_save,
            }
        }

        /// Blocks until the Wi-Fi station is associated and an IP address is
        /// assigned via DHCP.
        ///
        /// Returns the assigned IPv4 address, or `Err` if the timeout elapses
        /// before association + DHCP completes.
        ///
        /// Internally runs a smoltcp DHCP client on the STA `WifiDevice`.
        pub fn wait_connected(
            &mut self,
            timeout_ms: u64,
        ) -> Result<smoltcp::wire::Ipv4Address, WifiError> {
            use smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
            use smoltcp::socket::dhcpv4;
            use smoltcp::wire::{EthernetAddress, HardwareAddress};

            let delay = esp_hal::delay::Delay::new();

            // Wait for L2 association first.
            let start = esp_hal::time::Instant::now();
            loop {
                if self.controller.is_connected().unwrap_or(false) {
                    log::info!("Wi-Fi associated");
                    break;
                }
                if start.elapsed().as_millis() >= timeout_ms {
                    log::error!("Wi-Fi association timeout after {} ms", timeout_ms);
                    return Err(WifiError::ConnectFailed);
                }
                delay.delay_millis(wifi_pure::POLL_INTERVAL_MS as u32);
            }

            // Run DHCP to obtain an IP address.
            let device = self.sta_device.as_mut().ok_or_else(|| {
                log::error!("STA device already taken");
                WifiError::ConfigureFailed
            })?;

            let mac = esp_radio::wifi::sta_mac();
            let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
            let mut iface = Interface::new(IfaceConfig::new(hw_addr), device, smoltcp_now());

            let mut socket_storage = [smoltcp::iface::SocketStorage::EMPTY; 1];
            let mut sockets = SocketSet::new(&mut socket_storage[..]);
            let dhcp_handle = sockets.add(dhcpv4::Socket::new());

            loop {
                if start.elapsed().as_millis() >= timeout_ms {
                    log::error!("DHCP timeout after {} ms", timeout_ms);
                    return Err(WifiError::ConnectFailed);
                }

                iface.poll(smoltcp_now(), device, &mut sockets);

                let socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
                if let Some(dhcpv4::Event::Configured(config)) = socket.poll() {
                    let ip = config.address.address();
                    log::info!("DHCP assigned IP: {}", ip);
                    return Ok(ip);
                }

                delay.delay_millis(50);
            }
        }

        /// Convenience alias for [`wait_connected`][Self::wait_connected], matching
        /// the [`WiFiManager::get_ip`][rustyfarian_esp_idf_wifi] API.
        pub fn get_ip(&mut self, timeout_ms: u64) -> Result<smoltcp::wire::Ipv4Address, WifiError> {
            self.wait_connected(timeout_ms)
        }

        /// Takes ownership of the STA `WifiDevice` for use with a custom
        /// `smoltcp` or `embassy-net` stack.
        ///
        /// This is a **single-use** method — returns `None` on subsequent calls.
        /// After taking the device, [`wait_connected`][Self::wait_connected] and
        /// [`get_ip`][Self::get_ip] will fail because they need the device for DHCP.
        /// Use this only when you want full control over the network stack.
        pub fn take_sta_device(&mut self) -> Option<WifiDevice<'d>> {
            self.sta_device.take()
        }

        /// Maps the portable power-save enum to the esp-radio variant.
        ///
        /// Only modem-based power saving is supported (radio sleep between
        /// DTIM beacons). Deep sleep and AP-specific power management are
        /// not handled by this driver.
        fn map_power_save(ps: WifiPowerSave) -> PowerSaveMode {
            match ps {
                WifiPowerSave::None => PowerSaveMode::None,
                WifiPowerSave::MinModem => PowerSaveMode::Minimum,
                WifiPowerSave::MaxModem => PowerSaveMode::Maximum,
            }
        }
    }

    impl<'d> WifiDriver for WiFiManager<'d> {
        type Error = WifiError;

        fn configure(&mut self, ssid: &str, password: &str) -> Result<(), WifiError> {
            validate_ssid(ssid).map_err(|_| WifiError::ConfigureFailed)?;
            validate_password(password).map_err(|_| WifiError::ConfigureFailed)?;

            let client_config = ClientConfig::default()
                .with_ssid(ssid.into())
                .with_password(password.into());

            self.controller
                .set_config(&ModeConfig::Client(client_config))
                .map_err(WifiError::Driver)?;

            log::info!("Wi-Fi configured (SSID len={}, bare-metal)", ssid.len());
            Ok(())
        }

        fn start(&mut self) -> Result<(), WifiError> {
            self.controller.start().map_err(WifiError::Driver)?;

            let ps = Self::map_power_save(self.power_save);
            if let Err(e) = self.controller.set_power_saving(ps) {
                log::warn!("Failed to set power save mode (non-fatal): {:?}", e);
            }

            log::info!("Wi-Fi started, power save: {:?}", self.power_save);
            Ok(())
        }

        fn connect(&mut self) -> Result<(), WifiError> {
            self.controller.connect().map_err(WifiError::Driver)?;
            log::info!("Wi-Fi connect initiated");
            Ok(())
        }

        fn disconnect(&mut self) -> Result<(), WifiError> {
            self.controller.disconnect().map_err(WifiError::Driver)?;
            log::info!("Wi-Fi disconnected");
            Ok(())
        }

        fn is_connected(&self) -> Result<bool, WifiError> {
            match self.controller.is_connected() {
                Ok(connected) => Ok(connected),
                Err(esp_radio::wifi::WifiError::Disconnected) => Ok(false),
                Err(e) => Err(WifiError::Driver(e)),
            }
        }
    }
}

#[cfg(any(feature = "esp32c6", feature = "esp32c3"))]
pub use driver::{HalWifiConfig, WiFiConfigExt, WiFiManager, WifiError};

// ─── Stub fallback (no chip feature — host compilation) ─────────────────────

#[cfg(not(any(feature = "esp32c6", feature = "esp32c3")))]
mod stub {
    use wifi_pure::WifiDriver;

    /// Error type for [`WiFiManager`] operations.
    #[derive(Debug)]
    pub enum WifiError {
        /// Operation not yet implemented on this platform.
        NotSupported,
    }

    impl core::fmt::Display for WifiError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::NotSupported => write!(f, "Wi-Fi not supported on this platform"),
            }
        }
    }

    /// Bare-metal Wi-Fi manager stub (no chip feature active).
    ///
    /// All methods return [`WifiError::NotSupported`].
    /// Enable a chip feature (e.g. `esp32c6`) to get the real implementation.
    pub struct WiFiManager;

    impl WiFiManager {
        /// Create a stub Wi-Fi manager.
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for WiFiManager {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WifiDriver for WiFiManager {
        type Error = WifiError;

        fn configure(&mut self, _ssid: &str, _password: &str) -> Result<(), Self::Error> {
            Err(WifiError::NotSupported)
        }

        fn start(&mut self) -> Result<(), Self::Error> {
            Err(WifiError::NotSupported)
        }

        fn connect(&mut self) -> Result<(), Self::Error> {
            Err(WifiError::NotSupported)
        }

        fn disconnect(&mut self) -> Result<(), Self::Error> {
            Err(WifiError::NotSupported)
        }

        fn is_connected(&self) -> Result<bool, Self::Error> {
            Err(WifiError::NotSupported)
        }
    }
}

#[cfg(not(any(feature = "esp32c6", feature = "esp32c3")))]
pub use stub::{WiFiManager, WifiError};
