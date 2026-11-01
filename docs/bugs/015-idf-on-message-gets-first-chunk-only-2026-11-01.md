---
id: 015
title: IDF on_message receives only the first chunk of a large message
captured-on: 2026-11-01
doc-version: 1
status: open-defect
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

## Owner
Claude (tier-parity work)

## Links
- `docs/features/tier-parity-v1-plan.md` (Step 8b-1)
- ADR 017 (callback threading contract)

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog after the review of 3117e31
