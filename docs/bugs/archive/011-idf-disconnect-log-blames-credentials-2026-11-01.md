---
id: 011
title: IDF Wi-Fi disconnect log blames credentials for every reason
captured-on: 2026-11-01
doc-version: 4
status: closed
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

## Fix
Implemented 2026-11-02.

- `wifi_disconnect_reason_name` now covers the 802.11 management codes 1..=24 and the ESP-IDF band 200..=212 with the `wifi_err_reason_t` spellings, so a router deauth logs `AUTH_LEAVE` instead of `(unmapped)`; codes 25..=68 (TDLS, QoS, fast transition) stay unmapped rather than guessed.
- New `reason_suggests_credentials(code)` (15, 201, 202, 204) plus `credentials_hint(code)`, host-tested.
- New `DisconnectReason(code)` `Display` type prints one shared line: `reason 8 (ASSOC_LEAVE)`, and appends ` — check SSID/password and network availability` only for those four reasons.
- The IDF `StaDisconnected` handler logs `WiFi disconnected — {DisconnectReason}`; a suspend (reason 8) and a router deauth (reason 3) therefore no longer blame the credentials.
- The esp-hal `WifiLink` logs the same hint from esp-radio's variants (`FourWayHandshakeTimeout`, `HandshakeTimeout`, `NoAccessPointFound`, `AuthenticationFailed`). esp-radio exposes no numeric code, so its `Debug` reason name is kept and only the hint text is shared — a documented capability difference rather than a faked code.
- Found on hardware and fixed: esp-radio reports a refused association as `ConnectionError::Failed`, not as a disconnect event, so the `[wifi] connect failed` line carried the reason but not the hint — the one case where a wrong password is the likely cause. `connect_failure_hint` now applies the same mapping there.
- `AUTH_EXPIRE` (2) is deliberately not in the credentials set: the bug's expected behaviour names four reasons, and an auth timeout is not proof of a typo.

## Hardware Verification (2026-11-02)
| Tier | Board | Observed line |
|:--|:--|:--|
| ESP-IDF | ESP32-C3 | `reason 15 (4WAY_HANDSHAKE_TIMEOUT) — check SSID/password and network availability` |
| esp-hal | ESP32-C3 | `connect failed: … FourWayHandshakeTimeout … — check SSID/password and network availability` |
| ESP-IDF | ESP32-C3, S3 | `reason 3 (AUTH_LEAVE)`, `2 (AUTH_EXPIRE)`, `205 (CONNECTION_FAIL)`, `8 (ASSOC_LEAVE)` — named, no hint |

The four no-hint reasons come from the router blocks (bug 010, runbook R1) and the suspend cycles (R3); the hint cases are the R2 wrong-password run, which also held the back-off cap at `30000 ms` on both tiers.

## Owner
Claude (tier-parity work)

## Links
- Bug 010 (same hardware run)
- `docs/adr/022-tier-parity-one-async-api.md`
- `docs/features/tier-parity-v1.md`

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog from the Step 2 outage test on a C3
- 2026-11-01 — Updated: reason 205 seen unmapped during the outage recovery
- 2026-11-02 — Fixed in code: 37 reason names, gated credentials hint shared by both tiers (`reason_suggests_credentials` + `DisconnectReason`), host tests and a mutation check on the gate; the new wording is visible in the bug 010 router-block run
- 2026-11-02 — Closed on hardware: the R2 wrong-password run showed the hint on `4WAY_HANDSHAKE_TIMEOUT` (IDF) and `FourWayHandshakeTimeout` (esp-hal), the router-block and suspend runs showed named reasons without it, and the run exposed that the esp-hal connect-failure line lacked the hint — fixed in this change.
