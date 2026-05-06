# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-05-06

### Changed

- Adopt `rustyfarian-ws2812 v0.5.0` retag covering the upstream crate renames `led-effects` → `pennant` and `ws2812-pure` → `bunting`. Workspace dependency `led-effects` becomes `pennant`; `rustyfarian-esp-hal-ws2812` feature flag `led-effects` becomes `pennant`. All `use led_effects::…` imports updated to `use pennant::…`. The two HAL drivers stay on git (not yet on crates.io) so `pennant` is also kept as a git dep — sharing the source guarantees a single compiled copy and unifies `StatusLed` / `PulseEffect` across the HAL boundary.

## [0.2.0] - 2026-05-06

This release introduces an OTA MVP across both stacks, completes the April 2026 `esp-hal` upgrade wave, and switches bare-metal Wi-Fi to an async-only API built on `embassy-net`.

### Added

- OTA MVP — three new experimental crates (`ota-pure`, `rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`) for end-to-end firmware update
- Bare-metal async Wi-Fi via the new `embassy` Cargo feature on `rustyfarian-esp-hal-wifi`
- ESP-NOW peer discovery, reliable delivery, and the Peripheral Command Framework
- Wi-Fi TX power and power-save configuration in `wifi-pure`
- MQTT non-blocking publishes, `StatusLed` boot feedback, and configurable task stack / reconnect timeout

### Changed

- **BREAKING** — bare-metal Wi-Fi is now async-only; the synchronous `WiFiManager` surface and the `hal_*_connect` examples are gone
- **BREAKING** — `esp-radio 0.18` API renames cascade through `rustyfarian-esp-hal-wifi`
- April 2026 `esp-hal` stack wave: `esp-hal 1.1.0`, `esp-rtos 0.3.0`, `esp-radio 0.18.0`, plus matching embassy pins

### Fixed

- ESP-NOW channel-scan and `send_and_wait` race conditions

## [0.1.0] - 2026-03-16

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

[Unreleased]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/datenkollektiv/rustyfarian-network/releases/tag/v0.1.0
