---
desk-work: available
---

# Feature: Tier Parity v1

Goal: consumer code is exchangeable between `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, see [ADR 022](../../adr/022-tier-parity-one-async-api.md).
The program plan and the extraction inventory were removed at closure; both stay in history at `b069ffd:docs/features/tier-parity-v1-plan.md` and `b069ffd:docs/features/tier-parity-v1-inventory.md`.

## Decisions

| Decision                                               | Reason                                                                                    | Rejected Alternative                              |
|:-------------------------------------------------------|:------------------------------------------------------------------------------------------|:--------------------------------------------------|
| Async API on both tiers                                | Consumer code exchangeable (ADR 022, decision 2)                                          | Blocking primary surface; per-consumer adapters   |
| Cores are sans-IO state machines in `juggler`          | One machine fits threads (IDF) and cancel-safe async (hal)                                | Async code generic over one port; `dyn` ports     |
| IDF async as wrappers over the existing worker threads | Spike: equal behaviour, native types only saved the runtime they omit                     | `esp-idf-svc` `AsyncWifi` / `EspAsyncMqttClient`  |
| Blocking IDF API stays during 0.x                      | Additive change                                                                           | Removing it now                                   |
| Per-tier type aliases over generic cores               | Consumers never spell the generics                                                        | Generic public types                              |
| Wi-Fi and MQTT first                                   | A consumer needed both first                                                              | All domains at once                               |
| MQTT gate fed by link readiness notifications          | Instant edges on one source of truth (IDF lost-IP is 120 s late)                          | Polling `is_ready`; second `IpEvent` subscription |
| Shared `examples-shared/lwt_app.rs` via `#[path]`      | Bench-only harness; app code stays out of the published crates                            | Shipping it in a crate                            |
| IDF examples on `block_on`                             | Runs on the C3 spike path, no new dependency                                              | `embassy-executor` `arch-std`, a new dependency   |
| `WiFiManager` names the Wi-Fi link owner on both tiers | One type name per role (ADR 022 decision 5); hal `init_async` callers keep compiling      | A new shared name; per-tier names (`WifiLink`)    |
| IDF `wait_connected()` becomes async, a 0.x break      | The blocking inherent method hid the trait's async one; deprecation cannot free the name  | Deprecate in 0.8, flip in 0.9                     |
| IDF publish family becomes shared async, a 0.x break | The shared `publish` name must be freed; the sync drop-oldest path moves to `enqueue` | Deprecate first, flip in 0.9 |
| Blocking IDF API stays as sync front-ends | Same `juggler` cores and worker threads; a library `block_on` would pick an executor (ADR 021) | `block_on` wrappers over the async API |

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
- [x] Wi-Fi owner name and the IDF `wait_connected` signature (Step 10)?
  `WiFiManager` on both tiers (hal `WifiLink` becomes a deprecated alias), and a 0.x break: the blocking variant moves to `wait_connected_timeout(timeout_ms)`.
- [x] Is each breaking item (IDF payload type, `is_connected` return type, `with_startup_message` signature) a break or a deprecation?
  A 0.x break, because a deprecation cannot free the name; the IDF Wi-Fi `is_connected()` now returns `bool` too, while the MQTT `is_connected` and the inbound `&[u8]` payload needed no change.
- [x] What are the trait names, where does `InboundMessage` live, and what does `wait_connected` return?
  `NetworkLink` / `MqttClient`, tier-owned behind `MqttMessage`, `()`.
- [x] Does the blocking IDF API become thin wrappers over the async one?
  No, it stays a set of sync front-ends over the same `juggler` cores and worker threads, since a `block_on` wrapper would pick an executor (ADR 021).
- [x] Which IDF async foundation wins the Step 2 spike?
  Wrappers over the worker threads (variant A).

## State
- [x] Design approved (ADR 022 accepted, plan reviewed)
- [x] Step 0 — feature doc and roadmap entry
- [x] Step 1 — IDF `MqttHandle::state()` and stale docs
- [x] Step 2 — async IDF spike
- [x] Step 3 — extraction inventory (removed at closure, see the note under the goal)
- [x] Step 4 — public traits and mocks
- [x] Step 5 — link core
- [x] Step 6 — MQTT session core
- [x] Step 7 — esp-hal adopts the cores
- [x] Step 8 — IDF adopts the cores (8c closed on 2026-11-12)
- [x] Step 9 — one app, two tiers (release 0.7.0)
- [x] Step 10 — Wi-Fi names and value types
- [x] Step 11 — MQTT shape

## Follow-ups

Phase 6 domains stay open as their own work, each starting with an extraction inventory and cores in `juggler`, like Steps 3–6.
- Provisioning parity is open: async `wait_committed` on IDF (IDF `wait_committed(timeout) -> Option`, esp-hal `async wait_committed() -> Result`), one `on_event` shape (IDF closure, esp-hal `fn` pointer), and the boot helpers `load_with_reentry` and `run_wifi_mqtt_portal` on esp-hal.
  The AP IP type is already aligned (`core::net::Ipv4Addr`).
- OTA parity is open: one manager name (IDF `OtaSession`, esp-hal `EspHalOtaManager`) and the MQTT OTA runtime on esp-hal.
- Done elsewhere: ESP-NOW parity ([espnow-tier-parity-v1](espnow-tier-parity-v1.md)), DNS and runtime subscribe on esp-hal, and IDF MQTT reconnect on link-up (closed by the network gate).
- LoRa: the esp-hal driver beyond the stub stays the roadmap's Long term item.

## Session Log
- 2026-11-01 — Doc and plan created; Steps 0–7b and 8a–8b-2 done (C3 spike chose variant A, inventory, shared traits, link core, `SessionMachine`, hal `WifiLink` and MQTT runner, IDF `WiFiManager` and `MqttHandle` on the cores, network gate with `MqttBuilder::build(&wifi)` as the approved 0.x break)
- 2026-11-02 — Network gate confirmed on the C3 (`idf_c3_async_mqtt`); roadmap hygiene, Step 1 ticked after the IDF R1 on the S3
- 2026-11-03 — Step 9 done (`examples-shared/lwt_app.rs` in both S3 LWT examples, capability tables in both READMEs) and passed R1 and R3 on both tiers on the CrowPanel S3, closing Step 7 too; review of 33ae125 (`run` no longer awaits the first connect); ADR 022 accepted
- 2026-11-12 — Step 8c closed (and the soak drift fixed); next are Steps 10–11
- 2026-11-12 — Step 10 decisions: `WiFiManager` on both tiers, IDF `wait_connected` async as a 0.x break; implemented the same day (hal `WifiLink` → `WiFiManager` alias, IDF `wait_connected_timeout`, `ip()` on both tiers), `just verify`, `just clippy-hal` and four IDF example builds green
- 2026-11-14 — Step 11 done: IDF `publish` becomes the shared async `publish(topic, payload, qos, retain)` and the sync drop-oldest path moves to `enqueue` (0.x break), `try_publish` takes `&[u8]`, the IDF Wi-Fi `is_connected()` returns `bool`, esp-hal gains an async `publish` and deprecates `publish_with`, `InboundMessage` accessors on both tiers, the IDF `mqtt` module re-exports the juggler validators; the blocking IDF API stays as sync front-ends; feature closed and archived to `docs/features/archive/tier-parity-v1.md`
- 2026-11-14 — Review of 55df25a: concrete-call parity checks per tier (MQTT and Wi-Fi, outside the trait imports, mutation-checked), which found the last Wi-Fi gap; esp-hal `suspend` / `resume` now return `Result<(), Infallible>` and IDF gained an inherent `wait_suspended` (0.x break); `enqueue` and IDF async `publish` docs corrected
