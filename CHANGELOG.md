# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `LwtConfig` struct with `new()` constructor for Last Will and Testament support
- `MqttConfig::with_lwt()` builder for configuring LWT messages
- `MqttConfig::with_auth()` builder for MQTT broker authentication
- `MqttManager::publish_with()` for publishing with explicit QoS and retain control
- Multi-topic subscription via `&[&str]` constructor parameter
- Topic-based dispatch: callback receives `(topic, payload)` instead of just `payload`
- `ConnectMode` enum (`Blocking { timeout_secs }` / `NonBlocking`) on `WiFiConfig`, replacing the `connection_timeout_secs` field
- `WiFiConfig::connect_nonblocking()` builder — `WiFiManager::new` returns immediately and lets the ESP-IDF event loop drive association in the background
- `WiFiManager::new_without_led()` convenience constructor — avoids the `None::<&mut SomeLed>` turbofish annotation when no LED driver is needed

### Fixed

- `WiFiManager::get_ip` no longer propagates transient ESP-IDF errors (e.g. `ESP_ERR_TIMEOUT` from `is_connected` or `get_ip_info`) to the caller; they are logged at `debug` level and the poll loop continues, honouring the documented `Ok(Some(ip))` / `Ok(None)` contract
- `WiFiManager::new` in `Blocking` mode now correctly respects the configured timeout when no LED is present

### Changed

- `WiFiConfig::with_timeout` now sets `ConnectMode::Blocking { timeout_secs }` instead of `connection_timeout_secs: Option<u64>`
- `WiFiConfig` fields are now private; construct via `WiFiConfig::new()` and the `with_timeout()` / `connect_nonblocking()` builders
- `WiFiManager::new` now logs at `warn` level (was `info`) and remains blocking if `NonBlocking` is requested while an LED driver is present, as the driver is currently polled in the foreground
- `WiFiManager::new` in `NonBlocking` mode now propagates `connect()` initiation errors instead of logging and continuing

### Deprecated

- `MqttManager::send_startup_message()` — use `publish()` or `publish_with()` instead
- `MqttManager::send_shutdown_message()` — use `publish()` or `publish_with()` instead
