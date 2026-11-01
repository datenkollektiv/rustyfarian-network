---
id: 013
title: Minimum free heap dropped 9 KB across an IDF Wi-Fi and MQTT reconnect
captured-on: 2026-11-01
doc-version: 2
status: open-observation
kind: observation
---

# Bug 013: Minimum free heap dropped 9 KB across an IDF Wi-Fi and MQTT reconnect

## Symptom
In `idf_c3_async_mqtt` on the C3, the minimum free heap stayed at 130 324 B for 13 minutes and fell to 121 264 B across one 2-minute router block and reconnect, while the free heap recovered (145 276 B → 143 504 B).

## Suspected Cause
- Most likely a transient peak, not a leak: the reconnect allocates Wi-Fi and lwIP buffers and esp-mqtt's transport at the same time.
- Only one outage was observed, so a growing trend across repeated outages is not ruled out.
- Variant B (`idf_c3_async_mqtt_native`, no library runtime) lost only 588 B of minimum free heap across the same outage, so the 9 KB is specific to the library.
- Prime suspect: the library spawns a 12 KB connect helper thread (`CONNECT_THREAD_STACK_SIZE`) on every `Connected`, while Wi-Fi and lwIP are still allocating their reconnect buffers.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_async_mqtt.rs` (resource line from `examples/common/spike.rs`)
- Runbook `docs/runbooks/wifi-reconnect-hardware-test.md` R1 (outage protocol)

## Reproduction Confidence
low

## Session Log
- 2026-11-01 — Observation captured via /bug dialog from the Step 2 variant A outage test; compare with variant B and repeat the outage to check for a trend
- 2026-11-01 — Updated: variant B comparison (588 B drop) points at the per-connect helper thread
