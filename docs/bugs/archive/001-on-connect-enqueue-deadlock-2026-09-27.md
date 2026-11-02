---
id: 001
title: Client calls inside MqttBuilder on_connect deadlock the ESP-IDF MQTT event loop
captured-on: 2026-09-27
doc-version: 5
status: closed
kind: defect
---

# Bug 001: Client calls inside `MqttBuilder::on_connect` deadlock the ESP-IDF MQTT event loop

## Symptom
Calling `client.enqueue()` inside `MqttBuilder::on_connect` hangs the MQTT event loop forever, so `is_connected()` never becomes `true` and the broker eventually fires the Last Will.

## Suspected Cause
Verified against source, not a hunch.

- esp-mqtt sends `MQTT_EVENT_CONNECTED` to handlers from inside `esp_mqtt_task` while holding the recursive `api_lock`.
- In IDF v5.3.3 `mqtt_client.c`, the lock is taken at line 1580, the event is sent at line 1635, and the lock is released at line 1734.
- The client event loop is created without its own task (line 875), so handlers run synchronously inside the mqtt task.
- esp-idf-svc 0.53.0 passes each event to our thread via `zerocopy::Channel::share`, which blocks the mqtt task until the consumer calls `EspMqttConnection::next()` again.
- `esp_mqtt_client_enqueue` takes `api_lock` (line 2207), and a recursive mutex only lets its owning task back in.
- Our event-loop thread therefore waits on `api_lock` while the mqtt task waits on our thread.
- `connected` is only set after `on_connect` returns (`mod.rs` line 1095), so it never flips.
- The parked mqtt task sends no keepalive, so the broker publishes the LWT.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` — `MqttBuilder::build` event loop, `Connected` arm.
- Found by a downstream consumer and filed as an upstream bug report (on-connect enqueue deadlock, 2026-09-27).

## Reproduction Confidence
high

## Severity
high

## Environment
- `rustyfarian-esp-idf-network` 0.5.0 with esp-idf-svc 0.53.0 and ESP-IDF v5.3.3.
- Reported on an ESP32-C3-DevKitM-1 running esp-idf-svc 0.52.1; the same blocking handoff is still present in 0.53.0.
- Should hit every ESP-IDF target, because the lock scope comes from esp-mqtt and not from the chip.

## Expected Behaviour
- A publish queued from `on_connect` (or by `with_startup_message()`) is sent.
- The callback returns.
- `is_connected()` becomes `true` within milliseconds.

## Actual Behaviour
- The log shows `[mqtt] connected (clean_session=...)` and the callback's first line, then nothing more from MQTT.
- `is_connected()` stays `false`, so every later publish is dropped.
- No `Disconnected` event is delivered.
- The broker publishes the retained LWT while the device still believes it is connecting.

## Reproduction Steps
1. Build an `MqttBuilder` whose `on_connect` calls `client.enqueue(topic, QoS::AtLeastOnce, true, b"online")`, the pattern shown in the `MqttBuilder` rustdoc example.
2. Alternatively, enable `.with_startup_message()` with any `on_connect` or none at all.
3. Flash and connect to a broker, with an LWT configured.
4. Watch `is_connected()` stay `false` after `[mqtt] connected`, and watch the LWT payload appear on the broker.

## Suggested Fix Area
Other places the same mechanism hits:

- `with_startup_message()` calls `guard.enqueue` on the event-loop thread (`mod.rs` lines 1064–1067), and its rustdoc wrongly promises it "never deadlocks".
- The `MqttBuilder` rustdoc example (`mod.rs` line 762) and the `on_connect` docs (lines 838–854) recommend `client.enqueue()` in the callback.
- `examples/idf_c3_mqtt.rs` and `examples/idf_esp32_mqtt.rs` (line 105 in each) call both `subscribe()` and `enqueue()` in `on_connect`.
- The comment at `mod.rs` lines 1091–1094 blames the Rust mutex for a deadlock that is really the esp-mqtt `api_lock` one.
- Received messages are handled inside the same locked section, so calling `MqttHandle::publish` from `on_message` likely deadlocks too (inferred from the lock scope, not verified on hardware).

Proposed fix, in two steps:

- 0.5.x patch, without breaking the API:
  - Send the startup message from a helper thread, the same way `spawn_subscriber_thread` already sends subscriptions.
  - Fix the rustdoc and the two examples.
  - Correct the comment at `mod.rs` lines 1091–1094.
  - Consider a `WrongThread` guard on `MqttHandle::publish`.
- 0.6 breaking change: switch `on_connect` to `Fn(bool)` so the mistake no longer compiles, and optionally add `with_connect_message(topic, payload, retain)`.
- Re-verify on hardware: `idf_c3_mqtt`, `idf_c3_mqtt_button_oled`, and one `with_startup_message()` user.

## Owner
Florian Waibel — landed on `main` as `511b37f` (fix), `76d0d6e` (0.5.1 bump), `5550775` (credential fallbacks), `2f2597a` (optional OLED); branch `fix/mqtt-on-connect-deadlock` deleted.

## Fix Status (0.5.x patch; plumbing refreshed by tier-parity Steps 6 and 8b)
- The event-loop thread never takes the client mutex; since Step 8b-2 it only translates esp-mqtt events into session inputs, and the session runner performs `Close` with the raw handle (an event bit, no `api_lock`).
- Since Step 8b-1 the connect work runs on the tier's own `mqtt-connect` worker, holding the client mutex once, in the order both tiers share: the builder subscriptions, then the `with_startup_message()` publish, then `on_connect`.
- Because `on_connect` runs on that worker, `client.enqueue()` / `client.subscribe()` inside it are safe again; the originally documented retained-status idiom works.
- `is_connected()` still flips only after `on_connect` returns: the shared `SessionMachine` reports ready only on the connect work's `WorkDone`, and a `SessionToken` makes a stale worker's report a no-op (`juggler::mqtt::ConnectionEpoch` is deprecated and unused since Step 8b-1).
- `MqttHandle::publish*`, `try_publish*`, `subscribe`, `publish_acked` reject calls from inside any callback with `PublishAckError::WrongThread`, detected through a thread-local `juggler::mqtt::CallbackScope` (host-tested, lock-free); signatures unchanged.
- Corrected during review: `EspMqttClient::subscribe` never waits for a SUBACK, the blocker was always `api_lock`; a second, three-way cycle (publisher holding the mutex on `api_lock` during the handshake, mqtt task parked, event loop blocked on the mutex for `on_connect`) is removed by the same change.
- Rustdoc, both READMEs, CHANGELOG, `docs/project-lore.md` "MQTT Event Loop", and a CLAUDE.md failure row describe the final rule: use the `client` argument in `on_connect`, never `MqttHandle` from any callback, never the client from `on_message` / `on_disconnect`.

## Hardware Verification (2026-11-02)
| Board    | Example                      | Evidence                                                                |
|:---------|:-----------------------------|:------------------------------------------------------------------------|
| ESP32-C3 | `idf_c3_mqtt_callback_guard` | ready 180 ms after `Connected`; `guard_hits` = 2 × `received` for 316 s |
| ESP32-C3 | `idf_c3_mqtt`                | retained `online` on the broker; 14 heartbeats; `[cmd] ping received`   |
| ESP32-S3 | `idf_esp32s3_mqtt_lwt`       | retained `online`; 15 ticks; MQTT connect 60 ms after the IP            |

`received` runs slightly ahead of `sent` in the guard run because QoS 1 resends when a PUBACK is not handled within 1 s (at-least-once, `docs/project-lore.md` "OTA Consumer Runtime"); `guard_hits` tracked `2 × received` exactly, so duplicates are the only cause.
`idf_c3_mqtt_button_oled` was dropped from the gate: it needs an SSD1306 and exercises the same connect path as the two C3 runs above.

## Follow-ups (not part of this defect)
- Helper-spawn failure (heap exhaustion): the session never sees `WorkDone`, so `is_connected()` is `false` while the transport is up; `MqttHandle` publishes still reach the outbox, and subscriptions and `on_connect` retry on the next reconnect; an injectable spawner would make this path host-testable.
- A running `on_connect` cannot be cancelled by a disconnect; it is documented as short-lived and idempotent, and a connection-aware callback API is a candidate for the next breaking release.
- 0.6 (now optional): `on_connect` → `Fn(bool)` is no longer needed for safety; `with_connect_message(topic, payload, retain)` remains a convenience idea.

## Links
- Source report: upstream bug report from the downstream consumer (on-connect enqueue deadlock).
- Related feature doc: `docs/features/archive/mqtt-publish-acked-v1.md` (introduced the `WrongThread` guard).

## Session Log
- 2026-09-27 — Captured as a defect from the upstream report, with the root cause checked against esp-idf-svc 0.53.0 and IDF v5.3.3 source.
- 2026-09-27 — 0.5.x fix landed on `fix/mqtt-on-connect-deadlock` (helper-thread startup publish, `WrongThread` guard on all `MqttHandle` publish/subscribe methods, docs/examples/README/CHANGELOG/lore); review found the connect-time mutex cycle (documented, deferred); `just verify` and both MQTT example builds green; hardware checks pending, not closed.
- 2026-09-27 — Second review found the connect-time mutex cycle reachable from any unguarded publish; `on_connect` moved onto the helper thread, `ConnectionEpoch` + `CallbackScope` added to juggler, examples publish the retained status from `on_connect` again; hardware checks still pending, not closed.
- 2026-09-27 — Second code review round: `spawn_connect_thread` reports spawn failure so the event loop still confirms the epoch, `CallbackScope` made `!Send` / `#[must_use]` / depth-counted, stale helpers skip the prelude via `ConnectionEpoch::is_current`, stale rustdoc corrected; `just verify`, `just test-mqtt` (103 passed), and all four MQTT example builds green; hardware checks pending.
- 2026-09-27 — Commit `db8e789` reviewed; follow-ups: `doctor.sh` broker probe passes host/port as arguments, host regression test holds the client mutex from a publisher thread while the helper is spawned, `on_connect`/`on_disconnect` overlap and the helper-spawn readiness exception documented, workspace version and `juggler` minimum raised to 0.5.1; kept open pending hardware checks.
- 2026-09-27 — Third review: helper-spawn failure no longer confirms the epoch (contract kept, documented), stale-helper limits documented precisely, host test for a disconnect/reconnect while `on_connect` is blocked; still open pending hardware checks.
- 2026-09-27 — Merged to `main` as four commits (`511b37f`, `76d0d6e`, `5550775`, `2f2597a`) and pushed; `idf_c3_mqtt_button_oled` headless path confirmed on hardware, broker connection still blocked by a stale shell export; bug stays open until the MQTT paths are verified.
- 2026-09-28 — rustyfarian-rgb-clock filed an upstream request (their bug 001): on 0.5.0, `try_publish` from `on_message` hung the client until reset on an ESP32-C3. This is field confirmation of the `on_message` path, which had been inferred here. Re-checked against IDF v5.3.3 and esp-idf-svc 0.53.0: `receiver.done()` runs only inside `next()`, so "release the event before `on_message`" is not a viable fix (ADR 017). The `idf_c3_mqtt_callback_guard` example was added as the pending hardware re-check for both the connect path and the message path; the `try_publish*` docs were corrected. Still open pending that hardware run.
- 2026-11-02 — Re-checked against the post-Step-8b-2 code: the defect is still fixed and the hazard is unchanged, because esp-mqtt's `api_lock` scope and esp-idf-svc's park-until-`next()` are upstream and untouched; the plumbing references above were refreshed (tier-owned `mqtt-connect` worker, shared order subscriptions → startup → `on_connect`, `SessionToken` instead of `ConnectionEpoch`, `Close` performed by the lock-free runner), and `ConnectionEpoch`, `spawn_connect_thread` and `spawn_subscriber_thread` are deprecated; status stays `open-defect` pending the hardware run.
- 2026-11-02 — Closed on hardware: the C3 guard example cleared both callback paths (316 s, `guard_hits` = 2 × `received`), `idf_c3_mqtt` confirmed the ordinary connect plus inbound path, and the S3 confirmed the same connect path on a second chip; `idf_c3_mqtt_button_oled` dropped from the gate as redundant.
