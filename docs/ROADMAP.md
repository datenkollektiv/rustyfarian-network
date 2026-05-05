# Roadmap

*Last updated: May 2026*

The bare-metal stack is now aligned on the April 2026 esp-hal wave (`esp-hal 1.1.0` / `esp-radio 0.18.0` / `esp-rtos 0.3.0` / embassy 0.10), hardware-validated on ESP32-C3 and ESP32-C6.
The bare-metal Wi-Fi surface is now async-only — `esp-radio 0.18` removed direct `smoltcp` integration and made the controller async-only, so `WiFiManager::init_async` + `AsyncWifiHandle` is the single public path.
The OTA MVP three-crate triad (`ota-pure`, `rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`) has landed on `main` per [ADR 011](adr/011-ota-crate-hosting-and-transport.md) and [`docs/features/archive/ota-mvp-v1.md`](features/archive/ota-mvp-v1.md); all public APIs are explicitly marked experimental and remain unreleased until v0.2.0.
TTN v3 LoRa validation remains blocked on hardware.
Next milestone: cut v0.2.0 bundling the accumulated post-0.1.0 features (April 2026 stack upgrade, async Wi-Fi, OTA MVP, command framework, power-save, ESP-NOW channel scanning).

```mermaid
%%{init: {
  "theme": "base",
  "themeVariables": {
    "cScale0": "#e8f5e9",
    "cScaleLabel0": "#2e7d32",
    "cScale1": "#c8f7c5",
    "cScaleLabel1": "#1b5e20",
    "cScale2": "#fff3cd",
    "cScaleLabel2": "#7a5a00",
    "cScale3": "#e3f2fd",
    "cScaleLabel3": "#0d47a1"
  }
}}%%

timeline
    title rustyfarian-network Roadmap

    Near term : Release v0.2.0 — EspHalWifiManager, async Wi-Fi on the April 2026 esp-hal wave, OTA MVP triad, status_colors, non-blocking publish, power save, ESP-NOW channel scanning, command framework

    Mid term  : Phase 5 — TTN v3 EU868 OTAA validation (blocked on hardware)
              : LoRa post-adoption backlog — builder pattern, CRC-32, hardware driver, state machine

    Long term : Evaluate ESP-IDF v5.5.2 coex fix for ESP-NOW send failures
              : Full EspHalLoraRadio hardware driver (after TTN validation)
              : rustyfarian-esp-hal-mqtt — minimq-based bare-metal MQTT (esp-hal WiFi dependency resolved)
```

---

## Completed

<details>
<summary><strong>Delivered since v0.1.0</strong></summary>

- `EspHalWifiManager` with `esp-radio 0.17.0` — full `WifiDriver` impl, `WiFiConfigExt`, `wait_connected` with DHCP, `hal_c3_connect` / `hal_c6_connect` examples (ADR 006 Phases 1-6)
- Unified `WiFiManager::init(config)` API across ESP-IDF and esp-hal crates
- `validate_ssid` rejects empty SSIDs (shared in `wifi-pure`)
- `status_colors` module in `rustyfarian-network-pure`
- `MqttBuilder::build_and_wait()` with `StatusLed` support
- Non-blocking `MqttHandle::try_publish` / `try_publish_retained` / `try_publish_with`
- `WifiPowerSave` enum and `WiFiConfig::with_power_save()`
- `EspIdfEspNow::init_with_radio()` for ESP-NOW-only devices (ADR 008)
- Configurable MQTT task stack size (default raised to 8 KiB)
- Justfile cleanup: removed convenience recipes, consolidated `act` into single recipe with optional job arg
- `scan_for_peer()` and `send_and_wait()` for ESP-NOW channel scanning with MAC-layer ACK detection (ADR 009)
- `idf_c3_espnow_coordinator` and `idf_c3_espnow_scout` examples with LED feedback
- `default_interface()` fix: always STA (amends ADR 008)
- `CONFIG_ESP_WIFI_NVS_ENABLED=n` to prevent stale WiFi credential caching
- Wi-Fi Radio Power Config v1 — `TxPowerLevel` enum, `with_tx_power()` builder, ESP-IDF `esp_wifi_set_max_tx_power()`, ESP-NOW auto-burst during scanning
- ESP-NOW Peripheral Command Framework v1 — `CommandFrame` zero-copy parser, `SystemCommand` enum (Ping/SelfTest/Identify), response helpers in `espnow-pure`
- WiFiManager LED integration for esp-hal — `ActiveLowLed<P>` adapter, `hal_c3_connect_async_led` and `hal_c6_connect_async_led` examples (StatusLed support matching ESP-IDF; the synchronous `init_with_led` was later removed when the stack moved to esp-radio 0.18 — LED feedback now wires via spawned tasks alongside `init_async`)
- esp-hal Stack Upgrade — April 2026 wave: workspace exact-pinned to `esp-hal 1.1.0`, `esp-rtos 0.3.0`, `esp-radio 0.18.0`, `esp-bootloader-esp-idf 0.5.0`, `esp-alloc 0.10.0`, `esp-println 0.17.0`, `esp-backtrace 0.19.0`, `embassy-executor 0.10.0`, `embassy-net 0.8.0`, `embassy-time 0.5.1`, `embassy-sync 0.8.0`, `smoltcp 0.12.0`. `rustyfarian-esp-hal-wifi` collapsed to async-only (`WiFiManager::init_async` + `AsyncWifiHandle`) — sync surface and direct `smoltcp` integration removed (BREAKING for the bare-metal Wi-Fi consumers; `embassy` feature is now effectively required). Hardware-validated on ESP32-C3-DevKitM-1 and ESP32-C6-DevKitC-1; LoRa example builds clean for ESP32-S3 (Phase 5 hardware run separate). Tooling: `scripts/detect-port.sh` filters espflash's auto-detect to USB serial devices on macOS. See `docs/features/archive/esp-hal-stack-upgrade-april-2026-v1.md`.
- OTA MVP — three-crate dual-stack firmware update: `ota-pure` (no_std, host-tested), `rustyfarian-esp-idf-ota` (blocking, ESP-IDF), `rustyfarian-esp-hal-ota` (async, bare-metal via `embassy-net` + `esp_bootloader_esp_idf::OtaUpdater`). Streaming SHA-256 verify, partition swap, rollback; strict internal HTTP/1.1 GET parser. All public APIs explicitly experimental (locked by [ADR 011](adr/011-ota-crate-hosting-and-transport.md) and [`docs/features/archive/ota-mvp-v1.md`](features/archive/ota-mvp-v1.md)).

</details>

<details>
<summary><strong>Delivered in v0.1.0</strong></summary>

- `wifi-pure`, `lora-pure`, `espnow-pure` — platform-independent crates with traits and mocks
- `rustyfarian-esp-idf-lora` with `LoraRadioAdapter` bridging to `lorawan-device 0.12`
- `rustyfarian-esp-idf-espnow` — ESP-NOW driver implementing `EspNowDriver` trait
- `MqttBuilder` API with lifecycle callbacks, LWT, auth
- `rustyfarian-network-pure` — MQTT validation, state machine, `ExponentialBackoff`
- Dual-HAL script infrastructure (`build-example.sh`, `flash.sh`)
- CI: pure-crate test job for all host tests

</details>

---

## Near term detail

### OTA MVP — Three-Crate Dual-Stack Firmware Update

Locked by [ADR 011](adr/011-ota-crate-hosting-and-transport.md) and detailed in [`docs/features/archive/ota-mvp-v1.md`](features/archive/ota-mvp-v1.md).
Requested by `rustyfarian-ferriswheel-demo` (sibling repo).
All public APIs are explicitly marked experimental for the MVP; stabilization is owned by the future `ota-library` feature.

**Stage gates** (consumer pulls each stage via path-deps as it lands):

|  # | Crate                     | Owner         | Pass criteria                                                                                                            |
|---:|:--------------------------|:--------------|:-------------------------------------------------------------------------------------------------------------------------|
|  1 | `ota-pure`                | network repo  | `cargo test -p ota-pure --features mock` passes on host; `just verify` clean                                             |
|  2 | `rustyfarian-esp-idf-ota` | consumer demo | ESP32-C3 firmware fetches over Wi-Fi, validates SHA-256, swaps, reboots, calls `mark_valid`; truncated `.bin` rolls back |
|  3 | `rustyfarian-esp-hal-ota` | consumer demo | Identical hardware behaviour to Stage 2 on the same ESP32-C3 board, bare-metal async path                                |

**MVP scope (locked by feature request + ADR 011):**

- Plain HTTP/1.1 only — no TLS (deferred to `ota-hardened`)
- Streaming SHA-256 integrity check — never holds the full image in RAM
- Bare-metal HTTP client is hand-rolled and **internal** (strict subset: `HTTP/1.1 200 OK` + single `Content-Length`, rejects redirects/chunked/oversized) — not a workspace HTTP API
- Demo enables bootloader rollback (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`) and marks the running image valid after Wi-Fi association + 30 s wall-clock dwell
- 1 MiB OTA slot reference layout for ESP32-C3 SuperMini (4 MiB flash); demo owns the actual `partitions.csv`

**Out of scope (deferred to `ota-hardened` / `ota-library`):**

- TLS / HTTPS, Ed25519 signature verification, brown-out safety, automated rollback test in CI, multi-device concurrency, API stabilization, mock implementation

Once Stage 3 lands, this entry moves into the Completed band.

---

## Mid term detail

### Phase 5 — TTN v3 EU868 OTAA validation

<details>
<summary><strong>Validation checklist</strong></summary>

The goal is end-to-end OTAA join + first uplink + first downlink with the least moving parts.
All steps use TTN v3 EU868.

**Step 0 — Credentials**

- Create a TTN application and register an end device (LoRaWAN MAC V1.0.3, RP001-1.0.3, OTAA).
- Record DevEUI (8 bytes), JoinEUI/AppEUI (8 bytes), AppKey (16 bytes).
- Decide byte order: TTN displays EUIs as big-endian strings; many stacks expect LSB-first in memory.
  Log DevEUI/JoinEUI as bytes and compare against `lorawan-device` documentation before flashing.
  See `docs/project-lore.md` — "EUI byte order" for the full pitfall description.

**Step 1 — Gateway & RF sanity**

- Confirm a TTN-connected EU868 gateway is online (TTN Console → Gateways → "connected recently").
- Place the device within metres for initial tests; use a correct EU868 antenna.

**Step 2 — SX1262 bring-up (before LoRaWAN)**

- Verify SPI mode 0, 8 MHz; confirm NSS/CS, BUSY, RESET, DIO1 pins.
- Issue a status/sanity command after reset and log the response.
- Confirm BUSY line goes high during operations and returns low; if BUSY is never handled,
  every SPI command stalls — see `docs/project-lore.md` — "BUSY pin".

**Step 3 — TTN Live Data setup**

- Open TTN Console → Application → End Device → Live Data (leave open during testing).
- Enable join-accept and uplink viewing; confirm gateway metadata (RSSI/SNR) is visible.

**Step 4 — OTAA join**

- Firmware must log "joining..." and then either "joined" or the failure reason.
- In Live Data, expect: join-request uplink(s) → join-accept downlink.
- If a join-request is visible but no join-accept: wrong AppKey or EUI byte order mismatch.
- If join-accept is visible in TTN but a device never joins: RX timing or DIO1 IRQ issue
  (see `docs/project-lore.md` — "DIO1 interrupt" and "RX window").
- Tune `RX_WINDOW_OFFSET_MS` if windows are missed; start at -200 ms and adjust upward.

**Step 5 — First uplink**

- After joining, send a small payload (1-8 bytes) on FPort 1.
- TTN Live Data should show the uplink with decoded payload bytes and RSSI/SNR.
- Do not send it before join completes; `LorawanDevice::send()` guards this but the guard
  will need to hold once the real state machine is wired.

**Step 6 — First downlink (port 10 OTA commands)**

- In TTN Console, schedule a downlink: FPort 10, payload `01` (CheckUpdate).
- Trigger an uplink first — downlinks only arrive in RX windows after an uplink.
- Confirm `parse_ota_command()` receives the payload.
- Test additional commands: `05` (ReportVersion), `02 01 02 03` (UpdateAvailable 1.2.3).

**Step 7 — Deep sleep / session persistence (Phase 7 readiness)**

- After join, persist `LorawanSessionData` (CRC-32 check: implement before relying on restore).
- Sleep and wake; confirm TTN accepts subsequent uplinks with incremented `FCntUp`.
- If `FCntUp` is reset or reused, TTN silently rejects the frames — see `docs/project-lore.md` — "Frame counter reuse".

**Common pitfalls quick reference**

| Symptom                                    | Likely cause                                           |
|:-------------------------------------------|:-------------------------------------------------------|
| Join-request visible, no join-accept       | Wrong AppKey or EUI byte order mismatch                |
| Join-accept in TTN, device stays `Joining` | RX window timing off, or DIO1 IRQ not delivered        |
| All SPI commands stall / timeout           | BUSY pin not polled before each command                |
| Downlinks queued but never received        | No uplink to open the RX window; or wrong FPort        |
| Post-sleep uplinks rejected by TTN         | `FCntUp` reset to 0 (session key/counter not restored) |
| Never joins but gateway is nearby          | Wrong frequency plan (US915 vs EU868) or no antenna    |

</details>

### Research

| # | Item                                                                                                                       |
|--:|:---------------------------------------------------------------------------------------------------------------------------|
| 1 | Evaluate ESP-IDF v5.5.2 — contains fix for "ESP-NOW send failure when coexistence is enabled"; track for next ESP-IDF bump |

### LoRa post-adoption backlog

<details>
<summary><strong>Deferred items from initial adoption</strong></summary>

| # | Item                                                                              |
|--:|:----------------------------------------------------------------------------------|
| 1 | Builder pattern for `LoraConfig` (private fields, `::builder()`)                  |
| 2 | `from_hex_strings` returns `Result` with field-level diagnostics                  |
| 4 | `PartialEq` on `LorawanResponse` / `Downlink`                                     |
| 5 | Replace manual O(n) FIFO shift in `MockLoraRadio::receive` with `heapless::Deque` |
| 6 | Implement CRC-32 integrity check in `restore_from_sleep` (Phase 7)                |
| 7 | Implement `EspHalLoraRadio` hardware driver (Phase 2-4 milestones)                |
| 8 | Wire `LorawanDevice::process()` state machine to `lorawan-device 0.12`            |

</details>
