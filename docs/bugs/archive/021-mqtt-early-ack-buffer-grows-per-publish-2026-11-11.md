---
id: 021
title: ESP-IDF MQTT heap drifted about 2 KB per 10 min because the early-ack buffer kept one entry per fire-and-forget QoS 1 publish
captured-on: 2026-11-11
doc-version: 2
status: closed
kind: defect
---

# Bug 021: ESP-IDF MQTT heap drifted about 2 KB per 10 min because the early-ack buffer kept one entry per fire-and-forget QoS 1 publish

## Symptom
On the S3 soak (`idf_esp32s3_mqtt_lwt`, one QoS 1 `try_publish` every 5 s, no reconnect) the free heap fell in a doubling staircase: 116, 288, 576 and 1152 B at about 1.5, 2.5, 5 and 9.5 min, 2136 B over 10 min, while the esp-hal twin with the same app stayed flat.

## Suspected Cause
`juggler::mqtt::PendingAcks::resolve` buffers an outcome in `early` whenever no waiter is registered for the id.
A fire-and-forget publish never registers a waiter, so every `Published` event of a `try_publish` added one `HashMap` entry that only `fail_all` (a disconnect) removed.
The staircase is the map doubling at 14, 28, 56 and 112 entries, one entry per tick.

## Linked Artefact
- `crates/juggler/src/mqtt/mod.rs` (`PendingAcks`, `early`)
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` (`EventPayload::Published` → `resolve`)
- Runbook gate S3-I1 in `docs/runbooks/release-0.7.0-hardware-test.md`; bug 013 Step 8c notes

## Reproduction Confidence
high

## Severity
medium

## Environment
CrowPanel ESP32-S3, `prepare-release-bench` at `37488a7`, `just bench s3 soak` on 2026-11-08 (`bench/s3/logs/idf_esp32s3_mqtt_lwt-20261108-212305.log`); the 2026-11-03 run drifted 2148 B the same way.

## Expected Behaviour
Free heap stays within 1 KiB over a 10-minute soak of steady QoS 1 publishes.

## Actual Behaviour
Free heap steps down by a doubling amount each time the early-ack map grows, about 2 KB in 10 min, with no bound until the next disconnect.

## Reproduction Steps
1. `MQTT_HOST=<broker> MQTT_PORT=1883 just bench idf_esp32s3_mqtt_lwt soak --unattended --port <s3 port>`.
2. Read the `[app] heap free` minute lines: 206 564 → 204 428 B in the 2026-11-08 run.
3. On the host, `PendingAcks::resolve` 1000 ids with no `register` and read the buffer length.

## Suggested Fix Area
Cap `early` at a handful of entries (it only covers the gap between an enqueue returning its id and the publisher's `register`, the next instruction), oldest first.

## Links
- Bug 013 (minimum free heap across a reconnect) stays separate: that is a transient peak at connect time, this is steady-state growth.

## Session Log
- 2026-11-11 — Found from the shape of the S3-I1 soak heap lines (doubling steps at doubling intervals, one tick per entry); `PendingAcks::early` identified by reading the resolve path
- 2026-11-11 — Fixed: `early` is a `VecDeque` capped at `EARLY_ACK_CAPACITY` (8), oldest dropped first; host test `ack_early_buffer_is_bounded_without_waiters` fails without the cap (mutation check); S3-I1 rerun pending
- 2026-11-11 — Closed on the bench pass: `just bench s3 soak --unattended` 3/3 on `idf_esp32s3_mqtt_lwt` (02:33), minute heap lines 206 908 → 206 916 B over 10 min (flat) against 2136 B of drift before; lore recorded under "MQTT Event Loop"
