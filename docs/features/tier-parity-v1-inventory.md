# Tier Parity v1 — Extraction Inventory (Step 3)

Logic in the Wi-Fi and MQTT tier code that is not a plain driver call, event mapping or error mapping, grouped by the `juggler` core it moves into (ADR 022 decision 10).
Captured 2026-11-01 from the working tree after Step 2; line numbers drift, the grouping does not.

## Link core

| Rule                                  | IDF today                                              | esp-hal today                   | Already in `juggler`                    |
|:--------------------------------------|:-------------------------------------------------------|:--------------------------------|:----------------------------------------|
| Ready means associated and an IP      | `get_ip` polls driver and netif                        | `wait_config_up` in examples    | no                                      |
| One source of link truth              | `is_connected` reads the driver                        | `state()` from `LinkHandover`   | `LinkSupervisor::state` (unused by IDF) |
| `connect_count`                       | missing                                                | tier-side counter in `run`      | no                                      |
| Bring-up order and policy             | validate, configure, start, PS (fatal), TX (non-fatal) | in `init_async`                 | validators only                         |
| Fast resume while still associated    | n/a                                                    | `run` treats it as Connected    | no                                      |
| Handover while connecting             | supervisor                                             | `run` aborts on Disconnect only | `LinkHandover::take` (partly)           |
| Bounded park disconnect, then recheck | `stop` waits for `Stopped`                             | 500 ms timeout, 1 s recheck     | `begin_park` / `end_park` (partly)      |
| Back-off pre-empted by a handover     | supervisor                                             | `select` in `run`               | `LinkMachine` back-off value            |
| Idle restart                          | supervisor                                             | restart after 1 s               | no                                      |
| LED forces blocking connect mode      | `WiFiManager::new`                                     | n/a                             | no                                      |

## MQTT session core

| Rule                                    | IDF today                                                         | esp-hal today                                                   | Already in `juggler`                  |
|:----------------------------------------|:------------------------------------------------------------------|:----------------------------------------------------------------|:--------------------------------------|
| Config defaults and reconnect never off | keep-alive 30 s, `Some(ZERO)`                                     | keep-alive, timeouts resolved in `build`                        | no                                    |
| Socket and stall timing                 | esp-mqtt internal                                                 | 1.5 × keep-alive, stall = socket + keep-alive                   | no                                    |
| Network gate before every attempt       | missing                                                 | `is_link_up && is_config_up`, 500 ms poll                       | no                                    |
| Session state                           | `SessionPhase` + `ConnectionEpoch`                                | own `u8` encoding, duplicates `SessionPhase`                    | `SessionPhase`                        |
| Stale-session guard                     | `ConnectionEpoch` on a helper thread                              | session restart resets minimq                                   | `ConnectionEpoch` (std)               |
| Connect work order                      | startup, `on_connect`, confirm, then subscribe          | subscribe (fresh session only), startup, Connected              | no                                    |
| Publish gate                            | `try_publish` refuses unless confirmed; busy lock is `WouldBlock` | refuses unless Connected; full queue is `WouldBlock`            | `TryPublishError` type                |
| Acknowledged publish                    | `PendingAcks` waiter, QoS 1 forced                                | `publish_with` awaits queue space, skips the gate               | `PendingAcks` (std)                   |
| Outbound retry and drop                 | esp-mqtt outbox                                                   | `NotReady`: drive and retry 3 times, then drop and count        | no                                    |
| Inbound overflow                        | callbacks, no queue                                               | drop oldest, count; oversize dropped and counted                | no                                    |
| `wait_connected`                        | `build_and_wait` polls, 5 s default                               | one-waiter signal                                               | `connection_wait_iterations` (unused) |
| Teardown and reconnect pacing           | esp-mqtt retry timer                                              | abort, flush RST if network up, wait for network or fixed pause | no                                    |
| Callback-context guard                  | thread-local `CallbackScope`                                      | n/a (no callbacks)                                              | `CallbackScope`, `in_callback` (std)  |
| Default QoS and QoS 2                   | `AtLeastOnce` default                                             | QoS 2 clamped to QoS 1                                          | no                                    |

## Stays in the tier

- Driver event mapping (`WifiEvent` → `LinkEvent`, `EventPayload` → session input).
- Thread or task spawning, injected into the core as a spawner.
- Capabilities: hal IPv4-only broker, `MAX_QOS`, build-once statics, IDF drop order of worker and driver.

## Port-shape constraints

- IDF ports are synchronous, fire-and-forget and event-driven on threads; esp-hal ports are async request/response, and every port future must be cancel-safe because `run` races them in `select`.
- `LinkSupervisor`, `ConnectionEpoch`, `PendingAcks` and `CallbackScope` are std-only; the shared cores must be `no_std` with std helpers on top.
- esp-mqtt holds `api_lock` while dispatching events (ADR 017): the IDF event source and client must stay separately lockable, and the pump never takes the client lock.
- embassy-net `Stack` has one state waker, so the network gate polls; `Signal` wakes one waiter, which limits `wait_connected`.
- `riscv32imc` has no atomic read-modify-write: core counters stay single-writer `load`/`store` or use `critical_section`.

## Proposed design consequence (review point)

- Given the two execution models, the cores should be sans-IO state machines (inputs in, actions out, like `LinkMachine`), driven by a thin per-tier runner, rather than async code generic over one port trait.
- The public traits of Step 4 (`NetworkLink`, `MqttClient`) then sit on the runners' handles, and the conformance suites test the machines on the host.
