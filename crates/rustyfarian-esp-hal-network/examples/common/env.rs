//! Shared compile-time `.env` configuration for the bare-metal MQTT examples.
//!
//! The `no_std` twin of the ESP-IDF crate's `examples/common/env.rs`: same names, so an
//! example reads its configuration the same way on both tiers.
//! Values come from `option_env!`, so they are baked into the binary at build time via
//! `just` (`set dotenv-load` in the workspace `justfile` loads `.env` automatically).
//!
//! Each example includes this file with `#[path = "common/env.rs"] mod env;` and may not
//! use every item below, so unused items are allowed per-item rather than at module scope
//! (a non-root module cannot carry `#![allow(dead_code)]`).

use rustyfarian_esp_hal_network::mqtt::MqttConfig;

macro_rules! env_or {
    ($key:literal, $default:literal) => {
        match option_env!($key) {
            Some(v) => v,
            None => $default,
        }
    };
}

/// Wi-Fi network name.
#[allow(dead_code)]
pub const WIFI_SSID: &str = env_or!("WIFI_SSID", "");

/// Wi-Fi password; empty when `WIFI_PASS` is unset. Deliberately no literal fallback so
/// CodeQL's hard-coded-credential query has no source.
#[allow(dead_code)]
pub fn wifi_pass() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

/// MQTT broker address; an IPv4 literal on this tier (no DNS yet, ADR 019).
#[allow(dead_code)]
pub const MQTT_HOST: &str = env_or!("MQTT_HOST", "");

/// MQTT broker port, as a string straight from the environment (see [`mqtt_port`]).
#[allow(dead_code)]
pub const MQTT_PORT_STR: &str = env_or!("MQTT_PORT", "1883");

/// Fallback MQTT broker port when `MQTT_PORT` is unset or fails to parse.
#[allow(dead_code)]
pub const DEFAULT_MQTT_PORT: u16 = 1883;

/// Parses [`MQTT_PORT_STR`] into a `u16`, falling back to [`DEFAULT_MQTT_PORT`] and
/// logging a warning if the value set at build time is not a valid port number.
#[allow(dead_code)]
pub fn mqtt_port() -> u16 {
    match MQTT_PORT_STR.parse::<u16>() {
        Ok(port) => port,
        Err(_) => {
            log::warn!(
                "MQTT_PORT '{}' is not a valid port — falling back to {}",
                MQTT_PORT_STR,
                DEFAULT_MQTT_PORT
            );
            DEFAULT_MQTT_PORT
        }
    }
}

/// Returns `MQTT_CLIENT_ID` if set at build time, otherwise the caller-supplied default.
#[allow(dead_code)]
pub fn mqtt_client_id(default: &'static str) -> &'static str {
    option_env!("MQTT_CLIENT_ID").unwrap_or(default)
}

/// Returns [`MQTT_HOST`], or an error message if it was never configured.
#[allow(dead_code)]
pub fn mqtt_host() -> Result<&'static str, &'static str> {
    if MQTT_HOST.is_empty() {
        return Err("MQTT_HOST not configured — set it at build time; see .env.example");
    }
    Ok(MQTT_HOST)
}

/// Builds an [`MqttConfig`] from the shared `.env` settings for the given client ID.
#[allow(dead_code)]
pub fn mqtt_config(client_id: &'static str) -> Result<MqttConfig<'static>, &'static str> {
    Ok(MqttConfig::new(mqtt_host()?, mqtt_port(), client_id))
}

/// Logs the shared configuration at info level. Never logs `WIFI_PASS`.
#[allow(dead_code)]
pub fn log_config(client_id: &str) {
    log::info!(
        "Config — ssid={} mqtt_host={} mqtt_port={} client_id={}",
        WIFI_SSID,
        MQTT_HOST,
        mqtt_port(),
        client_id
    );
}

/// Returns `MQTT_TOPIC_PREFIX` if set at build time, otherwise the caller-supplied default.
#[allow(dead_code)]
pub fn mqtt_topic_prefix(default: &'static str) -> &'static str {
    option_env!("MQTT_TOPIC_PREFIX").unwrap_or(default)
}

/// Optional MQTT username (`MQTT_USER`).
#[allow(dead_code)]
pub const MQTT_USER: Option<&str> = option_env!("MQTT_USER");

/// Optional MQTT password (`MQTT_PASS`); no literal fallback, so CodeQL sees no source.
#[allow(dead_code)]
pub fn mqtt_pass() -> Option<&'static str> {
    option_env!("MQTT_PASS")
}

/// `MQTT_SUSPEND_DEMO=1` makes the S3 LWT examples suspend Wi-Fi for 20 s every 60 s.
#[allow(dead_code)]
pub fn mqtt_suspend_demo() -> bool {
    option_env!("MQTT_SUSPEND_DEMO") == Some("1")
}
