# Roadmap

*Last updated: May 2026*

The bare-metal stack is aligned on the April 2026 esp-hal wave (`esp-hal 1.1.0` / `esp-radio 0.18.0` / `esp-rtos 0.3.0` / embassy 0.10), hardware-validated on ESP32-C3 and ESP32-C6.
The bare-metal Wi-Fi surface is async-only — `esp-radio 0.18` removed direct `smoltcp` integration and made the controller async-only, so `WiFiManager::init_async` + `AsyncWifiHandle` is the single public path.
TTN v3 LoRa validation remains blocked on hardware.
A May 2026 deep-dive review surfaced a set of workspace-hygiene and architecture-clarity items now tracked in Near and Mid term: README crate-status table, pure-crate scope ADR, OTA security model, contract tests, and `WifiDriver` trait documentation.

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

    Ready     : Finish hal_c3_connect_async hardware validation — AP reconnect loop + heap headroom (feature-doc)
              : MQTT startup message — `.with_startup_message()` opt-in on MqttBuilder (feature-doc)

    Near term : LoRa pure-side polish — LoraConfig builder + from_hex_strings Result return
              : README 2D crate-status table — protocols × HAL tiers with maturity per cell
              : rustyfarian-network-pure scope ADR — document the cross-cutting catch-all rule
              : .cargo/config.toml setup detection — detect missing config in justfile, clearer first-build errors

    Mid term  : Phase 5 — TTN v3 EU868 OTAA validation (blocked on hardware)
              : OTA security model doc — threat model, rollback policy, signed-manifest question
              : WifiDriver async/sync trait ADR — document trait duality + first paragraph of wifi-pure rustdoc
              : Contract tests in wifi-pure — run_contract_tests<D: WifiDriver>() conformance pattern (prototype, then replicate to LoRa + ESP-NOW)
              : LoRa post-adoption backlog — PartialEq, heapless Deque FIFO, CRC-32, hardware driver, state machine

    Long term : Full EspHalLoraRadio hardware driver (after TTN validation)
              : Async ESP-IDF MQTT decision ADR — thin ESP-IDF wrapper vs async-first design choice
              : rustyfarian-esp-hal-mqtt — minimq-based bare-metal MQTT (after async MQTT ADR)
```

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

Items #1 (builder) and #2 (`from_hex_strings` Result) promoted to Near term on 2026-05-06.

| # | Item                                                                              |
|--:|:----------------------------------------------------------------------------------|
| 4 | `PartialEq` on `LorawanResponse` / `Downlink`                                     |
| 5 | Replace manual O(n) FIFO shift in `MockLoraRadio::receive` with `heapless::Deque` |
| 6 | Implement CRC-32 integrity check in `restore_from_sleep` (Phase 7)                |
| 7 | Implement `EspHalLoraRadio` hardware driver (Phase 2-4 milestones)                |
| 8 | Wire `LorawanDevice::process()` state machine to `lorawan-device 0.12`            |

</details>
