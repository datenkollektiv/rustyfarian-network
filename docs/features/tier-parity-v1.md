---
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
| Shared `examples-shared/lwt_app.rs` via `#[path]`      | Bench-only harness; app code stays out of the published crates        | Shipping it in a crate                            |
| IDF examples on `block_on`                             | Runs on the C3 spike path, no new dependency                          | `embassy-executor` `arch-std`, a new dependency   |

## Constraints
- ADR 021 holds: the firmware owns the executor and the providers, and libraries select none.
- Changes are additive or deprecating unless the maintainer approves a break.
- The ADR 017 callback contract is unchanged.
- Trait futures need not be `Send`.
- Every step ends with `just fmt` and `just verify`, plus `just clippy-hal` and `just build-example` where touched.

## Open Questions
- [x] Which IDF executor do the examples use?
  `block_on` (Step 9); the firmware chooses its executor (ADR 021), and `embassy-executor` (`arch-std`) on IDF is untested.
- [x] Does the embassy crate set unify across tiers?
  Yes for the IDF graph: one `embassy-time-driver` 0.2.2, no duplicate embassy crates (Step 2 spike).
- [ ] Is each breaking item (IDF payload type, `is_connected` return type, `with_startup_message` signature) a break or a deprecation?
- [x] What are the trait names, where does `InboundMessage` live, and what does `wait_connected` return?
  `NetworkLink` / `MqttClient`, tier-owned behind `MqttMessage`, `()`.
- [ ] Does the blocking IDF API become thin wrappers over the async one?
- [x] Which IDF async foundation wins the Step 2 spike?
  Wrappers over the worker threads (variant A).

## State
- [x] Design approved (ADR 022 accepted, plan reviewed)
- [x] Step 0 — feature doc and roadmap entry
- [x] Step 1 — IDF `MqttHandle::state()` and stale docs
- [x] Step 2 — async IDF spike
- [x] Step 3 — extraction inventory ([inventory](tier-parity-v1-inventory.md))
- [x] Step 4 — public traits and mocks
- [x] Step 5 — link core
- [x] Step 6 — MQTT session core
- [x] Step 7 — esp-hal adopts the cores
- [x] Step 8 — IDF adopts the cores (8c closed on 2026-11-12)
- [x] Step 9 — one app, two tiers (release 0.7.0)
- [ ] Step 10 — Wi-Fi names and value types
- [ ] Step 11 — MQTT shape

Other domains (provisioning, OTA, ESP-NOW, behaviour gaps, LoRa) follow as their own feature docs.

## Session Log
- 2026-11-01 — Doc and plan created; Steps 0–7b and 8a–8b-2 done (C3 spike chose variant A, inventory, shared traits, link core, `SessionMachine`, hal `WifiLink` and MQTT runner, IDF `WiFiManager` and `MqttHandle` on the cores, network gate with `MqttBuilder::build(&wifi)` as the approved 0.x break)
- 2026-11-02 — Network gate confirmed on the C3 (`idf_c3_async_mqtt`); roadmap hygiene, Step 1 ticked after the IDF R1 on the S3
- 2026-11-03 — Step 9 done (`examples-shared/lwt_app.rs` in both S3 LWT examples, capability tables in both READMEs) and passed R1 and R3 on both tiers on the CrowPanel S3, closing Step 7 too; review of 33ae125 (`run` no longer awaits the first connect); ADR 022 accepted
- 2026-11-12 — Step 8c closed (and the soak drift fixed); next are Steps 10–11
