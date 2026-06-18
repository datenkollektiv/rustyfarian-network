//! Bare-metal SoftAP captive-portal provisioning example for ESP32-C3.
//!
//! Demonstrates the [`SchemaProfile::WifiMqttDevice`] host contract on a
//! bare-metal target:
//!
//! 1. Open the [`ProvisioningStore`] and check if credentials already exist.
//! 2. If already provisioned, log the stored config (lengths only, no secrets)
//!    and idle — a real application would reboot into its normal boot path.
//! 3. Otherwise, bring up the SoftAP captive portal via
//!    [`ProvisioningBuilder::start`].
//! 4. After the portal commits, log the committed profile and idle — a real
//!    application would call `esp_hal::reset::software_reset()` here so the
//!    device reboots into its normal STA + MQTT boot.
//!
//! The library never reboots or erases; those decisions belong to the host.
//!
//! # INTEGRATOR: HTTP task stack requirement
//!
//! The HTTP portal task requires at least **14 KiB** of stack.
//! The steady-state frame is ~8.3 KiB (`req_buf` 2048 B + `resp_buf` 6144 B
//! + executor overhead); a `POST /save` request transiently adds ~4.6 KiB for
//! `ProvisioningStore::save`'s encode and read buffers, raising the peak to
//! ~13 KiB.
//! The `esp-rtos` 0.3.0 thread-mode embassy executor runs **all** async tasks
//! on a single shared stack — there is no per-task stack size knob exposed by
//! `#[embassy_executor::task]` or `#[esp_rtos::main]`.
//! Size the main-thread stack via the linker script stack symbol
//! (`_stack_size`) or via `CONFIG_ESP_MAIN_TASK_STACK_SIZE` when using an
//! IDF-paired build system.
//! A runtime `log::warn!` below confirms this requirement at startup.
//!
//! # Flash-partition offset
//!
//! `FLASH_PARTITION_OFFSET` is set to `0x300000` (3 MiB).
//! You **must** align this to a 4 KiB boundary in your partition table.
//! The store occupies two adjacent 4 KiB sectors (8 KiB total).
//! A safe default for an ESP32-C3 with 4 MiB flash and the standard IDF
//! single-app partition layout is anywhere in the `storage` region —
//! check your `partitions.csv` before flashing.
//!
//! # Build and flash
//!
//! ```sh
//! just build-example hal_c3_provision_mqtt
//! just flash hal_c3_provision_mqtt
//! ```
//!
//! Override the AP SSID prefix at build time:
//!
//! ```sh
//! PORTAL_SSID_PREFIX="MyDevice" just build-example hal_c3_provision_mqtt
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use rustyfarian_esp_hal_provisioning::{
    PortalConfig, ProvisioningBuilder, ProvisioningEvent, ProvisioningOutcome, ProvisioningStore,
    SchemaProfile,
};
use rustyfarian_esp_hal_wifi::{ApConfig, ApConfigExt, WiFiManager};

esp_bootloader_esp_idf::esp_app_desc!();

/// SoftAP SSID prefix.
/// The last two bytes of the AP MAC address are appended to form the full SSID.
/// Override at build time: `PORTAL_SSID_PREFIX="MyDevice" just build-example hal_c3_provision_mqtt`
const PORTAL_SSID_PREFIX: &str = match option_env!("PORTAL_SSID_PREFIX") {
    Some(s) => s,
    None => "RustyFarian",
};

/// Human-readable device name shown in the portal header.
const DEVICE_NAME: &str = match option_env!("DEVICE_NAME") {
    Some(s) => s,
    None => "c3-provision-demo",
};

/// Firmware version string shown in the portal header.
const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Flash offset for the provisioning store (two 4 KiB sectors = 8 KiB total).
///
/// Set to `0x300000` (3 MiB) as a documented placeholder.
/// INTEGRATOR: align this to a 4 KiB boundary in your `partitions.csv`.
/// Ensure that `FLASH_PARTITION_OFFSET + 8192` does not cross a partition
/// boundary and that no other firmware region overlaps this area.
const FLASH_PARTITION_OFFSET: u32 = 0x0030_0000;

/// Total size passed to `ProvisioningStore::open`.
/// Must be >= 8192 (two 4 KiB sectors).
const FLASH_PARTITION_SIZE: u32 = 8192;

/// Lifecycle-event callback.
/// Runs synchronously inside the HTTP task and must not block.
fn on_event(event: ProvisioningEvent) {
    match event {
        ProvisioningEvent::ClientConnected { mac } => match mac {
            Some(m) => log::info!(
                "Portal: phone joined AP (mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
                m[0],
                m[1],
                m[2],
                m[3],
                m[4],
                m[5]
            ),
            None => log::info!("Portal: phone joined AP (mac not yet exposed by esp-radio 0.18)"),
        },
        ProvisioningEvent::ClientDisconnected { mac } => match mac {
            Some(m) => log::info!(
                "Portal: phone left AP (mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
                m[0],
                m[1],
                m[2],
                m[3],
                m[4],
                m[5]
            ),
            None => log::info!("Portal: phone left AP (mac not yet exposed by esp-radio 0.18)"),
        },
        ProvisioningEvent::SubmissionAccepted => {
            log::info!("Portal: submission accepted");
        }
        ProvisioningEvent::SubmissionRejected => {
            log::warn!("Portal: submission rejected (nonce or validation failure)");
        }
        ProvisioningEvent::Committed => {
            log::info!("Portal: credentials committed to flash");
        }
        ProvisioningEvent::FactoryResetRequested => {
            log::warn!("Portal: factory reset requested");
        }
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C3 has contiguous SRAM — a single 72 KiB region is sufficient for
    // the Wi-Fi radio buffers and general-purpose allocations.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    println!("hal_c3_provision_mqtt starting");

    // INTEGRATOR: the HTTP portal task (http_task) peaks at ~13 KiB during a
    // POST /save request (steady-state 8.3 KiB + ProvisioningStore::save 4.6 KiB
    // transient).  esp-rtos 0.3.0 thread-mode embassy shares a single stack for
    // all async tasks; there is no per-task stack size knob.  Size the
    // main-thread stack to at least 14 KiB via the linker script (_stack_size
    // symbol) or CONFIG_ESP_MAIN_TASK_STACK_SIZE (IDF-paired build).
    log::warn!(
        "HTTP portal requires >=14 KiB main-thread stack (POST /save peak ~13 KiB). \
         Size via linker _stack_size or CONFIG_ESP_MAIN_TASK_STACK_SIZE."
    );

    // Open the flash store — `esp_storage::FlashStorage` implements `NorFlash`
    // with a 4 KiB erase size.  The `FLASH` peripheral token is required by
    // esp-storage 0.9.0 for singleton ownership; it is not used at runtime.
    let flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let mut store = ProvisioningStore::open(flash, FLASH_PARTITION_OFFSET, FLASH_PARTITION_SIZE)
        .expect("provisioning store open");

    // Check whether the device is already provisioned.
    // If so, log the stored config and idle — a real application would reboot
    // into its normal STA + MQTT boot path here.
    if store.is_provisioned().unwrap_or(false) {
        match store.load() {
            Ok(Some(cfg)) => {
                log::info!(
                    "Already provisioned — ssid len={}, device_name={}, profile={:?}",
                    cfg.wifi_ssid().len(),
                    cfg.device_name(),
                    cfg.profile(),
                );
                if let Some(mqtt) = cfg.mqtt() {
                    log::info!(
                        "MQTT target: host len={}, port={}",
                        mqtt.host().len(),
                        mqtt.port(),
                    );
                }
            }
            Ok(None) => {
                log::warn!("Store reports provisioned but load returned None; re-provisioning");
            }
            Err(e) => {
                log::warn!("Store load error: {:?}; re-provisioning", e);
            }
        }
        // A real application reboots into its normal STA + MQTT boot here.
        // That normal path brings up Wi-Fi via `WiFiManager::init_async`, which
        // calls `esp_rtos::start` and installs the scheduler's time driver.
        // This provisioned branch performs no Wi-Fi bring-up, so the scheduler
        // is never started — and the esp-rtos async executor panics the moment
        // any task `.await`s and it tries to park on the absent time driver
        // (`Timer::after` and even `core::future::pending().await` both trip it).
        // We therefore halt with a non-async spin loop rather than awaiting.
        println!(
            "Already provisioned — halting (real app would reboot into normal STA + MQTT mode)"
        );
        loop {
            core::hint::spin_loop();
        }
    }

    // Not yet provisioned — bring up the SoftAP captive portal.
    // The AP is open (no WPA2 password); the nonce in the form is the access
    // control mechanism.
    let ap_config = ApConfig::open(PORTAL_SSID_PREFIX)
        .with_channel(1)
        .with_ap_peripherals(
            peripherals.TIMG0,
            peripherals.SW_INTERRUPT,
            peripherals.WIFI,
        );

    let softap = WiFiManager::init_softap_async(ap_config).expect("SoftAP init");

    // The `store` declared above is still owned here (we only enter this branch
    // when `is_provisioned()` returned false, so no move occurred).
    // Pass it directly to `ProvisioningBuilder::start`; the builder erases the
    // flash type behind a trait object and takes ownership.

    // Obtain a hardware RNG for the per-session CSRF nonce.
    // In esp-hal 1.1.0, `Rng::new()` takes no peripheral argument.
    let rng = esp_hal::rng::Rng::new();

    let portal_config = PortalConfig {
        ssid_prefix: PORTAL_SSID_PREFIX,
        ap_password: None,
        channel: 1,
        device_name: DEVICE_NAME,
        firmware_version: FIRMWARE_VERSION,
        profile: SchemaProfile::WifiMqttDevice,
    };

    let session = ProvisioningBuilder::new(portal_config)
        .on_event(on_event)
        .start(spawner, softap, store, rng)
        .expect("provisioning start");

    let ip = session.ap_ip();
    println!(
        "Provisioning AP up — open http://{}.{}.{}.{}/ to configure",
        ip[0], ip[1], ip[2], ip[3]
    );

    match session.wait_outcome().await {
        ProvisioningOutcome::Committed(cfg) => {
            log::info!(
                "Credentials committed (ssid len={}, profile={:?}) — \
                 host should reboot to apply",
                cfg.wifi_ssid().len(),
                cfg.profile(),
            );
            // The library never reboots; the host decides.
            // A real application calls `esp_hal::reset::software_reset()` here.
            println!("Committed — idling (real app would reboot into normal STA + MQTT mode)");
            loop {
                Timer::after(Duration::from_secs(10)).await;
            }
        }
        ProvisioningOutcome::FactoryResetRequested => {
            log::warn!("Factory reset requested — host should erase the store and reboot");
            // A real application calls `store.erase_all()` then reboots here.
            println!("Factory reset requested — idling (real app would erase and reboot)");
            loop {
                Timer::after(Duration::from_secs(10)).await;
            }
        }
        ProvisioningOutcome::HostAborted => {
            log::info!("Session host-aborted");
            loop {
                Timer::after(Duration::from_secs(10)).await;
            }
        }
    }
}
