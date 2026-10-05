# ADR 017: MQTT Callback Threading Contract

## Status

Accepted — 2026-09-28.
Implemented on `main` (commit 511b37f..2f2597a); ships in v0.5.1.

## Context

`esp-mqtt` in ESP-IDF v5.3.3 holds a recursive `MQTT_API_LOCK` during the entire event-loop iteration (line 1580–1734 in `mqtt_client.c`).
Events including `CONNECTED` and `DATA` are dispatched synchronously inside the lock via `esp_mqtt_dispatch_event`.

`esp-idf-svc` 0.53.0 blocks the mqtt task until the event-loop thread calls `EspMqttConnection::next()` again (line 797–799 in `src/mqtt/client.rs`).
Any `esp_mqtt_client_*` call (publish, subscribe, etc.) made from a callback — `on_connect`, `on_message`, or `on_disconnect` — before calling `next()` again attempts to acquire the same lock while it is already held, deadlocking both tasks forever.

Field evidence: rustyfarian-network bug 001 (local `on_connect` enqueue deadlock on startup publish), rgb-clock bug 001 (2026-09-28, `try_publish` from `on_message` rejection callback).

## Decision

### 1. Guard: callback detection and fast-fail

Every `MqttHandle` method that touches the client (`publish`, `publish_with`, `subscribe`, etc.) calls `ensure_not_in_callback()` first.
If the caller is on the event-loop thread inside a callback, it returns `PublishAckError::WrongThread` (for `anyhow`-wrapped methods) or `TryPublishError::Other` (for the `try_*` family).

Detection uses a thread-local depth counter `juggler::mqtt::CallbackScope`, entered for the whole event-loop thread and around each `on_connect` invocation, not a `ThreadId` comparison.
Host tests confirm the mechanism (see project lore reference).

### 2. Off-thread helper: per-connection thread for startup and subscribe work

On every `Connected` event, one short-lived helper thread (`juggler::mqtt::spawn_connect_thread`) runs, holding the client mutex once, in order:
1. The `with_startup_message()` publish (if configured).
2. The `on_connect` callback.
3. All builder subscriptions.

The event-loop thread never takes the client mutex directly.
`on_connect` receives a mutable `&mut EspMqttClient<'_>` argument and may use it safely (`enqueue`, `subscribe`).

`is_connected()` flips only after `on_connect` returns, via generation-checked `juggler::mqtt::ConnectionEpoch`, so stale reconnect helpers cannot mark a newer connection as connected if an older helper still runs.

### 3. User contract for safe callback design

- **From `on_connect`:** use the `&mut EspMqttClient` parameter; never call `MqttHandle` methods.
- **From `on_message` or `on_disconnect`:** never call the client or `MqttHandle` — hand work to another thread via a channel.
- **Never block on a bounded channel** whose consumer might itself block on an `MqttHandle` call; use unbounded channels or async oneshots.
- Off-callback code may use `MqttHandle` freely; `try_publish` is non-blocking only outside callbacks.

## Consequences

### Positive

- Deadlock is impossible via `MqttHandle`: callbacks that violate the contract fail fast instead of hanging forever (the deprecated legacy `MqttManager` is excluded — see Negative).
- Backward-compatible: no signature changes, no new enum variants; `on_connect` signature unchanged.
- Ordering preserved: startup message precedes any `on_connect` publish.
- The `on_connect` helper thread approach is minimal and isolates unsafe-to-call code to a single point.

### Negative

- Behavior change: a callback call that used to hang now fails with an error — acceptable in a patch release for safety.
- Stack cost: one helper thread (~12 KiB per reconnect).
- Known limit: if the helper thread cannot be spawned, `is_connected()` stays false and retries on the next reconnect.
- Known limit: `is_connected()` confirms before subscriptions are sent (ROADMAP row notes this).
- A running `on_connect` cannot be cancelled by a disconnect.
- The deprecated legacy `MqttManager` has no guard (out of scope).
- Guard cost: every `MqttHandle` publish/subscribe call first reads the thread-local `CallbackScope` depth counter (`juggler::mqtt::in_callback`) — one thread-local `Cell<u32>` read, no lock, no allocation; negligible next to the `enqueue` it precedes.
- A blocked `on_connect` holds the helper thread's client mutex until it returns, keeping `is_connected()` false and blocking `publish*`/`subscribe` from other threads; `try_publish*` returns `WouldBlock` — why `on_connect` must be short-lived.
- A panic in `on_connect` aborts and resets the device on ESP-IDF targets (std with `panic_abort`); unwinding builds poison the client mutex, causing all later `MqttHandle` calls to fail until reboot.

## Rationale

### On the helper thread vs. event-loop release

Releasing the event before invoking callbacks was rejected: in-place release is impossible because the only release point is `next()`, which also blocks waiting for the next event; that next event re-parks the mqtt task holding `api_lock`, so a publish from `on_message` deadlocks on any pending second event.
A working variant requires a separate pump thread plus a dispatcher thread with a bounded queue copying topic/payload — extra stack and per-message heap copies; on a full queue either deadlock returns or messages are lost (the broker already acknowledged them).
It also migrates `on_message` to another thread.
Kept as a possible opt-in design for a future breaking release, not adopted now.

## Alternatives Considered

- Change `on_connect` to `Fn(bool)` (no client access) — no longer needed since `on_connect` now runs on the helper thread; possible v0.6 cleanup only.
- Guard only, without the helper thread — insufficient: the startup message and `on_connect` client access would still deadlock in the `Connected` path.

<details>
<summary>Source evidence: ESP-IDF esp-mqtt and esp-idf-svc threading model</summary>

- `components/mqtt/esp-mqtt/mqtt_client.c` lines 1580–1734: `esp_mqtt_task` loop holds `MQTT_API_LOCK` for the entire iteration; `esp_mqtt_dispatch_event` (line 1040) runs inside the lock.
- `components/mqtt/esp-mqtt/mqtt_client.c` line 2207: `esp_mqtt_client_enqueue` takes the same lock; any call from within `CONNECTED` dispatch deadlocks.
- `esp-idf-svc` 0.53.0 `src/mqtt/client.rs` lines 797–799: `EspMqttConnection::next()` is the only release point for the zerocopy channel, so calls before the next `next()` block.
- `esp-idf-svc` 0.53.0 `src/private/zerocopy.rs` lines 150–181: the channel blocks the mqtt task until `receiver.done()`.

</details>

## References

- `docs/project-lore.md` § "MQTT Event Loop" — full context on the deadlock mechanism and the fix.
- `docs/bugs/001-on-connect-enqueue-deadlock-2026-09-27.md` — initial bug report.
- `docs/features/archive/mqtt-publish-acked-v1.md` — origin of the `WrongThread` error variant.
- rgb-clock outbox `docs/outbox/rustyfarian-network-try-publish-from-callback.md` (external repo, field validation).
