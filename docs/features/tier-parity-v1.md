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
- [x] Which IDF executor do the examples use? `block_on` (Step 9); the firmware chooses its executor (ADR 021), and `embassy-executor` (`arch-std`) on IDF is untested.
- [x] Does the embassy crate set unify across tiers? Yes for the IDF graph: one `embassy-time-driver` 0.2.2, no duplicate embassy crates (Step 2 spike).
- [ ] Is each breaking item (IDF payload type, `is_connected` return type, `with_startup_message` signature) a break or a deprecation?
- [x] What are the trait names, where does `InboundMessage` live, and what does `wait_connected` return? `NetworkLink` / `MqttClient`, tier-owned behind `MqttMessage`, `()`.
- [ ] Does the blocking IDF API become thin wrappers over the async one?
- [x] Which IDF async foundation wins the Step 2 spike? Wrappers over the worker threads (variant A).

## State
- [x] Design approved (ADR 022 accepted, plan reviewed)
- [x] Step 0 — feature doc and roadmap entry
- [x] Step 1 — IDF `MqttHandle::state()` and stale docs
- [x] Step 2 — async IDF spike
- [x] Step 3 — extraction inventory ([inventory](tier-parity-v1-inventory.md))
- [x] Step 4 — public traits and mocks
- [x] Step 5 — link core
- [x] Step 6 — MQTT session core
- [x] Step 7 — esp-hal adopts the cores (T5a/T5b rerun passed on the CrowPanel S3, 2026-11-03)
- [ ] Step 8 — IDF adopts the cores (8a, 8b-1, 8b-2 done and bench-run on the C3 and S3; only 8c, bug 013, open)
- [x] Step 9 — one app, two tiers (CrowPanel S3 gate passed 2026-11-03; release 0.7.0)
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
- 2026-11-03 — Step 9 desk work done: `examples-shared/lwt_app.rs` is compiled by both S3 LWT examples (both files are setup only, ESP-IDF on `block_on`), both packages exclude their LWT example, and both READMEs carry the capability table; the CrowPanel S3 gate (R1/R3 with identical `[app]` lines) is open
- 2026-11-03 — Step 9 bench run on the CrowPanel S3 (working tree at f81d83e plus Step 9): R1 and R3 pass on both tiers, R3 with byte-identical `[app]` template sets and R1 identical except the per-tier prefix and the sampled intermediate states; the same session closed the esp-hal R1 cell and T5a/T5b, so Steps 7 and 9 are done; next is release 0.7.0
- 2026-11-03 — T6 soak after the bench: esp-hal ran 4 h 28 min (3210 ticks, one external outage of about 25 min with 10 s retry pacing and automatic recovery, heap flat at 25928 B afterwards); ESP-IDF ran 10 min (123 ticks, no gap, no disconnect) but drifted 2148 B, outside the runbook's 1 KiB criterion, and is left to Step 8c (bug 013 territory)
- 2026-11-03 — Review of 33ae125: `run` no longer awaits the first connect before starting, so the tick loop, the receive loop and the suspend demo report while the broker is unreachable; the `mqtt ready` marker is a third joined future and the `[app]` line set is unchanged. Uptime stays tick-counted, as before the extraction.
- 2026-11-03 — Bookkeeping: ADR 022 accepted, Step 9 detail doc folded into this doc and the plan and removed; next is Step 8c (bug 013), then release 0.7.0, then Steps 10–11
