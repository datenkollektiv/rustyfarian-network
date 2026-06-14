# Feature: MQTT Startup Message v1

## Decisions

|                                                                 Decision | Reason                                                                                                           | Rejected Alternative                                                                                         |
|-------------------------------------------------------------------------:|:-----------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------|
|                        `.with_startup_message()` opt-in on `MqttBuilder` | Batteries-included: consumer opts in once, crate handles the rest on every (re)connect automatically             | `MqttHandle::send_startup_message()` (manual call) — caller would need to wire it themselves in `on_connect` |
| Publish via `client.enqueue()` inside the builder's wrapped `on_connect` | `enqueue` is non-blocking and safe to call from a callback context                                               | `MqttHandle::publish()` — holds the Mutex, deadlocks when called from inside a callback                      |
|         Topic and payload hardcoded to `iot/{client_id}/startup` / `"1"` | Matches the old `MqttManager.send_startup_message()` convention; YAGNI — no consumer has requested customisation | Configurable topic/payload — deferred to v2 if a real need arises                                            |
|                       Update `send_startup_message()` deprecation notice | Gives consumers a clear migration path to `.with_startup_message()`                                              | Leave notice as-is — unhelpful without a pointer to the replacement                                          |

## Constraints

- Must use `client.enqueue()` (passed into `on_connect`), not `MqttHandle::publish()` — the latter acquires the Mutex and deadlocks from a callback context.
- Topic must interpolate `client_id`, which is available from `MqttConfig` at `.build()` time — no runtime lookup needed.
- Must fire on every (re)connect, not just the first — consistent with `MqttBuilder`'s reconnect transparency.

## Open Questions

_(none)_

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log

- 2026-05-12 — Feature doc created via /feature dialog
- 2026-06-14 — Implementation landed against `crates/rustyfarian-esp-idf-mqtt/src/lib.rs`.
  Added `MqttBuilder::with_startup_message()` (sets a `bool` flag); `build()` precomputes `startup_topic: Option<String>` as `format!("iot/{}/startup", client_id)` when the flag is set and moves it into the event-loop closure.
  In the `Connected` arm, when `startup_topic` or `on_connect` is `Some`, a single mutex acquisition covers both: the startup publish via `guard.enqueue(topic, QoS::AtLeastOnce, false, b"1")` first (so it lands first in the outgoing queue), then the user's `on_connect`.
  A failed startup `enqueue` is logged at `warn!` and does not abort the connection — best-effort, matching the locked Decision.
  Deprecation note on `MqttHandle::send_startup_message` rewritten to point at `MqttBuilder::with_startup_message()` as the migration path; the secondary `publish() / publish_with()` pointer is preserved for custom lifecycle messages.
  Module-level doc example now includes `.with_startup_message()` so the canonical builder chain showcases the opt-in.
  Gates: `just fmt`, `just verify`, `just test-mqtt` (65 / 0) all clean.
  No new host tests added — the builder method is a pure setter (no logic to isolate), the topic format is `format!` at the call site (same string as the deprecated method), and the `Connected`-arm behaviour would require a mocked `EspMqttClient` to exercise (no infrastructure for that, and not justified by a single inline `enqueue` call).
