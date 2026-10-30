//! SoftAP captive-portal provisioning example for ESP32-C3 using the
//! Wi-Fi + MQTT profile.
//!
//! Demonstrates the [`SchemaProfile::WifiMqttDevice`] host contract using the
//! `WifiMqttBoot` helper and the provisioning re-entry hook:
//!
//! 1. [`WifiMqttBoot::load_with_reentry`] first asks the re-entry hook, which
//!    samples the BOOT button (GPIO9, active low).
//!    Button up: the hook answers [`ReentryRequest::None`] at once, and the
//!    NVS store is read as with `WifiMqttBoot::load`.
//!    Held for less than `BOOT_HOLD_MS` (3 s): [`ReentryRequest::Portal`],
//!    so the portal reopens and prefills from the kept record.
//!    Held for `BOOT_HOLD_MS` or longer: [`ReentryRequest::FactoryReset`];
//!    this example erases the record (via [`ProvisioningStore::erase_all`])
//!    and continues into the portal in the same boot.
//!
//!    Timing: GPIO9 is a strapping pin, and the hook samples it once, shortly
//!    after reset.
//!    Release RESET (or power on) with BOOT up, then press and hold BOOT
//!    right away, within about the first half second.
//!    The console's "press BOOT now" line marks the sample point.
//!    Holding BOOT through reset enters ROM download mode instead, and a press
//!    after the sample is ignored.
//! 2. If not provisioned (or re-entry was requested), an indicator thread is
//!    spawned (placeholder for a WS2812/LED animation), then
//!    [`run_wifi_mqtt_portal`] starts the SoftAP captive portal and blocks
//!    until a submission is committed, the portal's factory-reset button is
//!    pressed, or the portal times out.
//! 3. On every portal exit the indicator thread is cancelled and joined BEFORE
//!    calling `restart()` — this guarantees no WS2812 SPI/RMT transfer is
//!    left mid-frame when the device reboots.
//!    On `FactoryResetRequested` the NVS provisioning namespace is erased (via
//!    [`ProvisioningStore::erase_all`]) after the join, then the device restarts
//!    so it re-enters the portal on the next boot.
//!
//! The library never reboots or erases on its own: the hook's answer is only
//! reported as an outcome.  Every `esp_idf_svc::hal::reset::restart()` call
//! and every `ProvisioningStore::erase_all()` call below live in this
//! **example**, not the crate.
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
use std::time::{Duration, Instant};

use anyhow::Context;

use rustyfarian_esp_idf_network::provisioning::{
    run_wifi_mqtt_portal, BootConfig, DeviceName, PortalConfig, PortalDefaults, PortalOutcome,
    ProvisioningEvent, ProvisioningStore, ReentryRequest, SchemaProfile, WifiMqttBoot,
    WifiMqttLoadOutcome,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InputPin, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Optional WPA2 password for the provisioning AP. Without it the AP is open.
const AP_PSK: Option<&str> = option_env!("PROVISION_AP_PSK");

/// Holding BOOT this long (or longer) answers `FactoryReset`; a shorter hold
/// answers `Portal`.
const BOOT_HOLD_MS: u64 = 3000;

/// A low read must still be low after this interval to count as a press, so a
/// single noisy read on GPIO9 does not open the portal.
const BOOT_DEBOUNCE_MS: u64 = 20;

/// The re-entry hook: samples the BOOT button (GPIO9, active low) once, after
/// boot.
///
/// GPIO9 is a strapping pin: press BOOT after releasing RESET, not during it
/// (holding it through reset enters ROM download mode), and before this hook
/// samples it, within about the first half second.
///
/// Returns [`ReentryRequest::None`] immediately when the button is up, so a
/// normal boot is not delayed.  A low read is confirmed after
/// [`BOOT_DEBOUNCE_MS`]; if the second read is high it answers `None`.  While the button is held it blocks, then
/// answers [`ReentryRequest::FactoryReset`] for a hold of at least
/// [`BOOT_HOLD_MS`] and [`ReentryRequest::Portal`] for a shorter one.
///
/// The hook cannot fail the boot: if the pin cannot be configured it logs and
/// answers `None`.  The `PinDriver` is dropped on return, releasing GPIO9.
fn boot_button_reentry(pin: impl InputPin) -> ReentryRequest {
    let button = match PinDriver::input(pin, Pull::Up) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("BOOT button unavailable ({e}); skipping re-entry check.");
            return ReentryRequest::None;
        }
    };

    if button.is_high() {
        return ReentryRequest::None;
    }
    std::thread::sleep(Duration::from_millis(BOOT_DEBOUNCE_MS));
    if button.is_high() {
        return ReentryRequest::None;
    }

    log::info!(
        "BOOT held — release before {} ms to reopen the portal, keep holding to factory-reset.",
        BOOT_HOLD_MS
    );
    let pressed_at = Instant::now();
    let hold = Duration::from_millis(BOOT_HOLD_MS);
    while button.is_low() && pressed_at.elapsed() < hold {
        std::thread::sleep(Duration::from_millis(20));
    }

    // Decide from the loop's exit reason: still low means the hold ran out.
    if button.is_low() {
        log::info!(
            "BOOT held for {} ms — factory reset requested.",
            BOOT_HOLD_MS
        );
        ReentryRequest::FactoryReset
    } else {
        log::info!("BOOT released early — portal requested.");
        ReentryRequest::Portal
    }
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── Step 1: ask the re-entry hook, then load a WifiMqttDevice config ────
    //
    // The hook runs before the store is read; the load never claims the modem.
    let gpio9 = peripherals.pins.gpio9;
    log::info!(
        "Re-entry: press BOOT now to reopen the portal (hold {} ms to factory-reset).",
        BOOT_HOLD_MS
    );
    let loaded = WifiMqttBoot::load_with_reentry(
        nvs.clone(),
        || boot_button_reentry(gpio9),
        WifiMqttBoot::default_client_id,
    )?;

    let boot = match loaded {
        WifiMqttLoadOutcome::Ready(b) => b,

        reason @ (WifiMqttLoadOutcome::NotProvisioned
        | WifiMqttLoadOutcome::PortalRequested
        | WifiMqttLoadOutcome::FactoryResetRequested) => {
            match reason {
                WifiMqttLoadOutcome::PortalRequested => {
                    log::info!("Re-entry: portal requested — keeping the stored record.");
                }
                WifiMqttLoadOutcome::FactoryResetRequested => {
                    // The library only reported the answer; erasing is this
                    // example's decision.  Best-effort: log and continue into
                    // the portal even if the erase fails.
                    match ProvisioningStore::open(nvs.clone()).and_then(|mut s| s.erase_all()) {
                        Ok(()) => {
                            log::info!("Re-entry factory reset: NVS provisioning namespace erased.")
                        }
                        Err(e) => log::warn!("Re-entry factory reset: erase_all failed: {e:#}"),
                    }
                }
                _ => log::info!("Not provisioned — starting the portal."),
            }

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

            // Non-secret defaults seed the portal form on a fresh /
            // factory-reset device (values from `.env`, see `.env.example`).
            // Secrets (`WIFI_PASS`, `MQTT_PASS`) are never pre-filled.
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

            let boot_config = BootConfig {
                portal: PortalConfig {
                    ssid_prefix: "Rustyfarian",
                    ssid_override: None,
                    ap_password,
                    channel: 1,
                    // The portal pre-fills dev_name with the AP-MAC suffix, so two boards
                    // get distinct names and MQTT client IDs.
                    device_name: DeviceName::MacSuffixed("c3-mqtt-demo"),
                    firmware_version: env!("CARGO_PKG_VERSION"),
                    profile: SchemaProfile::WifiMqttDevice,
                    defaults,
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

    // Never log anything derived from a secret — not even its length.
    // A fresh literal keeps the password out of the log's data flow.
    let wifi_pass_state = if wifi_cfg.password.is_empty() {
        "missing"
    } else {
        "set"
    };

    log::info!(
        "Loaded provisioned config: wifi_ssid len={}, mqtt_host len={}, mqtt_port={}, \
         mqtt_client_id len={}, wifi_pass={}",
        wifi_cfg.ssid.len(),
        mqtt_cfg.host.len(),
        mqtt_cfg.port,
        mqtt_cfg.client_id.len(),
        wifi_pass_state,
    );

    // The device identity comes from the same load — no second store read.
    log::info!(
        "Device identity: device_name={}, ota_url={}",
        boot.device_name(),
        if boot.ota_url().is_some() {
            "set"
        } else {
            "not set"
        },
    );

    log::info!("A real application would now proceed to normal STA + MQTT boot.");
    // WiFiManager::new_without_led(modem, sys_loop, Some(nvs), wifi_cfg)?;
    // MqttBuilder::new(mqtt_cfg).build()?;

    Ok(())
}
