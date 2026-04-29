# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **BREAKING** `rustyfarian-esp-hal-wifi`: removed the synchronous `WiFiManager::init`, `init_with_led`, `wait_connected`, `get_ip`, `take_sta_device`, and the `WifiDriver` trait impl, along with the `S: StatusLed` generic on `WiFiManager` and the `WifiError::{StartFailed, ConnectFailed, DisconnectFailed, RadioInitFailed}` variants — `esp-radio 0.18` removed direct `smoltcp` integration and made the bare-metal Wi-Fi controller async-only, so the sync surface no longer has a backing driver. The async path (`WiFiManager::init_async` + `AsyncWifiHandle`) is now the only public surface; the `embassy` Cargo feature is therefore effectively required alongside any chip feature
- **BREAKING** `rustyfarian-esp-hal-wifi`: removed the `hal_c3_connect`, `hal_c6_connect`, `hal_c3_wifi_raw`, `hal_c6_wifi_raw`, and `hal_c6_connect_nonblocking_rgb` examples — all depended on the deleted sync smoltcp path; the three remaining `hal_*_async*` examples cover ESP32-C3 + ESP32-C6 in headless and LED-feedback variants

### Added

- **OTA MVP** — three new crates for end-to-end firmware update on ESP32-C3 (and ESP32-C6 / ESP32 for the bare-metal stack), aligned with `docs/adr/011-ota-crate-hosting-and-transport.md` and `docs/features/ota-mvp-v1.md`. All public APIs are explicitly experimental for MVP; stabilization is owned by the future `ota-library` feature.
  - `ota-pure` (new crate, **experimental API**) — platform-independent, `no_std`, host-tested. Surface: `Version` semver parser (`u16` components, `Display`, `Ord`), `StreamingVerifier` (chunk-fed SHA-256 over `sha2 = { default-features = false }`), `bytes_to_hex` / `hex_to_bytes` fixed-size helpers (returns `heapless::String<64>`), `ImageMetadata` sidecar parser (`.bin.sha256` + `.bin.version`), backend-neutral `OtaState` enum (`Idle → Downloading → Verifying → Writing → SwapPending → Booted`) with `next_state()`, and `OtaError` with the 8 MVP variants (`ServerUnreachable`, `DownloadFailed { status: u16 }`, `DownloadTimeout`, `ChecksumMismatch`, `VersionInvalid`, `FlashWriteFailed`, `PartitionNotFound`, `InsufficientSpace`). `DownloadFailed { status: 0 }` is reserved as a sentinel for protocol-shape rejections from the bare-metal HTTP client. 37 host unit tests.
  - `rustyfarian-esp-idf-ota` (new crate, **experimental API**, blocking) — ESP-IDF std, lifted from `rustyfarian-beekeeper/src/ota/`. Surface: `OtaSession::new(config)`, `fetch_and_apply(url, &expected_sha256)`, `mark_valid()`, `rollback()`. Wraps `EspOta` / `EspOtaUpdate` and `EspHttpConnection`; streams download → SHA-256 verify (`StreamingVerifier`) → flash → swap in one pass without holding the full image in RAM. Strips RFC 3986 userinfo from URLs before logging (no credential leakage to `espflash monitor`). HTTPS rejected at MVP scope per ADR 011 (`ota-hardened` will revisit).
  - `rustyfarian-esp-hal-ota` (new crate, **experimental API**, async-only) — bare-metal `no_std`, built fresh against `esp_bootloader_esp_idf::OtaUpdater` over `esp-storage` and `embassy-net::TcpSocket`. Surface: `EspHalOtaManager::new(config, FLASH<'d>)`, `async fetch_and_apply(socket, url, &expected_sha256)`, `mark_valid()`, `rollback()`. Carries an internal hand-rolled HTTP/1.1 GET parser (per ADR 011 §2): accepts only `HTTP/1.1 200 OK` with exactly one valid `Content-Length`; rejects redirects, `Transfer-Encoding: chunked`/`identity`, missing or duplicate `Content-Length`, non-`1*DIGIT` numeric values (incl. leading `+`/`-`), whitespace before colon, oversized bodies, and short reads. Chip features `esp32c3` (MVP), `esp32c6`, `esp32`; stack features `unstable`, `rt`, `embassy`. Host stub mirrors the wifi-crate pattern (typecheck-only). 29 parser unit tests.
- Workspace: `sha2 = { version = "0.10", default-features = false }`, `esp-storage = "=0.9.0"`, `embedded-storage = "0.3"` added to `[workspace.dependencies]`; `embedded-svc = "0.29"` declared (aligning with `esp-idf-svc 0.52`).
- Justfile: `check-ota-pure`, `test-ota`, `check-ota-idf`, `check-ota-hal`, `check-ota-hal-embassy`, `test-ota-hal` recipes; `test` aggregate extended.
- CI (`.github/workflows/rust.yml`): host tests for `ota-pure` and `rustyfarian-esp-hal-ota --no-default-features` added to the "Test pure crates" block.
- Build scripts: `scripts/detect-port.sh` narrows `espflash`'s auto-detect to USB serial devices (`usbmodem*`/`usbserial*` on macOS, `ttyUSB*`/`ttyACM*` on Linux) so paired Bluetooth ports stop hijacking the probe; used by `flash.sh`, `just run`, `just monitor`, and `just erase-flash`. `ESPFLASH_PORT=…` still wins when set explicitly
- `rustyfarian-esp-hal-wifi`: `embassy` Cargo feature + `WiFiManager::init_async()` returning an `AsyncWifiHandle { controller, stack, runner }` wired into an `embassy-net` stack with automatic DHCPv4 (`AsyncWifiHandle::wait_for_ip().await` awaits the first lease). Originally landed alongside a synchronous `WiFiManager::init` path that drove `smoltcp` directly; that sync path was removed later in this same release cycle when the stack moved to `esp-radio 0.18` (see the breaking-change entry below). The `embassy` feature is now the only supported Wi-Fi path on bare-metal — see `docs/features/embassy-feature-flag-v1.md` and `docs/features/wifi-manager-async-v1.md`
- `rustyfarian-esp-hal-wifi`: `hal_c3_connect_async` example — first async bare-metal Wi-Fi demo on ESP32-C3, uses `#[esp_rtos::main]` with two spawned tasks (`wifi_task` for association + reconnection, `net_task` for the embassy-net runner), prints the DHCP-assigned IP and idles asynchronously (see `docs/features/hal-c3-connect-async-example-v1.md`)
- Build scripts: `scripts/build-example.sh` now appends the `embassy` feature automatically for any `hal_*_async*` example
- Justfile: `check-wifi-hal-embassy` recipe that verifies the `embassy` feature compiles for ESP32-C6 (`riscv32imac-unknown-none-elf`) and ESP32-C3 (`riscv32imc-unknown-none-elf`)
- `espnow-pure`: `PeerTracker` — heartbeat-based peer liveness tracker with online/offline transition detection, extracted from rustbox-rgb-puzzle brain firmware
- `espnow-pure`: `ScanConfig::with_probe_timeout()` and `DEFAULT_PROBE_TIMEOUT` (100 ms) — per-channel probe timeout is now configurable
- `espnow-pure`: `ScanConfig::with_burst_timeout()` and `DEFAULT_BURST_TIMEOUT` (3 s) — bounds total time the radio spends at boosted TX power during peer discovery
- `wifi-pure`: `TxPowerLevel` enum (`Lowest`, `Low`, `Medium`, `High`, `Max`) with `to_quarter_dbm()` mapping to ESP-IDF quarter-dBm values; `WiFiConfig::with_tx_power()` builder method (see `docs/features/wifi-radio-power-config-v1.md`)
- `rustyfarian-esp-idf-wifi`: applies `TxPowerLevel` via `esp_wifi_set_max_tx_power()` after Wi-Fi start
- `rustyfarian-esp-hal-wifi`: stores `TxPowerLevel` config; logs a warning that `esp-radio` does not expose a TX power API on bare-metal targets (true on both 0.17 and 0.18 — the radio default applies)
- `rustyfarian-esp-idf-espnow`: `scan_for_peer()` auto-bursts TX power to maximum during channel scanning, restores previous level after scan completes
- `espnow-pure`: `command` module — `CommandFrame<'a>` zero-copy parser, `SystemCommand` enum (`Ping`, `SelfTest`, `Identify`), tag range helpers, and response payload builders for the ESP-NOW Peripheral Command Framework (see `docs/features/espnow-peripheral-command-framework-v1.md`)
- `rustyfarian-esp-hal-wifi`: `ActiveLowLed<P>` adapter — implements `StatusLed` with inverted polarity for onboard LEDs wired active-low (e.g. ESP32-C3 Super Mini GPIO8)
- `rustyfarian-esp-hal-wifi`: `hal_c3_connect_async_led` example — async Wi-Fi connect with spawned `led_task` that blinks the onboard GPIO8 LED during connection, holds steady once IP acquired; uses `AtomicBool` for task coordination
- `rustyfarian-esp-hal-wifi`: `hal_c6_connect_async_led` example — async Wi-Fi connect with spawned `led_task` that pulses the onboard WS2812 RGB LED (GPIO8) blue via `PulseEffect` during connection, holds dim green once connected
- Build scripts: `build-example.sh` and `flash.sh` auto-detect `rustyfarian-esp-hal-ws2812` feature for `hal_c6_*_led*` examples
- Justfile: `check-wifi-hal-embassy` recipe that verifies the `embassy` feature compiles for ESP32-C6 (`riscv32imac-unknown-none-elf`) and ESP32-C3 (`riscv32imc-unknown-none-elf`)
- `espnow-pure`: `PeerTracker` — heartbeat-based peer liveness tracker with online/offline transition detection, extracted from rustbox-rgb-puzzle brain firmware
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

- **April 2026 esp-hal stack wave** — coordinated workspace-wide bump of every bare-metal crate to exact pins (`Cargo.toml` `[workspace.dependencies]`): `esp-hal 1.0.0`→`=1.1.0`, `esp-rtos 0.2.0`→`=0.3.0`, `esp-radio 0.17.0`→`=0.18.0`, `esp-bootloader-esp-idf 0.4.0`→`=0.5.0`, `esp-alloc 0.9.0`→`=0.10.0`, `esp-println 0.16.1`→`=0.17.0`, `esp-backtrace 0.18.1`→`=0.19.0`; embassy ecosystem `embassy-executor 0.9`→`=0.10.0`, `embassy-net 0.7`→`=0.8.0`, `embassy-time 0.5`→`=0.5.1`, `embassy-sync` newly pinned at `=0.8.0`. `smoltcp` is now exact-pinned at `=0.12.0` but is no longer a direct dependency of `rustyfarian-esp-hal-wifi` — `esp-radio 0.18` removed the `smoltcp` feature in favour of `embassy-net-driver`. Coordinated with `rustyfarian-ws2812`'s April 2026 wave (see `docs/features/esp-hal-stack-upgrade-april-2026-v1.md`)
- ws2812 cross-repo dependencies (`led-effects`, `rustyfarian-esp-idf-ws2812`, `rustyfarian-esp-hal-ws2812`) re-pinned from `tag = "v0.4.0"` to `tag = "v0.5.0"` — v0.5.0 is the April 2026 wave release that ships the matching exact pins for `esp-hal`/`esp-rtos`/`esp-radio`, so the resolved feature graph stays unified with this workspace's bare-metal stack
- **BREAKING** `rustyfarian-esp-hal-wifi`: API renames flowing through from `esp-radio 0.18` — `WifiDevice` → `Interface`, `ModeConfig::Client(ClientConfig)` → `Config::Station(StationConfig)`, `Interfaces.sta` → `.station`, `WifiEvent::StaDisconnected` → `StationDisconnected`, `WifiError::Disconnected` is now a tuple variant carrying `DisconnectedStationInfo`, `controller.is_connected()` returns `bool` directly (not `Result`), `controller.connect()`/`disconnect()` are async-only (`connect_async`/`disconnect_async`), `controller.wait_for_event(StaDisconnected)` becomes `wait_for_disconnect_async`, `esp_radio::wifi::new()` is now `(WIFI, ControllerConfig)` (the radio init parameter is gone — radio init is implicit)
- **BREAKING** `rustyfarian-esp-hal-wifi`: `set_config` is now idempotent in `esp-radio 0.18` and implicitly starts the controller and initiates association — the explicit `start()`/`connect()` calls that existed in 0.17 are no longer needed (or available)
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
