---
id: 022
title: ESP-IDF ESP-NOW coordinator replies are mostly unacknowledged by the C3 scout
captured-on: 2026-11-11
doc-version: 4
status: closed
kind: defect
---

# Bug 022: ESP-IDF ESP-NOW coordinator replies are mostly unacknowledged by the C3 scout

## Symptom
`idf_c6_espnow_coordinator` receives every frame of the C3 scout, but only about 12 % of its replies are acknowledged (`rx=121 reply_acked=11 reply_failed=110`).

## Suspected Cause
- Main cause, supported by an alternating A/B series: the C3 Super Mini scout transmitting at the ESP-IDF driver's 20 dBm.
- The C3's MAC ACKs are transmissions too, so a corrupted ACK shows up as the coordinator's reply ending `NoAck`.
- Ruled out one variable at a time: A-MPDU, a 5 ms reply delay, a coordinator reset, C6 uptime (fresh and settled pass on both tiers at 13 dBm).
- Unexplained: one 13 dBm run at 22:47 against a freshly flashed ESP-IDF C6 never found the coordinator.

| Scout TX power (read back at boot) | Fresh scout runs tonight | Clean passes |
|:--|:--|:--|
| 80 (20 dBm, driver default) | 5 | 1 |
| 52 (13 dBm, shared owned-radio default) | 10 | 9 |

- Alternating series 23:05 to 23:13, same ESP-IDF C6 coordinator: 80 fail (never found), 52 pass, 80 pass, 52 pass, 80 fail (send failed after 20 ACKs), 52 pass.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/examples/idf_c6_espnow_coordinator.rs`
- `examples-shared/espnow_echo_app.rs` (`REPLY_TIMEOUT`, `send_acked`)
- `crates/rustyfarian-esp-idf-network/src/espnow/mod.rs` and `worker.rs` (send path, `peer_add_raw`)
- Bench check `bench/c3/espnow-power-cycle.toml`, log `bench/c3/logs/idf_c3_espnow_scout-20261111-215509.log`

## Reproduction Confidence
high

## Severity
high

## Environment
- Coordinator: ESP32-C6 (`40:4c:ca:47:97:28`), `idf_c6_espnow_coordinator`, ESP-IDF v5.3.3, station on the `iot` mesh, channel 11, country 01.
- Scout: ESP32-C3 Super Mini (`A0:F2:62:A6:7B:5C`), `idf_c3_espnow_scout` default scenario, owned SoftAP radio pinned to channel 11.
- Branch `prepare-release-bench`, uncommitted Wi-Fi country work (ADR 026).

## Expected Behaviour
Nearly every reply is acknowledged, as with `hal_c6_espnow_sta_coordinator` (`rx=61 reply_acked=57 reply_failed=4`, the 4 during boot).

## Actual Behaviour
- About 88 % of replies end `NoAck` with no `[app] reply … failed` warning, steadily over 5 minutes.
- The scout's own sends to the ESP-IDF coordinator fail now and then (`TX #N failed: peer did not ACK`, every 7 to 40 s), each refound on channel 11 within 0.2 s.

## Reproduction Steps
1. `ESPFLASH_PORT=<c6 port> just run idf_c6_espnow_coordinator` and wait for `Operating on Wi-Fi channel 11`.
2. `COORDINATOR_MAC=40:4c:ca:47:97:28 just bench c3 espnow-power-cycle --port <c3 port>`.
3. Watch the coordinator's `[app] rx=… reply_acked=… reply_failed=…` line every 10 s.
4. Flash `hal_c6_espnow_sta_coordinator` instead and compare the same line.

## Suggested Fix Area
- Fixed: both tiers' owned radios take `EspNowConfig::owned_tx_power_quarter_dbm()` (13 dBm unless configured) from the shared `juggler` config.
- The scout's `ESPNOW_TX_POWER` build setting and boot line `radio TX power …` let a later run bisect the power again.

## Owner

## Links
- Runbook gate C3-I1 in `docs/runbooks/release-0.7.0-hardware-test.md` (passed 2026-11-11 23:37)
- ADR 025 (probe reply), ADR 026 (Wi-Fi country)

## Session Log
- 2026-11-11 — Captured as a defect from the C3-I1 bench run; A-MPDU ruled out by experiment, esp-hal coordinator swap localises it to the ESP-IDF reply path
- 2026-11-11 — TX power looked like the cause after one A/B pair, then the 13 dBm build failed against a freshly flashed C6; root cause open, C6 uptime is the leading variable
- 2026-11-11 — C6-uptime sweep passed 4/4 on both tiers (hypothesis out); alternating A/B series 20 vs 13 dBm: 1 of 3 vs 3 of 3 clean; TX power is the main cause, one 13 dBm failure unexplained; close after the attended C3-I1 gate
- 2026-11-11 — Closed: C3-I1 passed 10/10 at the shared 13 dBm default (`radio TX power Some(52)`), log `bench/c3/logs/idf_c3_espnow_scout-20261111-233717.log`
