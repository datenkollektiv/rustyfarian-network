# Roadmap

This document tracks planned improvements and upcoming work for the `rustyfarian-network` workspace.

Items in **Unreleased** are complete and reflected in `CHANGELOG.md`.
Items in **Planned** are accepted and queued for the next development cycle.

---

## Unreleased

### MQTT Enhancements

Driven by [ADR 002](adr/002-mqtt-enhancements-for-downstream-project.md), the `rustyfarian-esp-idf-mqtt` crate has been expanded with support for Last Will and Testament, authentication, multi-topic subscription, topic-based callback dispatch, and retained-message publishing.

<details>
<summary><strong>Changes</strong></summary>

- `LwtConfig` struct with `new()` constructor for Last Will and Testament support
- `MqttConfig::with_lwt()` builder for attaching an LWT configuration
- `MqttConfig::with_auth()` builder for broker authentication
- Multi-topic subscription: constructor accepts `&[&str]` instead of a single topic
- Topic-based dispatch: callback signature changes from `Fn(&[u8])` to `Fn(&str, &[u8])`
- `MqttManager::publish_with()` for explicit QoS and retain control
- `send_startup_message()` and `send_shutdown_message()` deprecated in favour of `publish_with()`

</details>

### Wi-Fi Reliability Fixes

Two issues reported by rustbox-backstage (vault-standalone firmware) have been resolved in `rustyfarian-esp-idf-wifi`.

<details>
<summary><strong>Changes</strong></summary>

- `WiFiManager::get_ip` now treats transient `is_connected()` and `get_ip_info()` errors as "not ready yet" rather than propagating them; the polling loop continues until the timeout fires, honouring the documented `Ok(Some(ip))` / `Ok(None)` contract
- `ConnectMode` enum replaces the `connection_timeout_secs` field on `WiFiConfig`; timeout only lives inside `Blocking { timeout_secs }`, so it cannot be set in a context where it would have no effect
- `WiFiConfig::connect_nonblocking()` builder sets `NonBlocking` mode; `WiFiManager::new` fires `EspWifi::connect()` and returns immediately, letting the ESP-IDF event loop drive association in the background — see [ADR 003](adr/003-wifi-nonblocking-connect.md)

</details>

---

## Planned

### Grow `rustyfarian-network-pure`

Extract additional platform-independent logic into `rustyfarian-network-pure` so more behaviour
can be verified on the host without an ESP32 or ESP toolchain.

Candidates include reconnection backoff calculations, MQTT topic validation, and any other
pure functions that currently live inside the ESP-IDF crates but have no hardware dependency.
