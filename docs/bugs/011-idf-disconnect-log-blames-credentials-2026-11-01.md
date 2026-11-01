---
id: 011
title: IDF Wi-Fi disconnect log blames credentials for every reason
captured-on: 2026-11-01
doc-version: 2
status: open-defect
kind: defect
---

# Bug 011: IDF Wi-Fi disconnect log blames credentials for every reason

## Symptom
A router block logs `WiFi disconnected — reason 3 (unmapped) — check SSID/password and network availability`, although reason 3 is a deauthentication by the access point, not a credentials problem.

## Suspected Cause
- The IDF `StaDisconnected` handler appends the "check SSID/password" hint to every reason, including ones the router or the firmware causes on purpose.
- `juggler::wifi::wifi_disconnect_reason_name` maps only 7 codes (2, 15, 200–204), so common reasons such as 3 (`AUTH_LEAVE`), 8 (`ASSOC_LEAVE`, logged on every suspend) and 205 (`CONNECTION_FAIL`, a failed rejoin) print `(unmapped)`.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/wifi/mod.rs` (`StaDisconnected` arm of the link-event subscription)
- `crates/juggler/src/wifi/mod.rs` (`wifi_disconnect_reason_name`)
- `crates/rustyfarian-esp-hal-network/src/wifi/link.rs` logs `{:?}` of the esp-radio reason, so the tiers word the same event differently (ADR 022)

## Reproduction Confidence
high

## Severity
low

## Environment
ESP32-C3, ESP-IDF v5.3.3, branch `reconnect-rework`, example `idf_c3_async_mqtt`, router-side device block (tier-parity Step 2 run).

## Expected Behaviour
- The log names the reason (`AUTH_LEAVE`, `ASSOC_LEAVE`, …) for the common codes.
- The credentials hint appears only for reasons that point at credentials or a missing network (`AUTH_FAIL`, `4WAY_HANDSHAKE_TIMEOUT`, `HANDSHAKE_TIMEOUT`, `NO_AP_FOUND`).

## Actual Behaviour
Every disconnect, including a router deauth or the library's own suspend (reason 8), tells the user to check SSID and password.

## Reproduction Steps
1. `just flash idf_c3_async_mqtt` and `just monitor` with the C3 connected.
2. Block the device on the router.
3. Observe `WiFi disconnected — reason 3 (unmapped) — check SSID/password and network availability`.

## Suggested Fix Area
- Extend `wifi_disconnect_reason_name` with the common ESP-IDF codes and add a pure `reason_suggests_credentials(reason)` helper with host tests in `juggler`.
- Use both from the IDF handler, and log the same wording from the esp-hal `WifiLink` for tier parity.

## Owner
Claude (tier-parity work)

## Links
- Bug 010 (same hardware run)
- `docs/adr/022-tier-parity-one-async-api.md`
- `docs/features/tier-parity-v1.md`

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog from the Step 2 outage test on a C3
- 2026-11-01 — Updated: reason 205 seen unmapped during the outage recovery
