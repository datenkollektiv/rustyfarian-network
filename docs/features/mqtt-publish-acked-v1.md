# Feature: MQTT Acknowledged Publish (`publish_acked`) v1

The `MqttHandle` publish surface in `rustyfarian-esp-idf-network::mqtt` is
fire-and-forget everywhere: `publish`, `publish_retained`, and `publish_with`
call `EspMqttClient::enqueue` and return **before** the broker acknowledges
(`crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs:1189`). This proposes an
**acknowledged** publish primitive — `MqttHandle::publish_acked` — that blocks
until the broker's QoS 1 PUBACK arrives (or a timeout elapses), so a caller can
learn, with a bounded wait, whether a specific message was durably received.
The pure correlation logic lives in `juggler::mqtt` for host-testability; the
`rustyfarian-esp-idf-network` layer wires it to `esp-idf-svc`'s event loop. The
change is **purely additive (semver-minor)** — no existing publish method
changes.

## Problem & evidence

The OTA contract we designed contains **exactly one** ack-gated action. On every boot,
if NVS `pend_slot` differs from the running slot, the bootloader rolled back;
the device must publish `rolled_back` (retained, QoS 1) and clear `pend_slot`
**only after the publication is acknowledged**, so an unreachable broker means a
retry on the next boot instead of silently lost rollback.
Rollback evidence is what feeds
an operator's halt-on-failure gate, so losing it silently defeats the
contract's core purpose.

The current wrapper cannot express it:

| Call site                      | Behaviour                                                                              | Evidence                                          |
|:-------------------------------|:---------------------------------------------------------------------------------------|:--------------------------------------------------|
| `MqttHandle::publish_with`     | `enqueue` then return; no PUBACK wait                                                  | `mqtt/mod.rs:1175-1191`                           |
| `MqttHandle::publish_retained` | Thin wrapper over `publish_with`; documented QoS 1 but returns before ack              | `mqtt/mod.rs:1163-1165`                           |
| `on_connect` callback          | May only use `client.enqueue(...)`; returns a `MessageId` but no ack event is surfaced | `mqtt/mod.rs:996-1003`, doc `mqtt/mod.rs:787-791` |
| Builder event loop             | `EventPayload::Published(msg_id)` falls into the `_ => {}` arm and is **discarded**    | `mqtt/mod.rs:1044`                                |
| `MqttBuilder` hooks            | `subscribe` / `on_connect` / `on_disconnect` / `on_message` — no `on_published`        | `mqtt/mod.rs:710-849`                             |

The capability exists underneath: `esp-idf-svc` delivers
`EventPayload::Published(msg_id)` on the event loop; the wrapper simply drops it.
`EspMqttClient::enqueue` already returns a `MessageId` that would correlate a
PUBACK back to the originating publication.

**Precedent in-repo.** `juggler::espnow` already implements exactly this
cross-thread completion pattern for ESP-NOW send confirmation:
`AckStatus(Arc<(Mutex<Option<bool>>, Condvar)>)` with a `wait`/`signal` pair
(`crates/juggler/src/espnow/mod.rs:788` ff.). `publish_acked` is the MQTT
analogue — a pending-ack registry keyed by `MessageId`, signalled when the
matching `Published` event arrives.

**Threading hazard the design must respect.** The PUBACK arrives on the
event-loop thread. The consumer's follow-up (NVS erase) is blocking I/O that
must not run in a callback — the wrapper's own discipline (`mqtt/mod.rs:787-791`,
"Do not call `MqttHandle::publish` from inside this callback"). So the
completion signal must cross from the event-loop thread to the calling task,
and `publish_acked` must be forbidden from callbacks — calling it from
`on_connect`/`on_message` would block the very thread that must deliver the
PUBACK, deadlocking. This feature adds a runtime guard for that misuse.

## Proposed change

Add one primary method plus the internal plumbing to make it work.

**Public API** (`MqttHandle`, `rustyfarian-esp-idf-network::mqtt`):

```rust
/// Publish and block until the broker acknowledges (QoS 1 PUBACK) or the
/// timeout elapses. MUST NOT be called from an event-loop callback
/// (on_connect / on_message) — doing so returns `PublishAckError::WrongThread`.
pub fn publish_acked(
    &self,
    topic: &str,
    payload: &[u8],
    retained: bool,
    timeout: Duration,
) -> Result<(), PublishAckError>;
```

**There is deliberately no `qos` parameter.** v1 fixes the publication to QoS 1
internally — the only level that produces a PUBACK to correlate. This is *not* a
general "acked publication for arbitrary QoS": QoS 0 yields no acknowledgment at all,
and QoS 2's exactly-once PUBREC/PUBREL/PUBCOMP handshake is out of scope (see the
QoS decision and the QoS 2 open question). Fixing QoS at the type level keeps the
correlation invariant — every `publish_acked` call expects exactly one
`Published` event — from being violated by a caller passing QoS 0.

`PublishAckError` distinguishes at least four cases:

- `Timeout` — no PUBACK within the window. A **broker-timing outcome**: the
  caller keeps its NVS state and retries next boot.
- `Disconnected` — the session dropped before the ack. Also a **broker-timing
  outcome** (no false `Ok`); retry-eligible, same as `Timeout`.
- `WrongThread` — called from an event-loop callback. A **programming error**;
  never retry, fix the call site.
- `Other` — a **local, non-broker failure**: topic validation rejected the
  publish, the underlying `enqueue` call failed, or the client mutex was
  poisoned. These say nothing about broker reachability and are not resolved by
  retrying next boot; they indicate a bug or a malformed argument. (Whether to
  split `EnqueueFailed` out of `Other` is an open question below; v1 leans to
  folding it in since no current consumer reacts differently to the sub-cases.)

**Internal plumbing:**

1. A `MessageId`-keyed pending-ack registry (`Arc<Mutex<HashMap<MessageId,
   AckSlot>>>`, each slot an `Arc<(Mutex<Option<AckOutcome>>, Condvar)>`),
   shared between `MqttHandle` and the event-loop thread.
2. Event loop: handle `EventPayload::Published(msg_id)` (currently `_ => {}`) →
   look up the slot, store success, notify. On `Disconnected`, fail **all**
   outstanding slots so a session drop can never masquerade as an ack.
3. `publish_acked`: register a slot, `enqueue` under the client mutex to get the
   `MessageId`, insert the slot under that id, release the mutex, then wait on
   the condvar with the timeout. Clean up the slot on every exit path.
4. Event-loop thread identity captured at `build()` so the `WrongThread` guard
   can compare `std::thread::current().id()`.

**Pure tier (`juggler::mqtt`):** the correlation/state logic that does not touch
`esp-idf-svc` — the pending-ack registry type, `register` / `resolve(msg_id)` /
`fail_all` / timeout bookkeeping, and the outcome enum — lives here with
`#[cfg(test)]` host coverage (no ESP toolchain), mirroring how `next_state`
(`juggler/src/mqtt/mod.rs:272`) already isolates testable MQTT logic. The
`esp-idf-network` layer owns only the thread wiring and the `EspMqttClient`
calls.

## Decisions

| Decision                                                                                   | Reason                                                                                                                                                                                  | Rejected Alternative                                                                                                                                                                                                                                 |
|:-------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Option 1 — a blocking `publish_acked(…, timeout)`** returning `Ok` only on PUBACK        | Keeps the cross-thread condvar signalling *inside* the wrapper so every consumer stays simple. Matches the existing blocking `build_and_wait` style.                                    | **Option 2 — `on_published(MessageId)` hook** + `MessageId`-returning publish: pushes the condvar/threading burden onto every consumer (each would re-implement it). **Both**: doubles the surface/test matrix for no current consumer need.         |
| **Guard against calls from event-loop callbacks** (return `WrongThread`, not deadlock)     | Blocking on a PUBACK from inside the callback that must process it deadlocks; a runtime guard turns a silent hang into a clear, distinguishable error.                                  | Document-only ("MUST NOT call from callbacks") — a misuse then deadlocks silently.                                                                                                                                                                   |
| **QoS 1 only in v1** (PUBACK)                                                              | Exactly what the contract requires. QoS 2 is a distinct three-step handshake (PUBREC/PUBREL/PUBCOMP) with no current consumer.                                                          | QoS 1 + QoS 2 now — widens the completion-tracking + test matrix for a capability nothing uses yet.                                                                                                                                                  |
| **Pure correlation logic in `juggler::mqtt`; ESP wiring in `rustyfarian-esp-idf-network`** | Satisfies the host-testability requirement without an ESP toolchain; follows the established `next_state` split and the `AckStatus` condvar precedent in `juggler::espnow`.             | All logic in the ESP crate — untestable on host, and host-side simulation could not exercise it.                                                                                                                                                     |
| **Fail all outstanding acks on `Disconnected`**                                            | No false `Ok` when the session dropped before PUBACK. A dropped session must resolve waiters to `Disconnected`, never leave them to time out ambiguously or spuriously succeed.         | Let waiters time out — loses the ability to distinguish "broker slow" from "session gone," and the caller waits the full timeout needlessly.                                                                                                         |

## Constraints

- **Additive only** — no existing publish method changes signature or behaviour; `publish`, `publish_retained`, `publish_with`, and the `try_publish*` family are untouched. Cannot break any current consumer.
- **Must not deadlock the event loop** — `publish_acked` acquires the client mutex only to `enqueue` (to obtain the `MessageId`), then **releases it before waiting** on the condvar. The event-loop thread must be able to take the mutex and deliver the PUBACK while a caller waits. The existing "set connected AFTER `on_connect` releases the mutex" ordering (`mqtt/mod.rs:1012-1016`) is the model to preserve.
- **Callable only from a normal task/thread** — forbidden from `on_connect`/`on_message`; enforced by the `WrongThread` guard, not just docs.
- **No false positives across reconnects** — a `Disconnected` between enqueue and PUBACK MUST resolve the waiter to a distinguishable error.
- **Host-testable** — the pure tier logic runs under `just test-mqtt` (or the equivalent pure-crate test recipe) with no ESP toolchain; a host-side simulated-device test tier must be able to exercise it.
- **`MessageId` correlation is a blocking prerequisite** — the entire design assumes the `MessageId` returned by `EspMqttClient::enqueue` and the id carried by `EventPayload::Published` are the **same id space**. This MUST be verified against the installed `esp-idf-svc` version *before* implementation begins; if the two diverge, the registry key must be derived from `enqueue`'s return and matched to the event's value, or the correlation is unsound. Tracked as the first State item.
- **`MessageId` semantics** — correlate only QoS 1 publishes; a QoS 0 publish never yields a `Published` event, so `publish_acked` must reject / not be offered for QoS 0 (v1 fixes QoS to 1 internally, sidestepping this).
- **CHANGELOG** — entry under `[Unreleased] → Added` describing `publish_acked` and `PublishAckError`; no `Migration` entry (purely additive).
- **Completion gate** — end with `just fmt` then `just verify`; the MQTT event-loop wiring is ESP-IDF code, so also `just build-example <name>` for any example that exercises it.

## Open Questions

- [ ] **Exact `PublishAckError` variant set.** `Timeout`, `Disconnected`, `WrongThread`, `Other` are the minimum. Is a separate `EnqueueFailed` worth splitting from `Other`? (Leaning: fold into `Other` for v1.)
- [x] **`MessageId` id-space verification** — *promoted to a blocking pre-implementation prerequisite* (see the `MessageId` correlation Constraint and the first State item). **Resolved 2026-07-10** against esp-idf-svc 0.52.1 / embedded-svc 0.29.0: `MessageId = u32`; `enqueue → enqueue_cstr` returns `check(esp_mqtt_client_enqueue(..))` and `check` returns `Ok(result as MessageId)` (the ESP-IDF-assigned `msg_id`); `EventPayload::Published(self.0.msg_id as _)` echoes that same `msg_id`. Same `u32` id space — no divergence, so the registry keys directly off `enqueue`'s return.
- [ ] **Retained-only convenience wrapper?** OTA will always publish `rolled_back` retained. Do we also want a `publish_retained_acked(topic, payload, timeout)` thin wrapper, or is the `retained: bool` parameter on `publish_acked` enough? (Leaning: parameter only, matching `publish_with`.)
- [ ] **QoS 2 follow-up.** Deferred by decision; track as a future `-v2` if a consumer needs exactly-once ack semantics.
- [x] **Where the event-loop `ThreadId` is captured and stored** so the `WrongThread` guard can read it. **Resolved:** stored in an `Arc<Mutex<Option<ThreadId>>>` shared into the event-loop closure (which sets it to `std::thread::current().id()` at thread start) and held on `MqttHandle`; `publish_acked` reads it and returns `WrongThread` on a match.

## State

- [x] Design approved
- [x] **Prerequisite verified** — confirmed against esp-idf-svc 0.52.1: `enqueue`'s returned `MessageId` and `EventPayload::Published`'s id are the same `u32` id space (details in the Open Question above).
- [x] Core implementation — pure registry `PendingAcks` / `AckWaiter` / `AckOutcome` / `MessageId` in `juggler::mqtt` (std-gated); `publish_acked` + `PublishAckError` + event-loop `Published` (resolve) / `Disconnected` (fail_all) wiring + `WrongThread` guard (event-loop `ThreadId` captured at `build()`) in `rustyfarian-esp-idf-network::mqtt`.
- [x] Tests passing — 7 host tests for the pure correlation logic (resolve-before-wait, early-resolution buffer, timeout+cleanup, cross-thread wakeup, fail_all, stale-early-buffer clear, clone-shares-registry); public-path lock extended in `juggler/tests/public_paths.rs`. `just test-mqtt` = 83 passed; `just verify` green (clippy `-D warnings` clean, `check-mqtt` compiled `publish_acked` against the ESP-IDF target).
- [x] Documentation updated — module-doc "Acknowledged publish" section + full method rustdoc (with the forbidden-from-callbacks note) and CHANGELOG `[Unreleased] → Added`.

## Session Log

- 2026-07-10 — Feature doc created
- 2026-07-10 — PR review round. (1) Made the QoS contract explicit at the API definition — there is deliberately no `qos` parameter; v1 fixes QoS 1 at the type level to preserve the one-call-one-`Published` correlation invariant, and this is not a general arbitrary-QoS acked publish. (2) Tightened the `PublishAckError` model — each variant now states whether it is a broker-timing outcome (retry-eligible: `Timeout`, `Disconnected`) or a programming/local failure (never retry: `WrongThread`, `Other`), and `Other`'s membership (validation reject, `enqueue` failure, poisoned mutex) is enumerated. (3) Promoted the `MessageId` id-space question from an optional Open Question to a blocking pre-implementation prerequisite — added a Constraint, a first State checkbox ("Prerequisite verified"), and left a tracked Open Question pointer. Declined the code-reference-permalink suggestion: prose `file:line` refs match the established feature-doc house style (e.g. `ota-domain-reexport-parity-v1.md`) and the reviewer flagged it as optional/later.
- 2026-07-10 — Implemented on branch `mqtt-publish-acked`. Verified the blocking prerequisite first (esp-idf-svc 0.52.1: `enqueue` return and `EventPayload::Published` share the `u32` `msg_id` space). Built the pure `PendingAcks` registry in `juggler::mqtt` (std-gated, with an early-outcome buffer that makes the enqueue→register race correct by construction) + 7 host tests; wired `publish_acked` / `PublishAckError` / event-loop `Published`+`Disconnected` handling / `WrongThread` guard into `rustyfarian-esp-idf-network::mqtt`; extended the public-path lock; added module docs, method rustdoc, and a CHANGELOG entry. `just fmt` + `just verify` green (clippy `-D warnings` clean; `check-mqtt` compiled the new code against the ESP-IDF target); `just test-mqtt` = 83 passed. All State items complete. Remaining open questions (`EnqueueFailed` split, retained-only wrapper, QoS 2) are deferred by design, not blockers. Not yet hardware-validated on a broker — the correlation is host-tested but an on-device PUBACK round-trip against the real TTN/mosquitto path is still worth a smoke test before the OTA §6 consumer relies on it.