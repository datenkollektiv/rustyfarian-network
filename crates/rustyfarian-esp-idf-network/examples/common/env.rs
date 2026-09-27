//! Shared compile-time `.env` configuration for the MQTT examples.
//!
//! Values come from `option_env!`, so they are baked into the binary at build time via
//! `just` (`set dotenv-load` in the workspace `justfile` loads `.env` automatically).
//! A shell-exported variable (e.g. from a direnv `.envrc`) takes precedence over `.env`,
//! because `just` never overrides an already-set environment variable.
//!
//! Each example includes this file with `#[path = "common/env.rs"] mod env;` and may not
//! use every item below, so unused items are allowed per-item rather than at module scope
//! (a non-root module cannot carry `#![allow(dead_code)]`).

use rustyfarian_esp_idf_network::mqtt::MqttConfig;

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

/// MQTT broker IP or hostname.
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

/// Returns [`MQTT_HOST`], or bails if it was never configured.
#[allow(dead_code)]
pub fn mqtt_host() -> anyhow::Result<&'static str> {
    if MQTT_HOST.is_empty() {
        anyhow::bail!(
            "MQTT_HOST not configured — set it at build time, e.g.:\n  MQTT_HOST=192.168.1.100 cargo build ...\nSee .env.example for all available variables."
        );
    }
    Ok(MQTT_HOST)
}

/// Builds an [`MqttConfig`] from the shared `.env` settings for the given client ID.
#[allow(dead_code)]
pub fn mqtt_config(client_id: &'static str) -> anyhow::Result<MqttConfig<'static>> {
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
