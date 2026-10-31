---
id: 008
title: esp-hal provisioning fires ClientConnected on AP station disconnect events and drops the station MAC
captured-on: 2026-10-31
doc-version: 3
status: closed
kind: defect
---

# Bug 008: esp-hal provisioning fires ClientConnected on AP station disconnect events and drops the station MAC

## Symptom
When a phone leaves the provisioning SoftAP, the hal firmware logs `AP: station connected` and the session callback receives `ProvisioningEvent::ClientConnected { mac: None }`; `ClientDisconnected` is never fired.

## Suspected Cause
`wifi_task` in `src/provisioning/session.rs` awaits `WifiController::wait_for_access_point_connected_event_async` and matches `Ok(_info)` without inspecting the result.
Under the pinned `esp-radio =1.0.0-beta.1` that method resolves on both `ap::EventInfo::Connected(ConnectedInfo)` and `ap::EventInfo::Disconnected(DisconnectedInfo)`, and both carry `mac: [u8; 6]`.
The code and its comments were written against `esp-radio 0.18`, where the MAC field was unverified and no disconnect waiter existed; the 2026-09-25 wave bump changed the return type without a compile error because the variant is ignored.

## Linked Artefact
`crates/rustyfarian-esp-hal-network/src/provisioning/session.rs` `wifi_task` (match on `Ok(_info)`), `ProvisioningEvent::{ClientConnected, ClientDisconnected}` docs; `examples/hal_c3_provision_mqtt.rs` and `hal_c6_provision_mqtt.rs` "mac not yet exposed by esp-radio 0.18" log lines.
Found while verifying `review-queue/rustyfarian-network-esp-hal-docs-defects.md` (rgb-clock, 2026-10-31) at `cf49d4d`.

## Reproduction Confidence
high (code reading against the pinned crate source, fix confirmed on a bench 2026-10-31)

## Severity
low

## Environment
`rustyfarian-esp-hal-network` on `main` since the esp-radio 1.0.0-beta.1 pin (2026-09-25), `provisioning` feature, ESP32-C3 / C6 / S3 SoftAP portal.

## Expected Behaviour
A station joining fires `ClientConnected { mac: Some(mac) }`, a station leaving fires `ClientDisconnected { mac: Some(mac) }`, and the log line names the right direction.

## Actual Behaviour
Both directions fire `ClientConnected { mac: None }` and log `AP: station connected`.

## Reproduction Steps
1. Flash `hal_c3_provision_mqtt` on a blank C3 and let the portal come up.
2. Join the provisioning AP with a phone, then forget the network.
3. Observe two `AP: station connected` / `Portal: phone joined AP` log pairs and no `phone left AP` line.

## Suggested Fix Area
1. Match `ap::EventInfo::Connected` / `Disconnected` in `wifi_task` and pass `Some(info.mac)` to the matching `ProvisioningEvent` variant.
2. Drop the "mac not yet exposed" wording from the `ProvisioningEvent` docs and both hal example log lines.
3. CHANGELOG entry under the hal crate; hardware re-check on a C3 (expected log shows join and leave with the same MAC).

## Owner
Claude Code (1-2), user (3, bench).

## Links
rgb-clock request `rustyfarian-network-esp-hal-docs-defects.md` (2026-10-31, handled on branch `docs-cleanup` and removed from `review-queue/`); ADR 015 (esp-hal provisioning); `docs/project-lore.md` "SoftAP Provisioning".

## Session Log
- 2026-10-31 — Captured straight as a defect via /bug while triaging the rgb-clock docs-defects request
- 2026-10-31 — Fix areas 1-2 done on branch `bug-hunt`: `wifi_task` matches `Connected` / `Disconnected`, passes the MAC, `station_event` mapping has host tests, rustdoc refreshed; open: hardware re-check on a C3 (join and leave with the same MAC)
- 2026-10-31 — Bench check PASSED on an ESP32-C3 Super Mini (AP suffix 7B5C): phone join logged `AP: station connected (e2:7f:47:cb:6e:0e)` + `Portal: phone joined AP`, forgetting the network logged `AP: station disconnected` + `Portal: phone left AP` with the same MAC, no spurious connected line; one open-AP warning per boot confirmed the docs-cleanup fix; closed and archived
