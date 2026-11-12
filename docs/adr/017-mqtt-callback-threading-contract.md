# ADR 017: MQTT Callback Threading Contract

## Status

Accepted — 2026-09-28.
Implemented on `main` (commit 511b37f..2f2597a); ships in v0.5.1.
Amended 2026-11-02: §2's plumbing (helper thread, work order, stale-session guard) is superseded by the MQTT session core, the `MqttHandle` session machine and the network gate (ADR 022); the contract itself is unchanged.
Amended 2026-11-11 (ADR 022, MQTT thread memory): §2 has no helper thread any more; the `mqtt-net` runner performs the connect work one step per turn; contract §1 and §3 are unchanged.

## Context

`esp-mqtt` in ESP-IDF v5.3.3 holds a recursive `MQTT_API_LOCK` during the entire event-loop iteration (line 1580–1734 in `mqtt_client.c`).
Events including `CONNECTED` and `DATA` are dispatched synchronously inside the lock via `esp_mqtt_dispatch_event`.

`esp-idf-svc` 0.53.0 blocks the mqtt task until the event-loop thread calls `EspMqttConnection::next()` again (line 797–799 in `src/mqtt/client.rs`).
Any `esp_mqtt_client_*` call (publish, subscribe, etc.) made from a callback — `on_connect`, `on_message`, or `on_disconnect` — before calling `next()` again attempts to acquire the same lock while it is already held, deadlocking both tasks forever.

Field evidence: a local `on_connect` enqueue deadlock on startup publish, and an rgb-clock field report (2026-09-28, `try_publish` from `on_message` rejection callback).

## Decision

### 1. Guard: callback detection and fast-fail

Every `MqttHandle` method that touches the client (`publish`, `publish_with`, `subscribe`, etc.) calls `ensure_not_in_callback()` first.
If the caller is on the event-loop thread inside a callback, it returns `PublishAckError::WrongThread` (for `anyhow`-wrapped methods) or `TryPublishError::Other` (for the `try_*` family).

Detection uses a thread-local depth counter `juggler::mqtt::CallbackScope`, entered for the whole event-loop thread and around each `on_connect` invocation, not a `ThreadId` comparison.
Host tests confirm the mechanism (see project lore reference).

### 2. Connect work placement (originally an off-thread helper; superseded, see the 2026-11-11 amendment below)

On every `Connected` event, one short-lived helper thread (`juggler::mqtt::spawn_connect_thread`) runs, holding the client mutex once, in order:
1. The `with_startup_message()` publish (if configured).
2. The `on_connect` callback.
3. All builder subscriptions.

The event-loop thread never takes the client mutex directly.
`on_connect` receives a mutable `&mut EspMqttClient<'_>` argument and may use it safely (`enqueue`, `subscribe`).

`is_connected()` flips only after `on_connect` returns, via generation-checked `juggler::mqtt::ConnectionEpoch`, so stale reconnect helpers cannot mark a newer connection as connected if an older helper still runs.

Amended 2026-11-02 (ADR 022, MQTT session core and network gate): the helper is now the tier's own worker thread, named `mqtt-connect` on the ESP-IDF tier, and it runs the builder subscriptions first, then the `with_startup_message()` publish, then `on_connect` — the order both tiers share.
The stale-session guard is now `juggler::mqtt::SessionToken`: `is_connected()` turns true only when the shared `SessionMachine` sees `WorkDone` for the current token, and a stale worker's report is ignored.
`juggler::mqtt::spawn_connect_thread`, `juggler::mqtt::spawn_subscriber_thread` and `juggler::mqtt::ConnectionEpoch` are deprecated, used by no tier, and stay until 1.0.

Amended 2026-11-11 (ADR 022, MQTT thread memory): the ESP-IDF tier runs no connect helper thread at all.
The connect work runs on the `mqtt-net` runner thread itself, one step per loop turn, and each step takes the client lock only for its own esp-mqtt call: one SUBSCRIBE, the startup `enqueue`, the `on_connect` call, or the closing runtime catch-up.
The step order, stop at the first failed SUBSCRIBE, startup publish or runtime catch-up (a failing `on_connect` is logged, never fatal), the closing catch-up before `WorkDone`, and the terminal report are the shared `juggler::mqtt::ConnectWorkMachine` (ADR 023 rung 3), performed through the narrow `juggler::mqtt::ConnectPort` by `juggler::mqtt::perform`.
Before every step the runner asks `SessionMachine::is_current` for the work's `SessionToken`, and it abandons the work of a session that ended without reporting it.

`on_connect` still runs under `CallbackScope` with `&mut EspMqttClient`, and the event-loop thread still never takes the client lock.
A publisher thread that waits for the client lock gets it before the next step when it runs at the runner's priority (the runner yields after every step) or above (it preempts on the mutex give); a lower-priority publisher has no such guarantee.
The `mqtt-connect` thread named in the 2026-11-02 amendment is gone, and so is the one-helper-thread wording in the Consequences and Alternatives sections below, which this amendment supersedes.
Contract §1 and §3 are unchanged.

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
- Running the connect work on one library thread (the `mqtt-net` runner since 2026-11-11) isolates unsafe-to-call code to a single point.

### Negative

- Behavior change: a callback call that used to hang now fails with an error — acceptable in a patch release for safety.
- The MQTT thread memory change (2026-11-11) adds three `unsafe` blocks, all FFI: the `esp_pthread_get_cfg` and `esp_pthread_set_cfg` calls that name a library thread, the restore of the caller's pthread configuration, and the `uxTaskGetStackHighWaterMark` self-measurement.
- Known limit: `is_connected()` confirms before subscriptions are sent (ROADMAP row notes this).
- A running `on_connect` cannot be cancelled by a disconnect.
- The deprecated legacy `MqttManager` has no guard (out of scope).
- Guard cost: every `MqttHandle` publish/subscribe call first reads the thread-local `CallbackScope` depth counter (`juggler::mqtt::in_callback`) — one thread-local `Cell<u32>` read, no lock, no allocation; negligible next to the `enqueue` it precedes.
- A blocked `on_connect` holds the client lock AND defers the runner's Close, Shutdown and retry handling until it returns; a warning is logged past 1 s, and `is_connected()` stays false meanwhile, which is why `on_connect` must be short-lived.
- The connect plan no longer runs under one continuous client-lock hold, so an app publish may enqueue between two steps, before the startup message; readiness (`try_publish*`, `is_connected()`, `wait_connected`) is still gated on `WorkDone`.
- A SUBSCRIBE step performs `esp_mqtt_write` on the runner thread, so under `mqtts://` the TLS write frames run there, and the runner stack is sized for them.
- A panic in `on_connect` aborts and resets the device on ESP-IDF targets (std with `panic_abort`); unwinding builds poison the client mutex, causing all later `MqttHandle` calls to fail until reboot.

## Rationale

### On the helper thread vs. event-loop release

Releasing the event before invoking callbacks was rejected: in-place release is impossible because the only release point is `next()`, which also blocks waiting for the next event; that next event re-parks the mqtt task holding `api_lock`, so a publish from `on_message` deadlocks on any pending second event.
A working variant requires a separate pump thread plus a dispatcher thread with a bounded queue copying topic/payload — extra stack and per-message heap copies; on a full queue either deadlock returns or messages are lost (the broker already acknowledged them).
It also migrates `on_message` to another thread.
Kept as a possible opt-in design for a future breaking release, not adopted now.

### On the runner performing the connect work (2026-11-11)

The `mqtt-net` runner performs the connect work itself, because a step cannot deadlock on `api_lock`.
The lock is released when the event-loop thread calls `EspMqttConnection::next()` again, and that thread never waits on the runner: it only makes unbounded `Session::send` calls, pushes into the inbound queue and calls `PendingAcks`.
`publish_acked` registers its waiter only after it has released the outbox lock, so no lock cycle exists.
The earlier premise that the runner already blocked on `api_lock` in `connect()` and `close()` was wrong: `esp_mqtt_client_reconnect` and `esp_mqtt_client_disconnect` take no `api_lock` and only set event bits.
The calls that do take `api_lock` are `subscribe`, `enqueue` and `stop`, and the connect steps make the first two, one per step, apart from the user's `on_connect`, which makes whatever calls it chooses through the `&mut EspMqttClient` parameter.

## Alternatives Considered

- Change `on_connect` to `Fn(bool)` (no client access) — no longer needed since `on_connect` now runs on the helper thread; possible v0.6 cleanup only (the helper is gone since 2026-11-11; the argument holds for the runner).
- Guard only, without the helper thread — insufficient: the startup message and `on_connect` client access would still deadlock in the `Connected` path (the helper is gone since 2026-11-11; the argument holds for the runner).

<details>
<summary>Source evidence: ESP-IDF esp-mqtt and esp-idf-svc threading model</summary>

- `components/mqtt/esp-mqtt/mqtt_client.c` lines 1580–1734: `esp_mqtt_task` loop holds `MQTT_API_LOCK` for the entire iteration; `esp_mqtt_dispatch_event` (line 1040) runs inside the lock.
- `components/mqtt/esp-mqtt/mqtt_client.c` line 2207: `esp_mqtt_client_enqueue` takes the same lock; any call from within `CONNECTED` dispatch deadlocks.
- `esp-idf-svc` 0.53.0 `src/mqtt/client.rs` lines 797–799: `EspMqttConnection::next()` is the only release point for the zerocopy channel, so calls before the next `next()` block.
- `esp-idf-svc` 0.53.0 `src/private/zerocopy.rs` lines 150–181: the channel blocks the mqtt task until `receiver.done()`.

</details>

## References

- `docs/project-lore.md` § "MQTT Event Loop" — full context on the deadlock mechanism and the fix.
- `docs/features/archive/mqtt-publish-acked-v1.md` — origin of the `WrongThread` error variant.
- rgb-clock outbox `docs/outbox/rustyfarian-network-try-publish-from-callback.md` (external repo, field validation).
