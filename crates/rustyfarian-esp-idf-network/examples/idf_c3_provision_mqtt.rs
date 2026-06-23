//! SoftAP captive-portal provisioning example for ESP32-C3 using the
//! Wi-Fi + MQTT profile.
//!
//! Demonstrates the [`SchemaProfile::WifiMqttDevice`] host contract using the
//! `WifiMqttBoot` helper:
//!
//! 1. [`WifiMqttBoot::load`] opens the NVS store and returns a ready-to-borrow
//!    config bundle when the device is already provisioned under the
//!    `WifiMqttDevice` profile.
//! 2. If not provisioned, an indicator thread is spawned (placeholder for a
//!    WS2812/LED animation), then [`run_wifi_mqtt_portal`] starts the SoftAP
//!    captive portal and blocks until a submission is committed, the factory-reset
//!    button is pressed, or the portal times out.
//! 3. On every portal exit the indicator thread is cancelled and joined BEFORE
//!    calling `restart()` — this guarantees no WS2812 SPI/RMT transfer is
//!    left mid-frame when the device reboots.
//!    On `FactoryResetRequested` the NVS provisioning namespace is erased (via
//!    [`ProvisioningStore::erase_all`]) after the join, then the device restarts
//!    so it re-enters the portal on the next boot.
//!
//! The library never reboots or erases on its own; every
//! `esp_idf_svc::hal::reset::restart()` call and the
//! `ProvisioningStore::erase_all()` call below live in this **example**,
//! not the crate.
//!
//! # Build and flash
//!
//! ```sh
//! PROVISION_AP_PSK="provision-me" just build-example idf_c3_provision_mqtt
//! ```
//!
//! ```sh
//! just flash idf_c3_provision_mqtt
//! ```
//!
//! # Required `sdkconfig.defaults`
//!
//! OS captive-portal browsers send request headers larger than the ESP-IDF
//! httpd default (512 B), so a standalone consumer MUST raise the limit in its
//! workspace-root `sdkconfig.defaults` — otherwise the portal GET is rejected
//! with `431 Request Header Fields Too Large` and the page never renders:
//!
//! ```text
//! CONFIG_HTTPD_MAX_REQ_HDR_LEN=2048
//! ```
//!
//! This workspace already sets it; the `2048` mirrors the esp-hal portal's
//! request-size cap. After changing `sdkconfig.defaults`, clean the
//! `esp-idf-sys-*` build dir (or `cargo clean`) so CMake reconfigures.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use rustyfarian_esp_idf_network::provisioning::{
    run_wifi_mqtt_portal, BootConfig, PortalConfig, PortalOutcome, ProvisioningEvent,
    ProvisioningStore, SchemaProfile, WifiMqttBoot, WifiMqttLoadOutcome,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Optional WPA2 password for the provisioning AP. Without it the AP is open.
const AP_PSK: Option<&str> = option_env!("PROVISION_AP_PSK");

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── Step 1: try to load a provisioned WifiMqttDevice config ─────────────
    let boot = match WifiMqttBoot::load(nvs.clone())? {
        WifiMqttLoadOutcome::Ready(b) => b,

        WifiMqttLoadOutcome::NotProvisioned => {
            // ── Step 2: spawn indicator thread before the portal ─────────────
            //
            // The cancel flag is shared between this thread and the outcome
            // handler below.  A real consumer drives its WS2812 / LED here,
            // e.g. reacting to `on_event` pulses; here we use a placeholder
            // log loop so the pattern compiles on any target without a
            // hardware LED dep.
            let indicator_cancel = Arc::new(AtomicBool::new(false));
            let indicator_cancel_clone = Arc::clone(&indicator_cancel);
            let indicator = std::thread::Builder::new()
                .name("indicator".into())
                .stack_size(2048)
                .spawn(move || {
                    // A real consumer drives its WS2812/LED here, e.g. off on_event.
                    while !indicator_cancel_clone.load(Ordering::Relaxed) {
                        log::debug!("indicator: portal running…");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    log::debug!("indicator: cancelled, exiting");
                })
                .context("failed to spawn indicator thread")?;

            // ── Step 3: run the captive portal ───────────────────────────────
            let ap_password = match AP_PSK {
                Some(psk) => Some(psk),
                None => {
                    log::warn!(
                        "PROVISION_AP_PSK not set — running an OPEN provisioning AP. \
                         Set PROVISION_AP_PSK at build time for a WPA2-protected portal."
                    );
                    None
                }
            };

            // Retain a clone of `nvs` before moving it into `run_wifi_mqtt_portal`
            // so the factory-reset arm can open the store for erasure.
            let nvs_for_erase = nvs.clone();

            let boot_config = BootConfig {
                portal: PortalConfig {
                    ssid_prefix: "Rustyfarian",
                    ap_password,
                    channel: 1,
                    device_name: "c3-mqtt-demo",
                    firmware_version: env!("CARGO_PKG_VERSION"),
                    profile: SchemaProfile::WifiMqttDevice,
                },
                portal_timeout: Some(Duration::from_secs(600)),
                on_event: Some(Arc::new(|event: ProvisioningEvent| {
                    // A real consumer would nudge the indicator here, e.g. change
                    // colour on ClientConnected, solid green on Committed, etc.
                    log::info!("Provisioning event: {event:?}");
                })),
            };

            // Capture the Result WITHOUT `?` so the cleanup below runs even on
            // an operational error — an early return here would orphan the
            // indicator thread, leaving it running indefinitely.
            let result = run_wifi_mqtt_portal(peripherals.modem, sys_loop, nvs, boot_config);

            // ── Step 4: cancel + join before restart (and before propagating) ─
            //
            // Set the cancel flag and join the indicator thread on EVERY path —
            // success, any PortalOutcome, OR an operational error — BEFORE
            // calling restart() or returning Err. We must never reboot
            // mid-LED-transfer (a WS2812 latches its last colour until the
            // next full frame, so an interrupted RMT/SPI burst leaves the
            // strip in an undefined state across the reset), and the indicator
            // thread must never outlive this scope.
            indicator_cancel.store(true, Ordering::Relaxed);
            // join() only errors if the thread panicked; log and continue so
            // the device still restarts rather than getting stuck here.
            if let Err(e) = indicator.join() {
                log::warn!("indicator thread panicked: {e:?}");
            }

            // The indicator is now stopped; propagate any operational error.
            let outcome = result?;

            match outcome {
                PortalOutcome::JustProvisioned => {
                    log::info!("Provisioning committed — restarting into normal boot.");
                }
                PortalOutcome::FactoryResetRequested => {
                    // Erase the NVS provisioning namespace so the next boot
                    // re-enters the portal.  Best-effort: log and restart even
                    // if the erase fails — stranding the device here is worse
                    // than a second erase attempt on the following boot.
                    match ProvisioningStore::open(nvs_for_erase).and_then(|mut s| s.erase_all()) {
                        Ok(()) => log::info!("Factory reset: NVS provisioning namespace erased."),
                        Err(e) => log::warn!(
                            "Factory reset: erase_all failed (will retry on next boot): {e:#}"
                        ),
                    }
                    log::info!("Factory reset — restarting.");
                }
                PortalOutcome::PortalExitedWithoutCommit => {
                    log::warn!("Portal timed out with no commit — restarting.");
                }
                // `#[non_exhaustive]` requires a wildcard arm.
                _ => {
                    log::warn!("Unknown portal outcome — restarting.");
                }
            }

            // Restart on all portal outcomes.  The library never restarts itself.
            esp_idf_svc::hal::reset::restart();
        }

        WifiMqttLoadOutcome::OtherProfile(profile) => {
            // Provisioned under a different profile (e.g. LorawanFieldDevice).
            // A real application might erase and re-provision; here we restart.
            log::warn!(
                "Device is provisioned under the '{:?}' profile, not WifiMqttDevice. \
                 Factory-reset to re-provision.",
                profile,
            );
            esp_idf_svc::hal::reset::restart();
        }

        // `#[non_exhaustive]` requires a wildcard arm.
        _ => {
            log::warn!("Unexpected load outcome — restarting.");
            esp_idf_svc::hal::reset::restart();
        }
    };

    // ── Normal boot: hand borrowed configs to WiFiManager / MqttBuilder ─────
    let wifi_cfg = boot.wifi_config();
    let mqtt_cfg = boot.mqtt_config();

    log::info!(
        "Loaded provisioned config: wifi_ssid len={}, mqtt_host len={}, mqtt_port={}, \
         mqtt_client_id len={}, wifi_pass len={} (secret)",
        wifi_cfg.ssid.len(),
        mqtt_cfg.host.len(),
        mqtt_cfg.port,
        mqtt_cfg.client_id.len(),
        // Log secrets by length only — never the values themselves.
        wifi_cfg.password.len(),
    );

    log::info!("A real application would now proceed to normal STA + MQTT boot.");
    // WiFiManager::new_without_led(modem, sys_loop, Some(nvs), wifi_cfg)?;
    // MqttBuilder::new(mqtt_cfg).build()?;

    Ok(())
}
