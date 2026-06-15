# Roadmap

*Last updated: June 2026*

The bare-metal stack is aligned on the April 2026 esp-hal wave (`esp-hal 1.1.0` / `esp-radio 0.18.0` / `esp-rtos 0.3.0` / embassy 0.10), hardware-validated on ESP32-C3 and ESP32-C6.
The bare-metal Wi-Fi surface is async-only — `esp-radio 0.18` removed direct `smoltcp` integration and made the controller async-only, so `WiFiManager::init_async` + `AsyncWifiHandle` is the single public path.
TTN v3 LoRa validation remains blocked on hardware.
A May 2026 deep-dive review surfaced a set of workspace-hygiene and architecture-clarity items now tracked in Near and Mid term: README crate-status table, pure-crate scope ADR, OTA security model, contract tests, and `WifiDriver` trait documentation.
June 2026 delivered the ESP-NOW Variant 1 (STA↔STA) and Variant 2 (SoftAP scout ↔ AP-connected coordinator) milestones, including ADR 012 documenting the background-scanner channel-drift root cause and the SoftAP fix.
A June 2026 full-code deep dive (all 13 crates) confirmed the pure-first architecture is consistently executed and surfaced a set of hygiene items — LED/timeout logic dedup, a CI hardware-tier build job, a LoRa RF-config mapping guard, and README staleness fixes — tracked in Near term and in the findings table below.
June 2026 shipped the provisioning triad — SoftAP captive-portal provisioning with two schema profiles (`LorawanFieldDevice` and `WifiMqttDevice`), NVS schema v2 with a `profile` discriminator, a `no_std`-safe surface on `rustyfarian-network-pure`, and the `idf_c3_provision` / `idf_c3_provision_mqtt` examples; per ADR 013 (acceptance) and ADR 014 (profile generalisation), end-to-end validated 2026-06-14, full detail in the CHANGELOG.

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

    Ready     : rustyfarian-esp-hal-provisioning — bare-metal SoftAP captive-portal provisioning for the WifiMqttDevice profile, Phases 0–3 (ADR 015, feature-doc)
              : Finish hal_c3_connect_async hardware validation — AP reconnect loop + heap headroom (feature-doc)

    Near term : LoRa pure-side polish — LoraConfig builder + from_hex_strings Result return
              : README 2D crate-status table — protocols × HAL tiers with maturity per cell, also fixes the stale stub description of rustyfarian-esp-hal-wifi and the Wi-Fi/MQTT-only vision line
              : rustyfarian-network-pure scope ADR — document the cross-cutting catch-all rule (including why it is the only non-no_std pure crate)
              : .cargo/config.toml setup detection — detect missing config in justfile, clearer first-build errors
              : ESP-NOW scan country-code awareness — query esp_wifi_get_country() at scan start and restrict probed channels to schan..schan+nchan-1 instead of hardcoded 1-13, eliminates ESP_ERR_WIFI_NOT_ALLOWED_CHANNEL warnings on US/FCC and other restricted-band regions
              : MQTT subscribe-during-shutdown race fix — detached subscriber thread spawned on Connected can block forever in esp_mqtt_client_subscribe if last MqttHandle is dropped before SUBACK, track in-flight subscriptions via a shared atomic counter, gate event-loop exit on count==0 so the loop keeps pumping events until SubscribeAck arrives, also close the window where is_connected() reads true before the subscriber thread has sent SUBSCRIBE
              : LED/timeout dedup — extract the shared status-LED pulse + timeout-poll loop duplicated between rustyfarian-esp-idf-wifi (blocking vs LED paths) and MqttBuilder.build_and_wait into one helper
              : LoRa RF-config mapping guard — make map_rf_config/cr_to_sx126x non-exhaustive-safe so new upstream lora-modulation variants return InvalidRfConfig instead of failing to compile or panicking
              : CI hardware-tier build job — compile one example per tier (Xtensa IDF + bare-metal) in CI to close the just-verify blind spot
              : rustyfarian-esp-idf-provisioning StoredConfig Debug redaction — the IDF tier's StoredConfig derives Debug over plaintext wifi_password and mqtt_pass, leaking credentials into any caller log line that formats the struct, the bare-metal store closes the same gap by construction via a manual Debug, the IDF tier needs the parallel manual impl with the same — redacted — pattern (surfaced by the Wave-3 security audit of Phase 1)

    Mid term  : Phase 5 — TTN v3 EU868 OTAA validation (blocked on hardware)
              : OTA security model doc — threat model, rollback policy, signed-manifest question
              : WifiDriver async/sync trait ADR — document trait duality + first paragraph of wifi-pure rustdoc
              : Contract tests in wifi-pure — generic run_contract_tests() over any WifiDriver implementation, conformance pattern (prototype, then replicate to LoRa + ESP-NOW)
              : LoRa post-adoption backlog — PartialEq, heapless Deque FIFO, CRC-32, hardware driver, state machine

    Long term : Full EspHalLoraRadio hardware driver (after TTN validation)
              : Async ESP-IDF MQTT decision ADR — thin ESP-IDF wrapper vs async-first design choice
              : rustyfarian-esp-hal-mqtt — minimq-based bare-metal MQTT (after async MQTT ADR)
```

---

## June 2026 code deep-dive findings

Full review of all 13 crates (~13k lines), the build scripts, and CI.
Overall verdict: the pure-first architecture is consistently executed (thin HAL wrappers, ~208 host tests in the pure layer, minimal and justified unsafe); the items below are the deltas worth fixing.
Items promoted to the timeline are marked; the rest are small enough to batch into a hygiene session.

|  # | Area                | Finding                                                                                                                                                                                                                                                                                  | Tracked                                                                       |
|---:|:--------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------|
|  1 | README              | `rustyfarian-esp-hal-wifi` still described as "stub; full implementation in progress" — it is a working async STA driver, hardware-validated on C3/C6; vision line still says "Wi-Fi and MQTT" though the workspace spans five protocols                                                 | Near term (folded into 2D crate-status table)                                 |
|  2 | CI                  | Only pure crates and the `riscv32imac-esp-espidf` check target are exercised; Xtensa IDF and bare-metal targets never build in CI                                                                                                                                                        | Near term (CI hardware-tier build job)                                        |
|  3 | esp-idf wifi + mqtt | Status-LED pulse cadence and timeout-poll loop duplicated in three places (`WiFiManager` blocking path, LED path, `MqttBuilder::build_and_wait`)                                                                                                                                         | Near term (LED/timeout dedup)                                                 |
|  4 | esp-idf-lora        | RF-config mapping (`map_rf_config`, SF/BW/CR converters) has no guard for new upstream `lora-modulation` enum variants                                                                                                                                                                   | Near term (mapping guard)                                                     |
|  5 | esp-idf-mqtt        | Currently open: `is_connected()` flips true before the detached subscriber thread has sent SUBSCRIBE — brief window where publishes race ahead of subscriptions (distinct from the resolved SUBACK-deadlock)                                                                             | Near term (folded into subscribe-race fix)                                    |
|  6 | esp-idf-espnow      | `pinned_channel` (`AtomicU8`, sentinel `u8::MAX`) is only updated on successful scans and is never explicitly invalidated on failed scans; failed-scan recovery reuses the last known channel, which is intentional but could become stale if peer/channel reality changes underneath it | Hygiene batch (review whether to keep as-is or add an explicit staleness TTL) |
|  7 | espnow-pure         | `EspNowEvent::new()` panics on oversized payload in debug but silently truncates in release — mode-dependent behaviour                                                                                                                                                                   | Hygiene batch                                                                 |
|  8 | esp-idf-ota         | Single all-or-nothing HTTP timeout (connect + read share one `Duration`); no user-facing progress callback                                                                                                                                                                               | Hygiene batch / defer until a downstream project needs it                     |
|  9 | lora-pure           | `LorawanDevice::process()` returns `NoUpdate` — `PhyRxTx` bridge unwired pending TTN hardware validation (known, documented HIGH RISK)                                                                                                                                                   | Mid term (Phase 5, already tracked)                                           |
| 10 | Pure layer          | Minor style drift: error types vary between `&'static str`, concrete enums, and generic `LorawanError<E>`; acceptable, candidate for the pure-scope ADR to codify                                                                                                                        | Near term (folded into pure-scope ADR)                                        |

Positive findings worth keeping in mind (no action): `rustyfarian-esp-hal-ota`'s hand-rolled HTTP/1.1 parser is the security high-water mark (33 tests covering RFC 7230 smuggling vectors); MQTT's SUBACK-deadlock avoidance (resolved via `MqttBuilder::subscribe`) and `Weak`-based event-loop shutdown are solid; ESP-NOW's failed-scan recovery restores both peer registration and the last-known-good channel (see #6 for the staleness trade-off).

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
