# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `rust-toolchain.toml` pinning the workspace to the `esp` toolchain — rustup now selects the correct toolchain automatically without requiring `source ~/export-esp.sh` for every new shell session
- `rustyfarian-esp-idf-wifi`: In `NonBlocking` mode, `WiFiManager` now subscribes to `WifiEvent::StaDisconnected` and logs the reason code with a human-readable name (e.g. `NO_AP_FOUND`, `AUTH_FAIL`) at `WARN` level — previously a wrong SSID or unavailable AP was invisible without debug-level logging

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

- `rustyfarian-esp-idf-mqtt`: `MqttManager::new` no longer logs "MQTT connection timeout" when the broker is unreachable (`ESP_FAIL`); a dedicated `connection_error` flag now distinguishes a definitive connection failure from a genuine timeout, and the loop exits early instead of waiting for the full timeout duration
- `rustyfarian-network-pure`: `connection_wait_iterations` now uses `u64::div_ceil` instead of manual ceiling division, resolving a `clippy::manual_div_ceil` warning
- `rustyfarian-network-pure`: `empty_password_is_valid` test suppresses `clippy::unnecessary_owned_empty_strings` via `#[allow]` to preserve the `&String::new()` workaround that prevents CodeQL false-positive "hardcoded credential" alerts
- `WiFiManager::get_ip` no longer propagates transient ESP-IDF errors (e.g. `ESP_ERR_TIMEOUT` from `is_connected` or `get_ip_info`) to the caller; they are logged at `debug` level and the poll loop continues, honouring the documented `Ok(Some(ip))` / `Ok(None)` contract
- `WiFiManager::new` in `Blocking` mode now correctly respects the configured timeout when no LED is present
- `MqttManager::new` connection-wait loop now uses ceiling division for the iteration count, ensuring the full configured timeout is always honoured (e.g. a 5050 ms timeout previously yielded 50 iterations / 5000 ms)
- `MqttManager::shutdown` had a redundant inner `#[allow(deprecated)]` on the `send_shutdown_message` call; removed (the outer attribute on the function already suppresses the warning)

### Changed

- `WiFiManager::new` SSID and password length validation is now performed once by `validate_ssid` / `validate_password` from `rustyfarian-network-pure`; the subsequent `try_into` conversion failure is now treated as an internal invariant violation and includes the actual length and limit for diagnostics
- `validate_password` error message capitalised to match `validate_ssid` style
- `rustyfarian-network-pure` crate metadata: removed misleading `"no-std"` category (crate is standard `std` Rust)
- `WiFiConfig::with_timeout` now sets `ConnectMode::Blocking { timeout_secs }` instead of `connection_timeout_secs: Option<u64>`
- `WiFiConfig` fields are now private; construct via `WiFiConfig::new()` and the `with_timeout()` / `connect_nonblocking()` builders
- `WiFiManager::new` now logs at `warn` level (was `info`) and remains blocking if `NonBlocking` is requested while an LED driver is present, as the driver is currently polled in the foreground
- `WiFiManager::new` in `NonBlocking` mode now propagates `connect()` initiation errors instead of logging and continuing

### Deprecated

- `MqttManager::send_startup_message()` — use `publish()` or `publish_with()` instead
- `MqttManager::send_shutdown_message()` — use `publish()` or `publish_with()` instead
