//! SoftAP captive-portal provisioning example for ESP32-C3.
//!
//! Demonstrates the full host contract:
//!
//! 1. Open the [`ProvisioningStore`] and check [`ProvisioningStore::is_provisioned`].
//! 2. If already provisioned, load the stored config (logging secrets by length
//!    only) — a real application would proceed to its normal STA boot here.
//! 3. Otherwise, fall back to compile-time `WIFI_SSID`/`WIFI_PSK` if present
//!    (the `idf_c3_connect` pattern), or run the provisioning portal.
//! 4. After the portal commits, restart so the device boots into normal mode.
//!
//! The library never reboots or erases on its own; the `esp_idf_svc::hal::reset::restart()`
//! call below lives in this **example**, not the crate.
//!
//! # Build and flash
//!
//! ```sh
//! PROVISION_AP_PSK="provision-me" just build-example idf_c3_provision
//! ```
//!
//! ```sh
//! just flash idf_c3_provision
//! ```

use std::time::Duration;

use rustyfarian_esp_idf_network::provisioning::{
    PortalConfig, ProvisioningBuilder, ProvisioningStore, SchemaProfile,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Compile-time Wi-Fi fallback (same pattern as `idf_c3_connect`). When NVS is
/// empty but these are set at build time, a real application would use them
/// instead of running the portal.
const FALLBACK_SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const FALLBACK_PSK: &str = match option_env!("WIFI_PSK") {
    Some(s) => s,
    None => "",
};

/// Optional WPA2 password for the provisioning AP. Without it the AP is open.
const AP_PSK: Option<&str> = option_env!("PROVISION_AP_PSK");

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let store = ProvisioningStore::open(nvs.clone())?;
    if store.is_provisioned()? {
        if let Some(cfg) = store.load()? {
            log::info!(
                "Already provisioned: ssid len={}, dev_eui={}, ota_url len={}, name={}, \
                 app_key len={} (secret), wifi_pass len={} (secret)",
                cfg.wifi_ssid.len(),
                cfg.dev_eui_hex,
                cfg.ota_url.len(),
                cfg.device_name,
                cfg.app_key_hex.len(),
                cfg.wifi_password.len(),
            );
            log::info!("A real application would now proceed to normal STA boot.");
        }
        return Ok(());
    }

    if !FALLBACK_SSID.is_empty() {
        log::info!(
            "NVS empty but compile-time WIFI_SSID present (ssid len={}, psk len={}) — \
             a real application could boot from these instead of provisioning.",
            FALLBACK_SSID.len(),
            FALLBACK_PSK.len(),
        );
    }

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

    let config = PortalConfig {
        ssid_prefix: "Rustyfarian",
        ap_password,
        channel: 1,
        device_name: "c3-provision-demo",
        firmware_version: env!("CARGO_PKG_VERSION"),
        profile: SchemaProfile::LorawanFieldDevice,
    };

    let session = ProvisioningBuilder::new(config)
        .with_status_entry("role", "provision-demo")
        .on_event(|event| log::info!("Provisioning event: {event:?}"))
        .start(peripherals.modem, sys_loop, nvs)?;

    log::info!(
        "Portal running — connect to the AP and open http://{}/",
        session.ap_ip()
    );

    match session.wait_committed(Some(Duration::from_secs(600))) {
        Some(cfg) => {
            log::info!(
                "Provisioning committed (ssid len={}) — restarting into normal boot.",
                cfg.wifi_ssid().len()
            );
            session.shutdown()?;
            esp_idf_svc::hal::reset::restart();
        }
        None => {
            log::warn!("Provisioning timed out with no commit; shutting the portal down.");
            session.shutdown()?;
        }
    }

    Ok(())
}
