# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Pinned cross-repo `rustyfarian-ws2812` git dependencies (`led-effects`, `rustyfarian-esp-idf-ws2812`, `rustyfarian-esp-hal-ws2812`) to tag `v0.4.0`; v0.5.0 adopts the April 2026 esp-hal release wave (`esp-println 0.17`, `esp-backtrace ≥0.19`) which conflicts with this workspace's current esp-* baseline

### Added

- `rustyfarian-esp-hal-wifi`: `EspHalWifiManager` with real `WifiDriver` implementation using `esp-radio 0.17.0` for bare-metal ESP32-C3/C6 (ADR 006 Phase 5); `hal_c3_connect` and `hal_c6_connect` examples
- `rustyfarian-network-pure`: `status_colors` module with shared LED colour palette (`BOOT`, `WIFI_CONNECTING`, `MQTT_CONNECTING`, `CONNECTED`, `ERROR`, `OFFLINE`)
- `rustyfarian-esp-idf-mqtt`: `MqttBuilder::build_and_wait()` with `StatusLed` support for visual boot feedback (cyan pulse while connecting, green on success, red on timeout)
- `rustyfarian-esp-idf-mqtt`: non-blocking `MqttHandle::try_publish`, `try_publish_retained`, and `try_publish_with` with `TryPublishError` for time-critical loops
- `wifi-pure`: `WifiPowerSave` enum (`None`, `MinModem`, `MaxModem`) and `WiFiConfig::with_power_save()` builder method
- `rustyfarian-esp-idf-wifi`: applies configured power save mode via `esp_wifi_set_ps()` after Wi-Fi start
- `rustyfarian-esp-idf-espnow`: `EspIdfEspNow::init_with_radio()` starts and owns the Wi-Fi radio for ESP-NOW-only devices (ADR 008)
- `rustyfarian-esp-idf-espnow`: `EspIdfEspNow::default_interface()` returns the correct `WifiInterface` based on init mode
- `espnow-pure`: `PeerConfig::with_ap_interface()` builder method for ESP-NOW-only devices
- `rustyfarian-esp-idf-mqtt`: configurable MQTT task stack size via `MqttConfig::with_task_stack_size()`; default raised to 8 KiB (from ESP-IDF's 6 KiB) to prevent TLS stack overflow
- `rustyfarian-esp-idf-mqtt`: configurable reconnect interval via `MqttConfig::with_reconnect_timeout()` for battery-powered and thermally constrained devices (default: ESP-IDF 10 s)

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
