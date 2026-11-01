---
gate: hardware
gate-note: CrowPanel S3 runs runbook R1 and R3 on both tiers with the shared app module (plan Step 9)
desk-work: available
---

# Feature: Tier Parity v1

Goal: consumer code is exchangeable between `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, see [ADR 022](../adr/022-tier-parity-one-async-api.md) and the [program plan](tier-parity-v1-plan.md).

## Decisions

| Decision                                          | Reason                                                     | Rejected Alternative                            |
|:--------------------------------------------------|:-----------------------------------------------------------|:------------------------------------------------|
| Async API on both tiers                           | Consumer code exchangeable (ADR 022, decision 2)           | Blocking primary surface; per-consumer adapters |
| Behaviour in `juggler` cores over narrow ports    | Host-tested, parity by construction (ADR 022, decision 10) | Logic duplicated per tier; `dyn` ports          |
| IDF async as adapter over existing worker threads | Keeps the validated reconnect logic; Step 2 spike confirms | Rewrite on `AsyncWifi` / `EspAsyncMqttClient`   |
| Blocking IDF API stays during 0.x                 | Additive change                                            | Removing it now                                 |
| Per-tier type aliases over generic cores          | Consumers never spell the generics                         | Generic public types                            |
| Wi-Fi and MQTT first                              | Upstream request U2g                                       | All domains at once                             |

## Constraints
- ADR 021 holds: the firmware owns the executor and the providers, and libraries select none.
- Changes are additive or deprecating unless the maintainer approves a break.
- The ADR 017 callback contract is unchanged.
- Trait futures need not be `Send`.
- Every step ends with `just fmt` and `just verify`, plus `just clippy-hal` and `just build-example` where touched.

## Open Questions
- [ ] Which IDF executor do the examples use: embassy-executor with `arch-std`, or esp-idf-hal `block_on`?
- [ ] Does the embassy crate set unify across tiers? `embassy-time-driver` is already a single 0.2.2 in the lock.
- [ ] Is each breaking item (IDF payload type, `is_connected` return type, `with_startup_message` signature) a break or a deprecation?
- [ ] What are the trait names, where does `InboundMessage` live, and what does `wait_connected` return?
- [ ] Does the blocking IDF API become thin wrappers over the async one?

## State
- [ ] Design approved (ADR 022 accepted, plan reviewed)
- [x] Step 0 — feature doc and roadmap entry
- [ ] Step 1 — IDF `MqttHandle::state()` and stale docs
- [ ] Step 2 — async IDF spike
- [ ] Step 3 — extraction inventory
- [ ] Step 4 — public traits, ports and mocks
- [ ] Step 5 — link core
- [ ] Step 6 — MQTT session core
- [ ] Step 7 — esp-hal adopts the cores
- [ ] Step 8 — IDF adopts the cores
- [ ] Step 9 — one app, two tiers (hardware gate, release 0.7.0)
- [ ] Step 10 — Wi-Fi names and value types
- [ ] Step 11 — MQTT shape

Other domains (provisioning, OTA, ESP-NOW, behaviour gaps, LoRa) follow as their own feature docs.

## Session Log
- 2026-11-01 — Feature doc created from ADR 022 and the program plan via /feature
- 2026-11-01 — Step 0 done: roadmap Ready entry added, WifiDriver ADR item dropped, contract-tests item retargeted
