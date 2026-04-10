# Feature: Wi-Fi Radio Power Configuration v1

## Decisions

|                                                                                           Decision | Reason                                                                | Rejected Alternative                                                                 |
|---------------------------------------------------------------------------------------------------:|:----------------------------------------------------------------------|:-------------------------------------------------------------------------------------|
|                               5-level enum for TX power (`Lowest`, `Low`, `Medium`, `High`, `Max`) | Intuitive for users; abstracts raw dBm values which vary by chip      | Raw dBm integer — too low-level, error-prone, chip-dependent limits                  |
|                                    5-level enum for power-save mode reusing the same scale concept | Consistent API; lowest maps to `MinModem`, highest maps to `MaxModem` | Exposing ESP-IDF `WifiPowerSave` directly — leaks platform detail                    |
|                                                      Configuration at init time only (builder API) | Simplest correct approach; runtime adjustment can be added later      | Runtime-adjustable — unnecessary complexity for current use case                     |
| Auto-burst during discovery: full TX power during `scan_for_peer()`, then drop to configured level | Maximizes discovery range without permanent heat; no manual toggling  | Always-high power — defeats the purpose; manual toggle — error-prone, easy to forget |
|                                 Discovery burst has a configurable timeout (default a few seconds) | Prevents staying at full power if peer never comes online             | No timeout — risks sustained high power indefinitely on failed discovery             |
|                                       Exact dBm mapping per level determined during implementation | Requires testing on real hardware across C3/C6/S3                     | Guessing values upfront — unreliable without measurement                             |
|                                      Shared enum types in `wifi-pure` (platform-independent crate) | Keeps types testable and reusable across ESP-IDF and esp-hal backends | Defining in each HAL crate — duplication, divergent APIs                             |

## Constraints

- Must work on all current targets: ESP32-C3, ESP32-C6, ESP32-S3
- Must not break existing builder APIs — additive change only
- Applies to both ESP-NOW and WiFi/MQTT connection paths
- Uses `esp_wifi_set_max_tx_power()` under the hood (ESP-IDF) and equivalent for esp-hal

## Open Questions

- [ ] What are the correct dBm values for each of the 5 TX power levels? (research + hardware testing during implementation)
- [ ] Does `esp_wifi_set_max_tx_power` behave identically across C3/C6/S3?
- [ ] Does `MinModem` power-save mode interact badly with ESP-NOW peer discovery or `scan_for_peer()`?

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log

- 2026-04-03 — Feature doc created via /feature dialog
- 2026-04-03 — Added auto-burst during discovery with timeout
- 2026-04-10 — Implemented: `TxPowerLevel` enum in `wifi-pure` with 5 levels and `to_quarter_dbm()` mapping. ESP-IDF backend calls `esp_wifi_set_max_tx_power()` after `wifi.start()`. esp-hal backend stores config but logs warning (esp-radio 0.17 lacks TX power API). ESP-NOW `scan_for_peer()` auto-bursts to max TX power during scanning with save/restore. 24 wifi-pure tests pass including 6 new TxPowerLevel tests. `just verify` and `just build-example` (hal_c3_connect, hal_c3_connect_async) all pass clean.
