//! Wi-Fi driver for ESP-HAL projects (bare-metal, no_std).
//!
//! Provides [`WiFiManager`], a [`WifiDriver`] implementation that
//! connects to a WPA2 access point using `esp-radio` on bare-metal targets.
//!
//! # Quick start
//!
//! ```ignore
//! use rustyfarian_esp_hal_wifi::{WiFiManager, WiFiConfig, WiFiConfigExt};
//!
//! let peripherals = esp_hal::init(esp_hal::Config::default());
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .with_peripherals(peripherals.TIMG0, peripherals.SW_INTERRUPT, peripherals.WIFI);
//! let mut wifi = WiFiManager::init(config)?;
//! let ip = wifi.wait_connected(30_000)?;
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

// Re-export StatusLed, SimpleLed, and NoLed from led_effects for convenience
pub use led_effects::{NoLed, SimpleLed, StatusLed};

// ─── Real implementation (behind chip feature gates) ────────────────────────

#[cfg(any(feature = "esp32c6", feature = "esp32c3"))]
mod driver {
    extern crate alloc;

    use esp_hal::interrupt::software::SoftwareInterruptControl;
    use esp_hal::timer::timg::TimerGroup;
    use esp_radio::wifi::{
        ClientConfig, Interfaces, ModeConfig, PowerSaveMode, WifiController, WifiDevice,
    };
    use led_effects::{NoLed, PulseEffect, StatusLed};
    use rgb::RGB8;
    use wifi_pure::{validate_password, validate_ssid, WiFiConfig, WifiDriver, WifiPowerSave};

    // Mirrored from rustyfarian_network_pure::status_colors (which is not no_std).
    const WIFI_CONNECTING: (u8, u8, u8) = (0, 0, 255);
    const CONNECTED: (u8, u8, u8) = (0, 20, 0);
    const ERROR: (u8, u8, u8) = (255, 0, 0);

    /// Minimum heap size for the Wi-Fi radio stack (bytes).
    const WIFI_HEAP_SIZE: usize = 72 * 1024;

    /// Heap backing store for the Wi-Fi radio stack.
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
    /// Built from a [`WiFiConfig`] via [`with_peripherals`][WiFiConfigExt::with_peripherals],
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
    /// The `S` parameter controls LED status feedback during connection:
    /// use [`NoLed`] (the default) for headless operation, or pass any
    /// [`StatusLed`] implementation to [`init_with_led`][Self::init_with_led].
    pub struct WiFiManager<'d, S: StatusLed = NoLed> {
        controller: WifiController<'d>,
        sta_device: Option<WifiDevice<'d>>,
        power_save: WifiPowerSave,
        led: S,
    }

    impl WiFiManager<'_, NoLed> {
        /// Initializes the heap, scheduler, radio, and Wi-Fi — then configures,
        /// starts, and begins association in a single call.
        ///
        /// Uses [`NoLed`] for status feedback (no visual output).
        /// For LED feedback during connection, use [`init_with_led`][WiFiManager::init_with_led].
        pub fn init(config: HalWifiConfig<'_>) -> Result<WiFiManager<'static, NoLed>, WifiError> {
            Self::init_inner(config, NoLed)
        }
    }

    impl<'d, S: StatusLed> WiFiManager<'d, S>
    where
        S::Error: core::fmt::Debug,
    {
        /// Initializes Wi-Fi with LED status feedback during connection.
        ///
        /// Identical to [`init`][WiFiManager::init] but stores an LED driver that
        /// [`wait_connected`][Self::wait_connected] uses for visual feedback:
        /// blue pulse while associating, red pulse on timeout, dim green on success.
        pub fn init_with_led(
            config: HalWifiConfig<'_>,
            led: S,
        ) -> Result<WiFiManager<'static, S>, WifiError> {
            Self::init_inner(config, led)
        }

        fn init_inner(
            config: HalWifiConfig<'_>,
            led: S,
        ) -> Result<WiFiManager<'static, S>, WifiError> {
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
                led,
            };

            manager.configure(config.ssid, config.password)?;
            manager.start()?;
            manager.connect()?;

            Ok(manager)
        }

        /// Creates a Wi-Fi manager from pre-initialized `esp-radio` objects.
        pub fn new(
            controller: WifiController<'d>,
            interfaces: Interfaces<'d>,
            power_save: WifiPowerSave,
            led: S,
        ) -> Self {
            Self {
                controller,
                sta_device: Some(interfaces.sta),
                power_save,
                led,
            }
        }

        /// Blocks until the Wi-Fi station is associated and an IP address is
        /// assigned via DHCP.
        ///
        /// If an LED was provided via [`init_with_led`][Self::init_with_led],
        /// this method pulses blue during association, briefly pulses red on
        /// timeout, and sets dim green on success.
        pub fn wait_connected(
            &mut self,
            timeout_ms: u64,
        ) -> Result<smoltcp::wire::Ipv4Address, WifiError> {
            use smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
            use smoltcp::socket::dhcpv4;
            use smoltcp::wire::{EthernetAddress, HardwareAddress};

            let delay = esp_hal::delay::Delay::new();
            let mut pulse = PulseEffect::new();

            // Wait for L2 association first.
            let start = esp_hal::time::Instant::now();
            loop {
                if self.controller.is_connected().unwrap_or(false) {
                    log::info!("Wi-Fi associated");
                    break;
                }
                if start.elapsed().as_millis() >= timeout_ms {
                    log::error!("Wi-Fi association timeout after {} ms", timeout_ms);

                    for _ in 0..20 {
                        if let Err(e) = self.led.set_color(pulse.update(ERROR)) {
                            log::warn!("LED error: {:?}", e);
                        }
                        delay.delay_millis(50);
                    }

                    return Err(WifiError::ConnectFailed);
                }

                if let Err(e) = self.led.set_color(pulse.update(WIFI_CONNECTING)) {
                    log::warn!("LED error: {:?}", e);
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

                    for _ in 0..20 {
                        if let Err(e) = self.led.set_color(pulse.update(ERROR)) {
                            log::warn!("LED error: {:?}", e);
                        }
                        delay.delay_millis(50);
                    }

                    return Err(WifiError::ConnectFailed);
                }

                iface.poll(smoltcp_now(), device, &mut sockets);

                let socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
                if let Some(dhcpv4::Event::Configured(config)) = socket.poll() {
                    let ip = config.address.address();
                    log::info!("DHCP assigned IP: {}", ip);

                    if let Err(e) = self.led.set_color(RGB8::from(CONNECTED)) {
                        log::warn!("LED error: {:?}", e);
                    }

                    return Ok(ip);
                }

                if let Err(e) = self.led.set_color(pulse.update(WIFI_CONNECTING)) {
                    log::warn!("LED error: {:?}", e);
                }
                delay.delay_millis(50);
            }
        }

        /// Convenience alias for [`wait_connected`][Self::wait_connected].
        pub fn get_ip(&mut self, timeout_ms: u64) -> Result<smoltcp::wire::Ipv4Address, WifiError> {
            self.wait_connected(timeout_ms)
        }

        /// Takes ownership of the STA `WifiDevice` for use with a custom
        /// `smoltcp` or `embassy-net` stack.
        ///
        /// This is a **single-use** method — returns `None` on subsequent calls.
        pub fn take_sta_device(&mut self) -> Option<WifiDevice<'d>> {
            self.sta_device.take()
        }

        fn map_power_save(ps: WifiPowerSave) -> PowerSaveMode {
            match ps {
                WifiPowerSave::None => PowerSaveMode::None,
                WifiPowerSave::MinModem => PowerSaveMode::Minimum,
                WifiPowerSave::MaxModem => PowerSaveMode::Maximum,
            }
        }
    }

    impl<'d, S: StatusLed> WifiDriver for WiFiManager<'d, S>
    where
        S::Error: core::fmt::Debug,
    {
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

    #[derive(Debug)]
    pub enum WifiError {
        NotSupported,
    }

    impl core::fmt::Display for WifiError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "Wi-Fi not supported on this platform")
        }
    }

    pub struct WiFiManager;

    impl WiFiManager {
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
