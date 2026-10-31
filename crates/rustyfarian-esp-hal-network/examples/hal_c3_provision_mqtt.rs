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
//! 4. After the portal commits, log the committed profile and restart via
//!    `esp_hal::system::software_reset()` so the device reboots into its
//!    normal STA + MQTT boot (the example decides this, not the library).
//!
//! The library never reboots or erases; those decisions belong to the host.
//!
//! # INTEGRATOR: HTTP task stack requirement
//!
//! The HTTP portal task requires at least **24 KiB** of stack.
//! The steady-state frame is ~8.3 KiB (`req_buf` 2048 B + `resp_buf` 6144 B
//! + executor overhead); a `POST /save` request transiently adds ~12.5 KiB
//! (analytic upper bound, not measured) for `ProvisioningStore::save`'s
//! encode and read buffers plus the portal's parsed config, raising the peak
//! to ~20.5 KiB.
//! The `esp-rtos` 0.4.0 thread-mode embassy executor runs **all** async tasks
//! on a single shared stack — there is no per-task stack size knob exposed by
//! `#[embassy_executor::task]` or `#[esp_rtos::main]`.
//! esp-hal has no stack-size setting either: the main stack is the RAM left
//! after `.data`, `.bss` and the heap, so grow it by trimming the heap
//! (`heap_allocator!`) and static buffers.
//! Enforce the floor at link time with
//! `ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM = "24576"` in the `[env]` table
//! of `.cargo/config.toml`; the link fails when less than 24 KiB remains.
//! `CONFIG_ESP_MAIN_TASK_STACK_SIZE` is an ESP-IDF setting and has no effect
//! on this tier.
//! A runtime `log::warn!` below confirms this requirement at startup.
//!
//! # Flash-partition offset
//!
//! `FLASH_PARTITION_OFFSET` is set to `0x300000` (3 MiB) as a placeholder,
//! and `just flash` installs no partition table of its own.
//! In espflash's default 4 MiB table the `factory` app partition spans
//! `0x10000`–`0x400000`, so this offset lies inside the running firmware's
//! partition; nothing keeps the app image and the store apart.
//! Before copying this example, add a dedicated `data` partition for the
//! store to your `partitions.csv` (ADR 015: 4 KiB aligned, two sectors =
//! 8 KiB), flash it with `espflash --partition-table`, and point the offset
//! there.
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
//!
//! Replace the whole SSID (no MAC suffix) at build time:
//!
//! ```sh
//! PORTAL_SSID_OVERRIDE="MyClock" just build-example hal_c3_provision_mqtt
//! ```
//!
//! Protect the portal AP with WPA2 at build time (8 to 63 bytes):
//!
//! ```sh
//! PROVISION_AP_PSK="provision-me" just build-example hal_c3_provision_mqtt
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use rustyfarian_esp_hal_network::provisioning::{
    DeviceName, PortalConfig, PortalDefaults, ProvisioningBuilder, ProvisioningEvent,
    ProvisioningOutcome, ProvisioningStore, SchemaProfile,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// SoftAP SSID prefix.
/// Unless `PORTAL_SSID_OVERRIDE` is set, the portal appends the last two bytes
/// of the AP MAC address, giving the full SSID `{prefix}-{MAC}`.
/// Override at build time: `PORTAL_SSID_PREFIX="MyDevice" just build-example hal_c3_provision_mqtt`
const PORTAL_SSID_PREFIX: &str = match option_env!("PORTAL_SSID_PREFIX") {
    Some(s) => s,
    None => "Rustyfarian",
};

/// Optional full SoftAP SSID that replaces `{prefix}-{MAC}` when set.
/// Override at build time: `PORTAL_SSID_OVERRIDE="MyClock" just build-example hal_c3_provision_mqtt`
const PORTAL_SSID_OVERRIDE: Option<&str> = option_env!("PORTAL_SSID_OVERRIDE");

/// Optional WPA2 password for the portal AP; unset runs an open AP.
/// Set at build time: `PROVISION_AP_PSK="provision-me" just build-example hal_c3_provision_mqtt`.
/// A `fn` rather than a `const`, like `mqtt_password()` in the MQTT examples:
/// CodeQL reports a `const` holding a credential as a hard-coded value.
fn ap_psk() -> Option<&'static str> {
    option_env!("PROVISION_AP_PSK")
}

/// Device-name prefix.
/// The portal appends the AP-MAC suffix (`-xxxx`), so two boards get distinct
/// names and MQTT client IDs.
const DEVICE_NAME: &str = match option_env!("DEVICE_NAME") {
    Some(s) => s,
    None => "c3-provision-demo",
};

/// Firmware version string shown in the portal header.
const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Flash offset for the provisioning store (two 4 KiB sectors = 8 KiB total).
///
/// Set to `0x300000` (3 MiB) as a documented placeholder; in espflash's
/// default 4 MiB table this falls inside the `factory` app partition.
/// INTEGRATOR: give the store its own 4 KiB-aligned `data` partition in
/// `partitions.csv` and point this constant at it (see the module docs).
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
            None => log::info!("Portal: phone joined AP (mac unavailable)"),
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
            None => log::info!("Portal: phone left AP (mac unavailable)"),
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

    // INTEGRATOR: the HTTP portal task (http_task) peaks at ~20.5 KiB during a
    // POST /save request (steady-state 8.3 KiB + ~12.5 KiB transient for
    // ProvisioningStore::save and the parsed config).  esp-rtos 0.4.0 thread-mode
    // embassy shares a single stack for all async tasks; there is no per-task
    // stack size knob.  The main stack is the RAM left after .data, .bss and the
    // heap: keep at least 24 KiB free and enforce it at link time with
    // ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM=24576 (see the module docs).
    log::warn!(
        "HTTP portal requires >=24 KiB main-thread stack (POST /save peak <=~20.5 KiB, analytic bound). \
         Enforce with ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM=24576."
    );

    // Open the flash store — `esp_storage::FlashStorage` implements `NorFlash`
    // with a 4 KiB erase size.  The `FLASH` peripheral token is required by
    // esp-storage since 0.9.0 for singleton ownership; it is not used at runtime.
    let flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let mut store = ProvisioningStore::open(flash, FLASH_PARTITION_OFFSET, FLASH_PARTITION_SIZE)
        .expect("provisioning store open");

    // Check whether the device is already provisioned.
    // If so, log the stored config and idle — a real application would reboot
    // into its normal STA + MQTT boot path here.
    let provisioned = store.is_provisioned().unwrap_or_else(|e| {
        log::warn!("Store is_provisioned error: {:?}; re-provisioning", e);
        false
    });
    if provisioned {
        match store.load() {
            Ok(Some(cfg)) => {
                log::info!(
                    "Already provisioned — ssid len={}, device_name={}, profile={:?}, extras={}",
                    cfg.wifi_ssid().len(),
                    cfg.device_name(),
                    cfg.profile(),
                    cfg.extras().len(),
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

    // Not yet provisioned — start the SoftAP captive portal.
    // `start` owns the AP bring-up: it derives the SSID from the AP MAC, brings
    // up the AP (open unless `PROVISION_AP_PSK` is set; the nonce in the form
    // is the access control mechanism) and spawns the portal tasks.

    // The `store` declared above is still owned here (we only enter this branch
    // when `is_provisioned()` returned false, so no move occurred).
    // Pass it directly to `ProvisioningBuilder::start`; the builder erases the
    // flash type behind a trait object and takes ownership.

    // Obtain a hardware RNG for the per-session CSRF nonce.
    // Since esp-hal 1.1.0, `Rng::new()` takes no peripheral argument.
    let rng = esp_hal::rng::Rng::new();

    // Non-secret defaults seed the portal form on a fresh / factory-reset
    // device so testing does not mean retyping every field. Values come from
    // `.env` at build time (see `.env.example`). Secrets (`WIFI_PASS`,
    // `MQTT_PASS`) are intentionally never pre-filled and must be typed.
    let defaults = PortalDefaults {
        wifi_ssid: option_env!("WIFI_SSID").unwrap_or(""),
        mqtt_host: option_env!("MQTT_HOST").unwrap_or(""),
        mqtt_port: option_env!("MQTT_PORT").unwrap_or(""),
        mqtt_user: option_env!("MQTT_USER").unwrap_or(""),
        // `mqtt_client` stays blank so the client ID derives from the per-device
        // name; an operator can still type one into the portal.
        ota_url: option_env!("OTA_URL").unwrap_or(""),
        ..PortalDefaults::default()
    };

    let portal_config = PortalConfig {
        ssid_prefix: PORTAL_SSID_PREFIX,
        ssid_override: PORTAL_SSID_OVERRIDE,
        ap_password: ap_psk(),
        channel: 1,
        device_name: DeviceName::MacSuffixed(DEVICE_NAME),
        firmware_version: FIRMWARE_VERSION,
        profile: SchemaProfile::WifiMqttDevice,
        defaults,
    };

    let session = ProvisioningBuilder::new(portal_config)
        .on_event(on_event)
        .start(
            spawner,
            peripherals.TIMG0,
            peripherals.FROM_CPU_INTR0,
            peripherals.WIFI,
            store,
            rng,
        )
        .expect("provisioning start");

    let mac = session.ap_mac();
    println!("Provisioning AP MAC suffix: {:02X}{:02X}", mac[4], mac[5]);
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
            // The library never reboots; the host decides. Give the log a
            // moment to drain, then restart into the normal STA + MQTT boot.
            println!("Committed — restarting into normal boot");
            Timer::after(Duration::from_millis(200)).await;
            esp_hal::system::software_reset();
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
