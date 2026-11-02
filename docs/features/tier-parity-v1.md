---
gate: hardware
gate-note: CrowPanel S3 bench run for Step 9 (one app on two tiers), also covering the esp-hal R1, T5a and T5b
desk-work: available
---

# Feature: Tier Parity v1

Goal: consumer code is exchangeable between `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, see [ADR 022](../adr/022-tier-parity-one-async-api.md) and the [program plan](tier-parity-v1-plan.md).

## Decisions

| Decision                                               | Reason                                                                | Rejected Alternative                              |
|:-------------------------------------------------------|:----------------------------------------------------------------------|:--------------------------------------------------|
| Async API on both tiers                                | Consumer code exchangeable (ADR 022, decision 2)                      | Blocking primary surface; per-consumer adapters   |
| Cores are sans-IO state machines in `juggler`          | One machine fits threads (IDF) and cancel-safe async (hal)            | Async code generic over one port; `dyn` ports     |
| IDF async as wrappers over the existing worker threads | Spike: equal behaviour, native types only saved the runtime they omit | `esp-idf-svc` `AsyncWifi` / `EspAsyncMqttClient`  |
| Blocking IDF API stays during 0.x                      | Additive change                                                       | Removing it now                                   |
| Per-tier type aliases over generic cores               | Consumers never spell the generics                                    | Generic public types                              |
| Wi-Fi and MQTT first                                   | Upstream request U2g                                                  | All domains at once                               |
| MQTT gate fed by link readiness notifications          | Instant edges on one source of truth (IDF lost-IP is 120 s late)      | Polling `is_ready`; second `IpEvent` subscription |

## Constraints
- ADR 021 holds: the firmware owns the executor and the providers, and libraries select none.
- Changes are additive or deprecating unless the maintainer approves a break.
- The ADR 017 callback contract is unchanged.
- Trait futures need not be `Send`.
- Every step ends with `just fmt` and `just verify`, plus `just clippy-hal` and `just build-example` where touched.

## Open Questions
- [ ] Which IDF executor do the examples use? `block_on` works with the generic timer queue; `embassy-executor` (`platform-std`) is untested.
- [x] Does the embassy crate set unify across tiers? Yes for the IDF graph: one `embassy-time-driver` 0.2.2, no duplicate embassy crates (Step 2 spike).
- [ ] Is each breaking item (IDF payload type, `is_connected` return type, `with_startup_message` signature) a break or a deprecation?
- [x] What are the trait names, where does `InboundMessage` live, and what does `wait_connected` return? `NetworkLink` / `MqttClient`, tier-owned behind `MqttMessage`, `()`.
- [ ] Does the blocking IDF API become thin wrappers over the async one?
- [x] Which IDF async foundation wins the Step 2 spike? Wrappers over the worker threads (variant A).

## State
- [ ] Design approved (ADR 022 accepted, plan reviewed)
- [x] Step 0 — feature doc and roadmap entry
- [x] Step 1 — IDF `MqttHandle::state()` and stale docs
- [x] Step 2 — async IDF spike
- [x] Step 3 — extraction inventory ([inventory](tier-parity-v1-inventory.md))
- [x] Step 4 — public traits and mocks
- [x] Step 5 — link core
- [x] Step 6 — MQTT session core
- [ ] Step 7 — esp-hal adopts the cores (desk work done, CrowPanel S3 rerun pending)
- [ ] Step 8 — IDF adopts the cores (8a, 8b-1, 8b-2 done and bench-run on the C3 and S3; only 8c, bug 013, open)
- [ ] Step 9 — one app, two tiers (hardware gate, release 0.7.0)
- [ ] Step 10 — Wi-Fi names and value types
- [ ] Step 11 — MQTT shape

Other domains (provisioning, OTA, ESP-NOW, behaviour gaps, LoRa) follow as their own feature docs.

## Session Log
- 2026-11-01 — Doc and plan created; Steps 0–5 done (roadmap, IDF MQTT state, C3 spike chose variant A, inventory, shared traits, link core); bugs 010–014 filed, 012 closed
- 2026-11-01 — Step 6 done: `SessionMachine` with network gate, one connect work order, stale guard and retry pacing
- 2026-11-01 — Step 7a done: hal `WifiLink` reports readiness from the stack, counts in the machine, implements `NetworkLink`
- 2026-11-01 — Step 7b done at the desk: hal MQTT runner driven by `SessionMachine`, `MqttHandle` implements `MqttClient`; S3 rerun T5a/T5b pending
- 2026-11-01 — Session end: next is the CrowPanel S3 T5a/T5b rerun for Step 7; Step 8 (IDF adopts the cores) is desk work that can start meanwhile
- 2026-11-01 — Step 8a done: IDF `WiFiManager` feeds IP events, gains `state`/`is_ready`/`wait_ready`, implements `NetworkLink`; bench runs for both tiers deferred by the maintainer
- 2026-11-01 — Step 8b-1 done: IDF `MqttHandle` driven by `SessionMachine` (bug 014 order), implements `MqttClient` with a receive queue; network gate (8b-2) next
- 2026-11-01 — Step 8b-2 done: IDF MQTT is gated on the link, `NetworkGate`/`Outbox`/`subscribe_ready` land in `juggler` with host tests and mutation checks, `MqttBuilder::build(&wifi)` is the approved 0.x break, bug 010 fixed in code (bench run pending); Step 8c and Steps 9–11 open
- 2026-11-02 — Bug 010 bench run on the C3 (`idf_c3_async_mqtt`): the client is created 40 ms after `sta ip:` and connects 300 ms later with no transport error, and a ~2 minute block showed no MQTT line until the recovery 250 ms after the next IP; bug 010 closed
- 2026-11-02 — Roadmap hygiene: gate moved to the Step 9 bench run; Step 1 ticked after the IDF R1 on the S3; next is Step 9
