# Roadmap

*Last updated: November 2026*

## Context

Both tiers run the September 2026 stack.
The bare-metal tier pins `esp-hal 1.2.2` and `esp-radio 1.0.0-beta.1`, a pre-release to re-pin to `1.0.0` when it ships.
The ESP-IDF tier pins `esp-idf-svc 0.53` and `esp-idf-hal 0.47`.
Tier parity (ADR 022) is the primary API goal, with both tiers on shared `juggler` cores.
For history, see `CHANGELOG.md` and `docs/features/archive/`.

## Forward plan

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

    Ready     : Tier parity — only the remaining Wi-Fi and MQTT name alignment is left after 0.7.0 (feature-doc)
              : Shared disconnect-reason vocabulary — one juggler DisconnectReason enum with name and code for both tiers, esp-hal maps esp-radio reasons locally (feature-doc)
              : ESP-NOW scan-burst TX power — scan boosts to max, near-field peers fail (feature-doc)
              : Portal defaults from build-time env — one helper builds the defaults for three consumers (feature-doc)
    Near term : Release 0.7.0 (done v0.7.0)
              : LoRa hardware validation deferred from 0.7.0 — Heltec V3 ESP32-S3 bench run, plus bug 007 (LoRa forces a critical-section provider)
              : LoRa pure-side polish — LoraConfig builder + from_hex_strings Result return
              : README 2D crate-status table — protocols × HAL tiers with maturity per cell, also fixes the stale stub text and the Wi-Fi/MQTT-only vision line
              : juggler scope ADR — document the cross-cutting catch-all rule for the consolidated pure crate
              : .cargo/config.toml setup detection — detect missing config in justfile, clearer first-build errors
              : LED/timeout dedup — extract the status-LED pulse and timeout-poll loop shared by the ESP-IDF Wi-Fi paths and MqttBuilder.build_and_wait
              : OTA download progress callback — user-facing progress for the OTA session
              : Upstream asks — esp-radio WifiController stop and start, esp-idf-svc inbound retain and QoS (from tier capability closure)

    Mid term  : LoRa TTN OTAA validation — first downlink (FPort 10) and session persistence remain (runbook)
              : OTA security model doc — threat model and signed-manifest question
              : Contract tests beyond Wi-Fi and MQTT — ADR 022 conformance suites for LoRa and ESP-NOW
              : LoRa post-adoption backlog — PartialEq on responses, heapless Deque FIFO in the mock, CRC-32 in restore_from_sleep, LorawanDevice process wiring
              : Portal peer MACs — ESP-NOW peers collected in the provisioning portal, plus an ESP-NOW-only profile, design questions open (feature-doc)
              : Boot policy helper — pure decision for the load, portal and restart flow, waits for a consumer (feature-doc)

    Long term : Full EspHalLoraRadio hardware driver (after TTN validation)
```

---

## Code deep-dive findings (June 2026, re-checked 2026-11-11)

The June review covered all crates before the juggler consolidation.
The rows were re-checked against the code on 2026-11-11 with the resolved ones removed.

|  # | Area                | Finding                                                                                     | Tracked                                                    |
|---:|:--------------------|:--------------------------------------------------------------------------------------------|:-----------------------------------------------------------|
|  7 | espnow-pure         | Both drivers build events with `EspNowEvent::try_new`, which never truncates; the public `EspNowEvent::new` still panics in debug and truncates in release, so deprecate it. | Hygiene batch |
|  9 | lora-pure           | juggler's `LorawanDevice::process()` still returns `NoUpdate` (the `PhyRxTx` bridge is unwired); the hardware-proven join path is `LorawanTimeline` plus `lorawan-device` in the ESP-IDF join example, so the wrapper is either wired or retired. | Mid term (LoRa, with the deferred hardware validation) |
| 10 | Pure layer          | Five juggler modules still return `&'static str` errors (wifi, provisioning boot and ssid, mqtt host and subscriptions) beside concrete enums and `LorawanError<E>`. | Near term (folded into pure-scope ADR)                     |

Positive findings worth keeping in mind (no action):

- `rustyfarian-esp-hal-network` OTA module's hand-rolled HTTP/1.1 parser is the security high-water mark (33 tests covering RFC 7230 smuggling vectors).
- MQTT's SUBACK-deadlock avoidance (resolved via `MqttBuilder::subscribe`) and its `Weak`-based event-loop shutdown are solid.
- ESP-NOW's failed-scan recovery keeps the last known channel by design; since ADR 025 a stale pin is corrected by the coordinator's probe reply on the next scan, and PeerLink rescans after 20 unacknowledged sends.

---

## Midterm detail

TTN v3 EU868 OTAA validation checklist: [runbooks/ttn-otaa-validation.md](runbooks/ttn-otaa-validation.md).

### Research

| # | Item                                                                                                                       |
|--:|:---------------------------------------------------------------------------------------------------------------------------|
| 1 | Evaluate ESP-IDF v5.5.5 — contains the v5.5.2 fix for "ESP-NOW send failure when coexistence is enabled"; requested by a downstream project, see `docs/features/esp-idf-v5-5-upgrade-v1.md` |
