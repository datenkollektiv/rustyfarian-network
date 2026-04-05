# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Pinned cross-repo `rustyfarian-ws2812` git dependencies (`led-effects`, `rustyfarian-esp-idf-ws2812`, `rustyfarian-esp-hal-ws2812`) to tag `v0.4.0`; v0.5.0 adopts the April 2026 esp-hal release wave (`esp-println 0.17`, `esp-backtrace ≥0.19`) which conflicts with this workspace's current esp-* baseline

### Added

- `espnow-pure`: `PeerTracker` — heartbeat-based peer liveness tracker with online/offline transition detection, extracted from rustbox-rgb-puzzle brain firmware
- `espnow-pure`: `ScanConfig::with_probe_timeout()` and `DEFAULT_PROBE_TIMEOUT` (100 ms) — per-channel probe timeout is now configurable
- `espnow-pure`: `ScanConfig::with_burst_timeout()` and `DEFAULT_BURST_TIMEOUT` (3 s) — bounds total time the radio spends at boosted TX power during peer discovery
- `wifi-pure`: `TxPowerLevel` enum (`Lowest`, `Low`, `Medium`, `High`, `Max`) with `to_quarter_dbm()` mapping to ESP-IDF quarter-dBm values; `WiFiConfig::with_tx_power()` builder method (see `docs/features/wifi-radio-power-config-v1.md`)
- `rustyfarian-esp-idf-wifi`: applies `TxPowerLevel` via `esp_wifi_set_max_tx_power()` after Wi-Fi start
- `rustyfarian-esp-hal-wifi`: stores `TxPowerLevel` config; logs warning that `esp-radio 0.17` does not expose TX power API
- `rustyfarian-esp-idf-espnow`: `scan_for_peer()` auto-bursts TX power to maximum during channel scanning, restores previous level after scan completes
- `espnow-pure`: `command` module — `CommandFrame<'a>` zero-copy parser, `SystemCommand` enum (`Ping`, `SelfTest`, `Identify`), tag range helpers, and response payload builders for the ESP-NOW Peripheral Command Framework (see `docs/features/espnow-peripheral-command-framework-v1.md`)
- Justfile: `check-wifi-hal-embassy` recipe that verifies the `embassy` feature compiles for ESP32-C6 (`riscv32imac-unknown-none-elf`) and ESP32-C3 (`riscv32imc-unknown-none-elf`)
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
- `espnow-pure`: `ScanConfig`, `ScanResult`, and `DEFAULT_SCAN_CHANNELS` types for ESP-NOW channel scanning configuration
- `rustyfarian-esp-idf-espnow`: `EspIdfEspNow::scan_for_peer()` probes Wi-Fi channels to find a peer by MAC-layer ACK (ADR 009)
- `rustyfarian-esp-idf-espnow`: `EspIdfEspNow::init_with_radio_scanning()` convenience constructor combining radio init and channel scan
- `rustyfarian-esp-idf-espnow`: `EspIdfEspNow::send_and_wait()` blocks until MAC-layer ACK via send callback — use for reliable delivery detection
- `rustyfarian-esp-idf-espnow`: `idf_c3_espnow_coordinator` and `idf_c3_espnow_scout` examples with onboard LED status feedback
- Justfile: `ESPFLASH_PORT` env var for multi-board setups, `fresh-run` and `erase-flash` recipes, `--non-interactive` monitor
- Build scripts: `*wifi*` and `*espnow*` crate auto-detection in `build-example.sh` and `flash.sh`

### Changed

- `rustyfarian-esp-idf-espnow`: `default_interface()` always returns `WifiInterface::Sta` — `init_with_radio()` starts in STA mode, not AP (fixes `ESP_ERR_ESPNOW_IF` on send)
- `sdkconfig.defaults`: added `CONFIG_ESP_WIFI_NVS_ENABLED=n` to prevent stale WiFi credential caching

### Fixed

- `rustyfarian-esp-idf-espnow`: `scan_for_peer()` and `send_and_wait()` now serialise their send-callback registration through an internal mutex — concurrent calls can no longer steal each other's ACKs
- `rustyfarian-esp-idf-espnow`: ACK wait now loops on the condvar to absorb spurious wakeups; previously a single `wait_timeout` could return early without an ACK
- `rustyfarian-esp-idf-espnow`: `idf_c3_espnow_scout` example — `parse_mac()` is now strict and returns `Result`; malformed MAC strings fail fast instead of silently substituting `0x00` for invalid hex digits
- `rustyfarian-esp-idf-espnow`: `idf_c3_espnow_coordinator` example — checks `esp_wifi_get_channel`/`esp_wifi_get_mac` return codes and logs a warning on failure instead of printing stale buffers
- `rustyfarian-esp-idf-espnow`: `scan_for_peer()` removes stale peer registration before scanning — fixes `ESP_ERR_ESPNOW_EXIST` on retry
- `rustyfarian-esp-idf-espnow`: channel scan uses `register_send_cb` with `Condvar` for real MAC-layer ACK detection — `esp_now_send()` is asynchronous and returns before the ACK arrives

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
