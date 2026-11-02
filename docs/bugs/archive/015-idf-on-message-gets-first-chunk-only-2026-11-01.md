---
id: 015
title: IDF on_message receives only the first chunk of a large message
captured-on: 2026-11-01
doc-version: 2
status: closed
kind: defect
---

# Bug 015: IDF on_message receives only the first chunk of a large message

## Symptom
A message larger than esp-mqtt's receive buffer reaches the `MqttBuilder::on_message` callback truncated to its first chunk; the rest of the body is never delivered.

## Suspected Cause
- esp-mqtt splits such a message into several `Received` events: the first carries the topic and the total size, the follow-ups carry only an offset.
- The builder's event loop calls `on_message` only for events with a topic, so it passes the first chunk as if it were the whole message and drops the follow-ups.
- The deprecated `MqttManager` matches `topic: Some(..)` the same way.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` (`EventPayload::Received` arm of the builder event loop; `MqttManager` event loop)
- `crates/juggler/src/mqtt/chunks.rs` (`ChunkAssembler`, already used for `MqttHandle::receive` since tier-parity Step 8b-1)

## Reproduction Confidence
high (by code reading; not yet reproduced on hardware)

## Severity
medium

## Environment
All ESP-IDF targets with `rustyfarian-esp-idf-network` 0.6.0 and branch `reconnect-rework`; any payload above esp-mqtt's receive buffer (1 KB by default).

## Expected Behaviour
`on_message` receives each message once, whole, like `MqttHandle::receive`.

## Actual Behaviour
`on_message` receives the first chunk with the topic and never the remaining bytes, so the callback sees a truncated body without any hint that it is incomplete.

## Reproduction Steps
1. Build an IDF firmware with `MqttBuilder::subscribe(cmd)` and an `on_message` callback that logs the payload length.
2. Publish a 3 KB message to `cmd` from the broker side.
3. Observe one callback with about 1 KB instead of 3 KB.

## Suggested Fix Area
- Feed the builder's `on_message` path through the same `ChunkAssembler` as the receive queue, so the callback is called once per whole message.
- This changes what the callback receives (whole messages, oversized ones dropped and counted), so record it as a behaviour change in the CHANGELOG.
- Decide whether the deprecated `MqttManager` gets the fix or stays as it is.

## Fix
Implemented 2026-11-02.

- The builder's event loop assembles first and dispatches afterwards: one `ChunkAssembler` per event-loop thread now feeds both the `on_message` callback and the receive queue, so the callback runs once per whole message.
- A message above `MAX_INBOUND_PAYLOAD_LEN` (16 KiB) is dropped and counted in `dropped_inbound()` instead of being delivered truncated, as for the queue; `on_message`'s rustdoc now says so.
- The deprecated `MqttManager` keeps the old behaviour: it is deprecated, and the fix lives in the builder the crate documents.

## Hardware Verification (2026-11-02, ESP32-C3, `idf_c3_mqtt`)
| Payload | Before the fix | After the fix |
|:--|:--|:--|
| 3000 B to the subscribed command topic | callback logged about 1000 B | callback logged all 3000 B |
| 20000 B | not run | dropped: `[mqtt] dropped inbound message above 16384 bytes` |

A normal `ping` published right after the oversized one still arrived (`[cmd] ping received`), so the assembler recovers from a dropped message.

## Owner
Claude (tier-parity work)

## Links
- `docs/features/tier-parity-v1-plan.md` (Step 8b-1)
- ADR 017 (callback threading contract)

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog after the review of 3117e31
- 2026-11-02 — Fixed and verified: reproduced on a C3 (3000 B published, about 1000 B delivered), then assembled the chunks before dispatch; the callback now sees whole messages and an oversized one is dropped and counted.
