---
id: 014
title: IDF MQTT session reports ready before its subscriptions are sent
captured-on: 2026-11-01
doc-version: 1
status: open-defect
kind: defect
---

# Bug 014: IDF MQTT session reports ready before its subscriptions are sent

## Symptom
`MqttHandle::is_connected()` and `state() == Connected` turn true before the builder's subscriptions have been sent to the broker.

## Suspected Cause
- `juggler::mqtt::spawn_connect_thread` runs the prelude first and subscribes afterwards, and the IDF prelude ends with `epoch.confirm(token)`.
- So the order is startup message, `on_connect`, confirm (ready), then SUBSCRIBE.
- The `connect_count` docs promise the opposite: the count bumps "after the startup message, `on_connect` and the subscriptions".

## Linked Artefact
- `crates/juggler/src/mqtt/mod.rs` (`spawn_connect_thread`: prelude, then the topic loop)
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` (prelude in `MqttBuilder::build` ends with `confirm`)
- `docs/features/tier-parity-v1-inventory.md` (MQTT session core, "Connect work order")

## Reproduction Confidence
high (by code reading; the window is short, so hardware rarely shows it)

## Severity
medium

## Environment
All ESP-IDF targets, `rustyfarian-esp-idf-network` 0.6.0 and branch `reconnect-rework`.

## Expected Behaviour
- The session is reported ready only after its subscriptions were sent, as on esp-hal (subscribe, startup message, then Connected).
- A firmware that publishes a request as soon as `is_connected()` turns true receives the reply on its subscribed topic.

## Actual Behaviour
- Between `confirm` and the end of the subscribe loop, `is_connected()` is true but the broker has no subscription yet.
- Messages published to the subscribed topics in that window are never delivered to the device on a clean session.
- The connect work order also differs between the tiers (IDF: startup first; esp-hal: subscriptions first).

## Reproduction Steps
1. Build an IDF firmware with `MqttBuilder::subscribe(cmd)` and a broker client that publishes to `cmd` as soon as the device's retained `online` appears.
2. Watch `[mqtt] subscribed to …` in the device log arrive after the `online` publish and after `is_connected()` turned true.
3. Repeat until a `cmd` sent in that window goes missing.

## Suggested Fix Area
- Confirm the epoch only after the subscriptions, in one helper run: subscribe, startup message, `on_connect`, confirm.
- Fix it in the shared MQTT session core (tier-parity Step 6) so both tiers use one connect work order, and add a host test for the order.

## Owner
Claude (tier-parity work)

## Links
- `docs/adr/022-tier-parity-one-async-api.md` (decision 10)
- `docs/features/tier-parity-v1-plan.md` (Step 6, Step 8)
- ADR 017 (callback threading contract)

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog from the tier-parity Step 3 inventory
- 2026-11-01 — Shared rule implemented in `juggler::mqtt::SessionMachine` (tier-parity Step 6); the IDF fix lands when IDF adopts it (Step 8)
- 2026-11-01 — Fixed in code (tier-parity Step 8b-1): the IDF helper thread subscribes, publishes the startup message and runs `on_connect` before the session reports ready; close after the bench run
