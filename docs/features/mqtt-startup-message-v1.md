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
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-05-12 — Feature doc created via /feature dialog
