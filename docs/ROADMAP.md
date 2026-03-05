# Roadmap

This document tracks planned improvements and upcoming work for the `rustyfarian-network` workspace.

Items in **Planned** are accepted and queued for the next development cycle.
Completed items are documented in `CHANGELOG.md` and archived at the bottom of this file.

---

## Planned

### LoRa Radio — `rustyfarian-esp-idf-lora` crate

The `rustyfarian-esp-idf-lora` crate has been adopted into the workspace.
It provides the `LoraRadio` trait, `LorawanDevice<R>` Class A device, OTA downlink command parser,
session persistence types, and a `MockLoraRadio` test double (37 host-runnable tests).
The SX1262 hardware driver and the `lorawan-device` state machine bridge are stubs, gracefully degrading.

<details>
<summary><strong>Phase 5 — TTN v3 (EU868) OTAA validation</strong></summary>

The goal is end-to-end OTAA join + first uplink + first downlink with the least moving parts.
All steps use TTN v3 EU868.

**Step 0 — Credentials**

- Create a TTN application and register an end device (LoRaWAN MAC V1.0.3, RP001-1.0.3, OTAA).
- Record DevEUI (8 bytes), JoinEUI/AppEUI (8 bytes), AppKey (16 bytes).
- Decide byte order: TTN displays EUIs as big-endian strings; many stacks expect LSB-first in memory.
  Log DevEUI/JoinEUI as bytes and compare against `lorawan-device` documentation before flashing.
  See `docs/key-insights.md` — "EUI byte order" for the full pitfall description.

**Step 1 — Gateway & RF sanity**

- Confirm a TTN-connected EU868 gateway is online (TTN Console → Gateways → "connected recently").
- Place the device within metres for initial tests; use a correct EU868 antenna.

**Step 2 — SX1262 bring-up (before LoRaWAN)**

- Verify SPI mode 0, 8 MHz; confirm NSS/CS, BUSY, RESET, DIO1 pins.
- Issue a status/sanity command after reset and log the response.
- Confirm BUSY line goes high during operations and returns low; if BUSY is never handled,
  every SPI command stalls — see `docs/key-insights.md` — "BUSY pin".

**Step 3 — TTN Live Data setup**

- Open TTN Console → Application → End Device → Live Data (leave open during testing).
- Enable join-accept and uplink viewing; confirm gateway metadata (RSSI/SNR) is visible.

**Step 4 — OTAA join**

- Firmware must log "joining…" and then either "joined" or the failure reason.
- In Live Data, expect: join-request uplink(s) → join-accept downlink.
- If a join-request is visible but no join-accept: wrong AppKey or EUI byte order mismatch.
- If join-accept is visible in TTN but a device never joins: RX timing or DIO1 IRQ issue
  (see `docs/key-insights.md` — "DIO1 interrupt" and "RX window").
- Tune `RX_WINDOW_OFFSET_MS` if windows are missed; start at −200 ms and adjust upward.

**Step 5 — First uplink**

- After joining, send a small payload (1–8 bytes) on FPort 1.
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
- If `FCntUp` is reset or reused, TTN silently rejects the frames — see `docs/key-insights.md` — "Frame counter reuse".

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

<details>
<summary><strong>Post-adoption backlog (from code review)</strong></summary>

These were deferred from the initial adoption and can be addressed in follow-up PRs:

| # | Item                                                                              |
|--:|:----------------------------------------------------------------------------------|
| 1 | Builder pattern for `LoraConfig` (private fields, `::builder()`)                  |
| 2 | `from_hex_strings` returns `Result` with field-level diagnostics                  |
| 4 | `PartialEq` on `LorawanResponse` / `Downlink`                                     |
| 5 | Replace manual O(n) FIFO shift in `MockLoraRadio::receive` with `heapless::Deque` |
| 6 | Implement CRC-32 integrity check in `restore_from_sleep` (Phase 7)                |
| 7 | Implement `EspLoraRadio` hardware driver (Phase 2–4 milestones)                   |
| 8 | Wire `LorawanDevice::process()` state machine to `lorawan-device 0.12`            |

</details>

---

### no-std / esp-hal LoRa with ws2812 LED status

With `rustyfarian-esp-hal-ws2812` (v0.3.0) complete in the companion repository,
this workspace extends to a bare-metal LoRa path via a dedicated `rustyfarian-esp-hal-lora` crate.
Per ADR 005, mutually exclusive HAL backends require separate crates, not feature flags.
The target structure is:

```
lora-pure                      — no_std; LoraRadio trait, TxConfig, RxConfig, config types
rustyfarian-esp-idf-lora       — std; esp-idf-hal; anyhow errors (existing, refactored)
rustyfarian-esp-hal-lora       — no_std; esp-hal SPI + GPIO; custom error enum (new)
```

`rustyfarian-esp-hal-lora` accepts `S: StatusLed` for visual join / uplink / downlink feedback
via the WS2812 LED on the Heltec V3 board — or `NoLed` for headless configurations.

**Items**

- Extract shared types from `rustyfarian-esp-idf-lora` into a new `lora-pure` crate
- Update `rustyfarian-esp-idf-lora` to depend on `lora-pure`
- Create `rustyfarian-esp-hal-lora` crate with `EspHalLoraRadio<S: StatusLed>` stub
- Implement full `EspHalLoraRadio` using `esp-hal` SPI + GPIO
- Wire `LorawanDevice<EspHalLoraRadio<S>>` end-to-end (prerequisite for Phase 5 TTN validation)

---

### Grow `rustyfarian-network-pure`

Extract additional platform-independent logic into `rustyfarian-network-pure` so more behaviour
can be verified on the host without an ESP32 or ESP toolchain.

Candidates include reconnection backoff calculations, MQTT topic validation, and any other
pure functions that currently live inside the ESP-IDF crates but have no hardware dependency.

---

<details>
<summary><strong>Completed</strong></summary>

### MQTT Enhancements

Driven by [ADR 002](adr/002-mqtt-enhancements-for-downstream-project.md), the `rustyfarian-esp-idf-mqtt` crate was expanded with support for Last Will and Testament, authentication, multi-topic subscription, topic-based callback dispatch, and retained-message publishing.

- `LwtConfig` struct with `new()` constructor for Last Will and Testament support
- `MqttConfig::with_lwt()` builder for attaching an LWT configuration
- `MqttConfig::with_auth()` builder for broker authentication
- Multi-topic subscription: constructor accepts `&[&str]` instead of a single topic
- Topic-based dispatch: callback signature changes from `Fn(&[u8])` to `Fn(&str, &[u8])`
- `MqttManager::publish_with()` for explicit QoS and retain control
- `send_startup_message()` and `send_shutdown_message()` deprecated in favour of `publish_with()`

### Wi-Fi Reliability Fixes

Two issues reported by rustbox-backstage (vault-standalone firmware) were resolved in `rustyfarian-esp-idf-wifi`.

- `WiFiManager::get_ip` now treats transient `is_connected()` and `get_ip_info()` errors as "not ready yet" rather than propagating them; the polling loop continues until the timeout fires, honouring the documented `Ok(Some(ip))` / `Ok(None)` contract
- `ConnectMode` enum replaces the `connection_timeout_secs` field on `WiFiConfig`; timeout only lives inside `Blocking { timeout_secs }`, so it cannot be set in a context where it would have no effect
- `WiFiConfig::connect_nonblocking()` builder sets `NonBlocking` mode; `WiFiManager::new` fires `EspWifi::connect()` and returns immediately, letting the ESP-IDF event loop drive association in the background — see [ADR 003](adr/003-wifi-nonblocking-connect.md)

</details>
