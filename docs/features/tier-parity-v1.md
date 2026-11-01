---
gate: hardware
gate-note: CrowPanel S3 runs runbook R1 and R3 on both tiers with the shared app module (plan Step 9)
desk-work: available
---

# Feature: Tier Parity v1

Goal: consumer code is exchangeable between `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, see [ADR 022](../adr/022-tier-parity-one-async-api.md) and the [program plan](tier-parity-v1-plan.md).

## Decisions

| Decision                                               | Reason                                                                | Rejected Alternative                             |
|:-------------------------------------------------------|:----------------------------------------------------------------------|:-------------------------------------------------|
| Async API on both tiers                                | Consumer code exchangeable (ADR 022, decision 2)                      | Blocking primary surface; per-consumer adapters  |
| Cores are sans-IO state machines in `juggler`          | One machine fits threads (IDF) and cancel-safe async (hal)            | Async code generic over one port; `dyn` ports    |
| IDF async as wrappers over the existing worker threads | Spike: equal behaviour, native types only saved the runtime they omit | `esp-idf-svc` `AsyncWifi` / `EspAsyncMqttClient` |
| Blocking IDF API stays during 0.x                      | Additive change                                                       | Removing it now                                  |
| Per-tier type aliases over generic cores               | Consumers never spell the generics                                    | Generic public types                             |
| Wi-Fi and MQTT first                                   | Upstream request U2g                                                  | All domains at once                              |

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
- [ ] Step 1 — IDF `MqttHandle::state()` and stale docs
- [x] Step 2 — async IDF spike
- [x] Step 3 — extraction inventory ([inventory](tier-parity-v1-inventory.md))
- [x] Step 4 — public traits and mocks
- [x] Step 5 — link core
- [x] Step 6 — MQTT session core
- [ ] Step 7 — esp-hal adopts the cores
- [ ] Step 8 — IDF adopts the cores
- [ ] Step 9 — one app, two tiers (hardware gate, release 0.7.0)
- [ ] Step 10 — Wi-Fi names and value types
- [ ] Step 11 — MQTT shape

Other domains (provisioning, OTA, ESP-NOW, behaviour gaps, LoRa) follow as their own feature docs.

## Session Log
- 2026-11-01 — Doc and plan created; Steps 0–5 done (roadmap, IDF MQTT state, C3 spike chose variant A, inventory, shared traits, link core); bugs 010–014 filed, 012 closed
- 2026-11-01 — Step 6 done: `SessionMachine` with network gate, one connect work order, stale guard and retry pacing
