# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `lora-pure` crate: platform-independent `no_std` library with the `LoraRadio` trait, all LoRa/LoRaWAN types, `LorawanDevice`, session types, OTA command parser, and `MockLoraRadio` test double (ADR 005); `rustyfarian-esp-hal-lora` bare-metal stub implementing `lora_pure::LoraRadio` for future esp-hal integration
- `rustyfarian-esp-idf-lora`: `LoraRadioAdapter` bridging `EspIdfLoraRadio` to `lorawan-device 0.12`'s `PhyRxTx + Timings`; `idf_esp32s3_join` OTAA join example for the Heltec WiFi LoRa 32 V3; `hal_esp32s3_join` bare-metal example performing SX1262 reset and status check
- Dual-HAL script infrastructure: `build-example.sh` and `flash.sh` extended for `hal_*` bare-metal targets with per-chip feature flags; `ensure-bootloader.sh`, `xtensa-toolchain.sh`, and `host-target.sh` helpers added; xtensa bare-metal target sections added to `.cargo/config.toml.dist`
- `rustyfarian-esp-idf-mqtt`: `MqttBuilder` API returning a cloneable `MqttHandle` with `on_connect`, `on_disconnect`, and `on_message` lifecycle callbacks; `MqttHandle::is_connected()` for synchronous connection-state polling
- `rustyfarian-network-pure`: MQTT input validation functions (`validate_client_id`, `validate_topic`, `validate_broker_host`, `validate_broker_port`, `format_broker_url`) and `MqttConnectionState` pure state machine; `idf_esp32_mqtt` example targeting Xtensa ESP32; `docs/heltec-wifi-lora-32-v3.md` hardware reference
- `espnow-pure` crate: platform-independent `no_std` library with the `EspNowDriver` trait, `EspNowEvent`, `PeerConfig`, `MacAddress`, validation, and `MockEspNowDriver` test double (ADR 007)
- `rustyfarian-esp-idf-espnow` crate: ESP-IDF driver implementing `EspNowDriver` via `esp-idf-svc::espnow::EspNow` with `sync_channel`-based receive bridge

### Fixed

- `rustyfarian-esp-idf-mqtt`: fixed `MqttHandle::is_connected()` race where the flag was set before `on_connect` released the mutex; fixed `MqttManager::new` subscribing on an unconnected client (caused FreeRTOS heap corruption); fixed connection error detection, `AtomicBool` memory ordering, and thread-spawn error propagation; `MqttConfig` `Debug` impl now redacts credentials
- `WiFiManager::get_ip` no longer propagates transient ESP-IDF errors; `Blocking` mode now correctly respects the configured timeout; connection-wait loop uses ceiling division to honour the full timeout

### Changed

- Bump `esp-idf-hal` 0.45→0.46, `esp-idf-svc` 0.51→0.52, `heapless` 0.8→0.9, `embedded-hal-bus` 0.2→0.3, `esp-println` 0.13→0.16: `PinDriver` pin-type parameter removed (type-erased), `PinDriver::input` now requires explicit `Pull` argument, `Modem` requires `'static` lifetime
- `rustyfarian-esp-idf-lora`: all pure types moved to `lora-pure` and re-exported; `esp-idf`/`mock` feature flags removed; `lora-pure` trait docs clarified; `rustyfarian-esp-hal-lora` stub returns per-operation error variants; `esp32c6` feature properly forwards to `esp-hal`
- `rustyfarian-esp-idf-wifi`: `WiFiConfig` fields privatised; `ConnectMode` enum replaces `connection_timeout_secs`; `WiFiManager::new_without_led()` added; `NonBlocking` mode logs disconnect reason at `WARN` and propagates initiation errors
- `rustyfarian-esp-idf-mqtt`: `MqttManager::new()` deprecated in favour of `MqttBuilder` (removal target 0.3.0); `WiFiManager::new` SSID/password validation now delegates to `rustyfarian-network-pure`

### Deprecated

- `MqttManager::send_startup_message()` — use `publish()` or `publish_with()` instead
- `MqttManager::send_shutdown_message()` — use `publish()` or `publish_with()` instead
