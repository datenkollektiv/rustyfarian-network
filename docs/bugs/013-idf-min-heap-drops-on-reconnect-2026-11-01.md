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
- 2026-11-11 — S3 bench figures (`idf_esp32s3_mqtt_lwt`, free heap, not the minimum): the `suspend` run lost 860 B across three suspend/resume reconnects (`connect #1` 206 760 B → `connect #4` 205 900 B), and the 2026-11-08 10-min soak drifted 2136 B with no reconnect; the fix is still open
- 2026-11-11 — The steady-state soak drift (2136 B per 10 min) is a separate defect, bug 021: `PendingAcks::early` kept one entry per fire-and-forget QoS 1 publish; fixed with a cap of 8, S3-I1 rerun pending. The reconnect minimum-heap peak of this observation stays open (Step 8c stack sizes).
