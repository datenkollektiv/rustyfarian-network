# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `wifi-pure` crate with `WifiDriver` trait, `WiFiConfig`, `ConnectMode`, `MockWifiDriver`, and SSID/password validation (ADR 006); `rustyfarian-esp-hal-wifi` bare-metal stub
- `lora-pure` crate with `LoraRadio` trait, LoRaWAN types, OTA command parser, and `MockLoraRadio` (ADR 005); `rustyfarian-esp-hal-lora` bare-metal stub
- `rustyfarian-esp-idf-lora`: `LoraRadioAdapter` bridging to `lorawan-device 0.12`; `idf_esp32s3_join` and `hal_esp32s3_join` examples for Heltec WiFi LoRa 32 V3
- `espnow-pure` crate with `EspNowDriver` trait, `EspNowEvent`, `PeerConfig`, `WifiInterface` (STA/AP), and `MockEspNowDriver` (ADR 007); `rustyfarian-esp-idf-espnow` ESP-IDF driver
- `rustyfarian-esp-idf-mqtt`: `MqttBuilder` API with `MqttHandle`, lifecycle callbacks (`on_connect`, `on_disconnect`, `on_message`), `LwtConfig`, `with_auth()`, and `publish_with()` (ADR 002)
- `rustyfarian-network-pure`: MQTT input validation, `MqttConnectionState` state machine, and `ExponentialBackoff` iterator for retry logic
- Dual-HAL script infrastructure: `build-example.sh`, `flash.sh`, `ensure-bootloader.sh`, and `xtensa-toolchain.sh` for `hal_*` bare-metal targets
- Examples: `idf_c3_connect`, `idf_c3_mqtt`, `idf_esp32_mqtt`; hardware reference `docs/heltec-wifi-lora-32-v3.md`
- CI: pure-crate test job for all host tests (`rustyfarian-network-pure`, `wifi-pure`, `lora-pure`, `espnow-pure`)

### Fixed

- `rustyfarian-esp-idf-mqtt`: fixed `MqttHandle::is_connected()` race where the flag was set before `on_connect` released the mutex; fixed `MqttManager::new` subscribing on an unconnected client (caused FreeRTOS heap corruption); fixed connection error detection, `AtomicBool` memory ordering, and thread-spawn error propagation; `MqttConfig` `Debug` impl now redacts credentials
- `WiFiManager::get_ip` no longer propagates transient ESP-IDF errors; `Blocking` mode now correctly respects the configured timeout; connection-wait loop uses ceiling division to honour the full timeout

### Changed

- Bump `esp-idf-hal` 0.45→0.46, `esp-idf-svc` 0.51→0.52, `heapless` 0.8→0.9, `embedded-hal-bus` 0.2→0.3, `esp-println` 0.13→0.16: `PinDriver` pin-type parameter removed (type-erased), `PinDriver::input` now requires explicit `Pull` argument, `Modem` requires `'static` lifetime
- `rustyfarian-esp-idf-lora`: all pure types moved to `lora-pure` and re-exported; `esp-idf`/`mock` feature flags removed; `lora-pure` trait docs clarified; `rustyfarian-esp-hal-lora` stub returns per-operation error variants; `esp32c6` feature properly forwards to `esp-hal`
- `rustyfarian-esp-idf-wifi`: `WiFiConfig` fields privatised; `ConnectMode` enum replaces `connection_timeout_secs`; `WiFiManager::new_without_led()` added; `NonBlocking` mode logs disconnect reason at `WARN` and propagates initiation errors
- `rustyfarian-esp-idf-mqtt`: `MqttManager::new()` deprecated in favour of `MqttBuilder` (removal target 0.3.0); multi-topic subscription (`&[&str]`); topic-based dispatch (callback receives `(&str, &[u8])` instead of `&[u8]`); SSID/password validation delegates to `rustyfarian-network-pure`

### Deprecated

- `MqttManager::new()` — use `MqttBuilder` instead (removal target 0.3.0)
- `MqttManager::send_startup_message()` — use `publish()` or `publish_with()` instead
- `MqttManager::send_shutdown_message()` — use `publish()` or `publish_with()` instead
