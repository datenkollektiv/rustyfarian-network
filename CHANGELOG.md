# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `rustyfarian-esp-idf-lora`: `LoraRadioAdapter` struct — bridges `EspIdfLoraRadio` to
  `lorawan-device 0.12`'s `PhyRxTx + Timings` traits, enabling the nb state machine to drive
  the SX1262 via the lorawan-device protocol stack
- `rustyfarian-esp-idf-lora`: `idf_esp32s3_join` example — OTAA join test for the Heltec WiFi
  LoRa 32 V3 (ESP32-S3 + SX1262) against TTN v3 EU868; credentials set via compile-time env vars
- `rustyfarian-esp-idf-lora`: `build.rs` added; `lorawan-device`, `lora-modulation`, and
  `rand_core` added as dependencies to support the `PhyRxTx` bridge and the join example
- `scripts/build-example.sh` and `scripts/flash.sh`: extended `idf` branch to handle `join`/`lora`
  package detection and `esp32s3` chip targeting with Xtensa toolchain activation
- `justfile`: `build-lora-idf-esp32s3` convenience recipe for building `idf_esp32s3_join`
- `docs/heltec-wifi-lora-32-v3.md`: hardware reference for the Heltec WiFi LoRa 32 V3 —
  SX1262 pin assignments, TCXO/DIO3 power requirement, correct initialisation sequence with
  byte-level SPI detail, `esp-hal 1.0` wiring pattern, and common bring-up failure modes
- `rustyfarian-esp-hal-lora`: `hal_esp32s3_join` example rewritten from stub to real SX1262
  hardware bring-up; performs reset → `SetDIO3AsTCXOCtrl` (1.8 V TCXO) →
  `SetDio2AsRfSwitchCtrl` → `GetStatus` and decodes chip mode + command status;
  expected output is `0x22` (STDBY_RC, cmd ok)
- `rustyfarian-esp-hal-lora`: `embedded-hal 1.0` and `embedded-hal-bus 0.2` added as
  dependencies; SPI transactions use `ExclusiveDevice::new_no_delay` wrapping `esp-hal`'s
  `SpiBus` into an `SpiDevice`

### Fixed

- `rustyfarian-esp-idf-mqtt`: `MqttHandle::is_connected()` now returns `true` only after the `on_connect` callback has completed and released the internal mutex.
  Previously the flag was set before the callback ran, creating a race window where a concurrent `publish_with()` caller could try to lock the same mutex while the event loop thread held it, potentially deadlocking both threads inside `esp_mqtt_client_enqueue()`.

### Changed

- `lora-pure`: `LoraRadio` trait docs clarified — `nb::WouldBlock` semantics, `prepare_*` call ordering, and `prepare_rx` re-entry contract after a failed receive window
- `lora-pure`: `RX_WINDOW_OFFSET_MS` doc made platform-neutral (removed ESP-IDF-specific rationale)
- `rustyfarian-esp-hal-lora`: stub driver now returns per-operation error variants (`TransmitFailed` from TX methods, `ReceiveFailed` from RX methods) instead of `RadioInitFailed` for all operations
- `rustyfarian-esp-hal-lora`: `esp32c6` Cargo feature now properly forwards `esp-hal/esp32c6` instead of hardcoding the chip feature inside the dependency declaration

### Added

- `scripts/host-target.sh`: extracted host-triple detection to a standalone script (mirrors `rustyfarian-ws2812`); `justfile` now delegates to it instead of inlining the `rustc -vV | awk` construct
- `.cargo/config.toml.dist`: added `[target.xtensa-esp32-none-elf]` and `[target.xtensa-esp32s3-none-elf]` sections with `-Tlinkall.x` and `-fno-stack-protector` rustflags required by bare-metal esp-hal builds with GCC 15.2
- `justfile`: `build-lora-esp32s3` convenience alias for `just build-example hal_esp32s3_join`
- `rustyfarian-esp-hal-lora`: `hal_esp32s3_join` example demonstrating a bare-metal LoRaWAN join attempt on the Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262); prints stub status to UART and halts — hardware integration pending
- `rustyfarian-esp-hal-lora`: `esp32s3`, `esp32`, and `esp32c3` chip features (mirror of existing `esp32c6`), plus `unstable` and `rt` forwarding features required by `esp-hal` 1.0 for bare-metal examples
- `scripts/build-example.sh`: dual-HAL support — `hal_*` prefix routes to bare-metal targets with per-chip feature flags and Xtensa toolchain sourcing; `idf_*` prefix preserves existing behaviour
- `scripts/flash.sh`: dual-HAL support — `hal_*` prefix builds bare-metal binary and flashes with the IDF-built bootloader sourced from the IDF target's release cache
- `scripts/ensure-bootloader.sh`: new helper that checks the IDF-built v5.3.3 bootloader cache and triggers an IDF example build to populate it if absent; required before flashing bare-metal HAL examples
- `scripts/xtensa-toolchain.sh`: new helper sourced by build and flash scripts to inject `xtensa-esp-elf-gcc` into PATH for ESP32 Xtensa bare-metal builds
- `rustyfarian-esp-idf-mqtt`: `MqttBuilder` API via `MqttBuilder::new(config)` with `.on_connect()`, `.on_disconnect()`, and `.on_message()` lifecycle callbacks; `on_connect` receives a `bool` indicating whether this is a clean session (no broker-side state preserved), enabling callers to skip redundant re-subscriptions on session resume
- `rustyfarian-esp-idf-mqtt`: `MqttHandle` returned by `MqttBuilder::build()` — cloneable, thread-safe MQTT handle that accepts `&self` for publish calls from any thread without requiring `&mut` access
- `rustyfarian-esp-idf-mqtt`: `MqttHandle::is_connected()` for synchronous connection-state polling, replacing the need to infer disconnection from publish-failure counts
- `rustyfarian-network-pure`: `validate_client_id`, `validate_topic`, `validate_broker_host`, `validate_broker_port` — pure MQTT input validation functions, host-testable
- `rustyfarian-network-pure`: `format_broker_url` — pure broker URL construction extracted from `MqttManager`
- `rustyfarian-network-pure`: `MqttConnectionState`, `MqttEvent`, and `next_state()` — pure connection state machine with fully tested transition table, encoding the invariants that `on_connect` never fires while already connected and `on_disconnect` never fires before the first successful connection
- `lora-pure` crate: platform-independent `no_std` library containing the `LoraRadio` trait, all LoRa/LoRaWAN types (`SpreadingFactor`, `Bandwidth`, `CodingRate`, `TxConfig`, `RxConfig`, `RxWindow`, `RxQuality`), `LorawanDevice`, session types, OTA command parser, and `MockLoraRadio` test double — implements ADR 005 (separate crate per HAL, no mutually exclusive feature flags)
- `rustyfarian-esp-hal-lora` crate: bare-metal `no_std` stub crate for future ESP-HAL LoRa integration; provides `EspHalLoraRadio<S: StatusLed>` implementing `lora_pure::LoraRadio`; all methods return graceful errors pending hardware integration
- `justfile`: `check-lora-pure` and `check-lora-hal` recipes for targeted crate checks
- `idf_esp32_mqtt` example for `rustyfarian-esp-idf-mqtt` targeting `xtensa-esp32-espidf` / `MCU=esp32` — the Xtensa parallel to `idf_c3_mqtt`

### Changed

- `rustyfarian-esp-idf-lora`: all pure types (`LoraRadio`, `LoraConfig`, `LorawanDevice`, `MockLoraRadio`, etc.) moved to `lora-pure` and re-exported for backward compatibility; crate now depends on `lora-pure` and is always `std`/ESP-IDF — the `esp-idf`/`mock` feature flags are removed
- `rustyfarian-esp-idf-lora`: `sx1262_driver.rs` updated to import from `lora_pure::` instead of `crate::`
- `justfile`: `test-lora` now runs tests from `lora-pure --features mock` (the pure crate hosts all platform-independent tests)
- `rustyfarian-esp-idf-lora` crate: LoRa radio abstraction (`LoraRadio` trait), LoRaWAN Class A session types, OTA downlink command parser (`commands.rs`), and `MockLoraRadio` test double — enables host-side unit testing of LoRaWAN application logic without hardware
- `rustyfarian-esp-idf-lora`: `EspIdfLoraRadio` driver scaffold for the SX1262 on the Heltec WiFi LoRa 32 V3; all methods return graceful errors until hardware integration is complete (see crate module docs for implementation milestones)
- Custom CodeQL GitHub Actions workflow with ESP toolchain pre-installed to enable full Rust analysis quality (resolves "Low Rust analysis quality" warning from GitHub's default CodeQL setup)
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

- `rustyfarian-esp-idf-mqtt`: `MqttBuilder::build()` no longer leaks config strings via `Box::leak`; `esp_mqtt_client_init()` copies credentials with `strdup()` during init, so owned `String` values borrowed only for the constructor call are sufficient — no `'static` lifetime required for config fields
- `rustyfarian-esp-idf-mqtt`: `MqttConfig` no longer derives `Debug` automatically; a manual `Debug` impl redacts `username` and `password` fields with `"<redacted>"` to prevent credentials appearing in log output (resolves CodeQL `cleartext-logging` alert)
- `rustyfarian-esp-idf-mqtt`: `MqttManager::new` no longer calls `subscribe()` on an unconnected client; previously, when the broker was unreachable within the timeout, the unconditional subscribe caused ESP-IDF heap corruption that manifested later as a FreeRTOS `heap_caps_free` assertion failure
- `rustyfarian-esp-idf-mqtt`: `EventPayload::Disconnected` now correctly clears the internal connected flag so connection state is accurate after a broker drop
- `rustyfarian-esp-idf-mqtt`: thread-spawn failure in `MqttManager::new` now propagates as an `anyhow::Error` instead of panicking with `.expect()`
- `rustyfarian-esp-idf-mqtt`: all `AtomicBool` accesses on the `connected` and `shutdown` flags upgraded from `Ordering::Relaxed` to `Ordering::Acquire`/`Release` to establish correct happens-before relationships across threads
- `rustyfarian-esp-idf-mqtt`: `MqttManager::new` no longer logs "MQTT connection timeout" when the broker is unreachable (`ESP_FAIL`); a dedicated `connection_error` flag now distinguishes a definitive connection failure from a genuine timeout, and the loop exits early instead of waiting for the full timeout duration
- `rustyfarian-network-pure`: `connection_wait_iterations` now uses `u64::div_ceil` instead of manual ceiling division, resolving a `clippy::manual_div_ceil` warning
- `rustyfarian-network-pure`: `empty_password_is_valid` test suppresses `clippy::unnecessary_owned_empty_strings` via `#[allow]` to preserve the `&String::new()` workaround that prevents CodeQL false-positive "hardcoded credential" alerts
- `WiFiManager::get_ip` no longer propagates transient ESP-IDF errors (e.g. `ESP_ERR_TIMEOUT` from `is_connected` or `get_ip_info`) to the caller; they are logged at `debug` level and the poll loop continues, honouring the documented `Ok(Some(ip))` / `Ok(None)` contract
- `WiFiManager::new` in `Blocking` mode now correctly respects the configured timeout when no LED is present
- `MqttManager::new` connection-wait loop now uses ceiling division for the iteration count, ensuring the full configured timeout is always honoured (e.g. a 5050 ms timeout previously yielded 50 iterations / 5000 ms)
- `MqttManager::shutdown` had a redundant inner `#[allow(deprecated)]` on the `send_shutdown_message` call; removed (the outer attribute on the function already suppresses the warning)

### Changed

- `rustyfarian-esp-idf-mqtt`: `MqttManager::new()` deprecated in favour of `MqttBuilder`; it will be removed in 0.3.0
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
