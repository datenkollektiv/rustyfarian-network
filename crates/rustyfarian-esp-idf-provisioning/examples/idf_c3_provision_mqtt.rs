//! SoftAP captive-portal provisioning example for ESP32-C3 using the
//! Wi-Fi + MQTT profile.
//!
//! Demonstrates the [`SchemaProfile::WifiMqttDevice`] host contract:
//!
//! 1. Open the [`ProvisioningStore`] and check [`ProvisioningStore::is_provisioned`].
//! 2. If already provisioned, load the stored config (logging secrets by length
//!    only) — a real application would proceed to its normal STA + MQTT boot
//!    here, constructing an `MqttConfig` from the stored MQTT group.
//! 3. Otherwise, run the provisioning portal under the `WifiMqttDevice` profile.
//! 4. After the portal commits, log the MQTT target (host length + port only,
//!    never credential values) and restart so the device boots into normal
//!    mode.
//!
//! The library never reboots or erases on its own; the
//! `esp_idf_svc::hal::reset::restart()` call below lives in this **example**,
//! not the crate.
//!
//! # Constructing the downstream `MqttConfig`
//!
//! The committed [`MqttFields`](rustyfarian_esp_idf_provisioning::MqttFields)
//! group maps one-to-one onto `rustyfarian_esp_idf_mqtt::MqttConfig`:
//! `MqttConfig::new(host, port, client_id)` plus optional
//! `with_auth(username, password)`. `MqttConfig` borrows its `&str` arguments,
//! so the owned strings backing them must outlive it; `client_id()` is `None`
//! when the firmware should derive one (here, from the device name truncated to
//! the 23-byte MQTT 3.1.1 cap). The construction is shown in
//! [`mqtt_config_from_stored`] below.
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

use std::time::Duration;

use rustyfarian_esp_idf_mqtt::MqttConfig;
use rustyfarian_esp_idf_provisioning::{
    PortalConfig, ProvisioningBuilder, ProvisioningStore, SchemaProfile, StoredConfig,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// The MQTT 3.1.1 client-ID byte cap; a derived ID must not exceed it.
const CLIENT_ID_MAX_LEN: usize = 23;

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
            // The stored profile may be lorawan if this device was provisioned
            // under the other profile; only build an MqttConfig when it matches.
            if cfg.profile == SchemaProfile::WifiMqttDevice {
                log::info!(
                    "Already provisioned (wifi_mqtt): ssid len={}, mqtt_host len={}, \
                     mqtt_port={}, mqtt_user={}, ota_url len={}, name={}, \
                     mqtt_pass len={} (secret), wifi_pass len={} (secret)",
                    cfg.wifi_ssid.len(),
                    cfg.mqtt_host.len(),
                    cfg.mqtt_port,
                    cfg.mqtt_user.as_deref().map(str::len).unwrap_or(0),
                    cfg.ota_url.len(),
                    cfg.device_name,
                    cfg.mqtt_pass.as_deref().map(str::len).unwrap_or(0),
                    cfg.wifi_password.len(),
                );
                // A real application would build the client and proceed to its
                // normal STA + MQTT boot here.
                let client_id = derive_client_id(&cfg);
                let mqtt_config = mqtt_config_from_stored(&cfg, &client_id);
                log::info!(
                    "Constructed MqttConfig (host len={}, port={}, client_id len={}).",
                    mqtt_config.host.len(),
                    mqtt_config.port,
                    mqtt_config.client_id.len(),
                );
                log::info!("A real application would now proceed to normal STA + MQTT boot.");
            } else {
                log::warn!(
                    "Already provisioned under the lorawan profile; this example only \
                     boots the wifi_mqtt profile. Factory-reset to re-provision."
                );
            }
        }
        return Ok(());
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
        device_name: "c3-mqtt-demo",
        firmware_version: env!("CARGO_PKG_VERSION"),
        profile: SchemaProfile::WifiMqttDevice,
    };

    let session = ProvisioningBuilder::new(config)
        .with_status_entry("role", "mqtt-provision-demo")
        .on_event(|event| log::info!("Provisioning event: {event:?}"))
        .start(peripherals.modem, sys_loop, nvs)?;

    log::info!(
        "Portal running — connect to the AP and open http://{}/",
        session.ap_ip()
    );

    match session.wait_committed(Some(Duration::from_secs(600))) {
        Some(cfg) => {
            // Log the MQTT target by host length and port only — never the
            // username or password values, only their presence by length.
            match cfg.mqtt() {
                Some(mqtt) => log::info!(
                    "Provisioning committed (ssid len={}, mqtt_host len={}, mqtt_port={}, \
                     mqtt_user present={}, mqtt_pass present={}, mqtt_client present={}) — \
                     restarting into normal boot.",
                    cfg.wifi_ssid().len(),
                    mqtt.host().len(),
                    mqtt.port(),
                    mqtt.username().is_some(),
                    mqtt.password().is_some(),
                    mqtt.client_id().is_some(),
                ),
                None => log::warn!(
                    "Committed config unexpectedly missing its MQTT group; restarting anyway."
                ),
            }
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

/// Derives the MQTT client ID for a stored `WifiMqttDevice` config.
///
/// When the operator supplied one (`mqtt_client`), it is used verbatim — it was
/// validated against the 23-byte cap at parse time. When blank, the firmware
/// derives one from the device name, truncated to [`CLIENT_ID_MAX_LEN`] on a
/// UTF-8 char boundary so a 24-byte device name still yields a valid ID.
fn derive_client_id(cfg: &StoredConfig) -> String {
    if let Some(client) = &cfg.mqtt_client {
        return client.clone();
    }
    let mut id = String::new();
    for ch in cfg.device_name.chars() {
        if id.len() + ch.len_utf8() > CLIENT_ID_MAX_LEN {
            break;
        }
        id.push(ch);
    }
    id
}

/// Builds an `MqttConfig` from a stored `WifiMqttDevice` config and a
/// pre-derived client ID.
///
/// `MqttConfig` borrows its arguments, so `cfg` and `client_id` must outlive the
/// returned value. The optional auth pair maps onto `with_auth` when both are
/// present, onto `with_username_only` when only the username is — the latter
/// omits the CONNECT password field entirely, matching broker-side username-
/// only ACLs — and leaves the auth unset for an anonymous connection.
fn mqtt_config_from_stored<'a>(cfg: &'a StoredConfig, client_id: &'a str) -> MqttConfig<'a> {
    let config = MqttConfig::new(&cfg.mqtt_host, cfg.mqtt_port, client_id);
    match (cfg.mqtt_user.as_deref(), cfg.mqtt_pass.as_deref()) {
        (Some(user), Some(pass)) => config.with_auth(user, pass),
        (Some(user), None) => config.with_username_only(user),
        _ => config,
    }
}
