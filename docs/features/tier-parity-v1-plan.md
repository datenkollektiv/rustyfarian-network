# Tier Parity (ADR 022) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consumer firmware switches between `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` by changing dependency and setup only, never application code.

**Architecture:** Shared async-capable traits in `juggler` define the surface.
Behaviour lives in generic, host-tested cores in `juggler` (ports and adapters, the strategy pattern, ADR 022 decision 10); each tier implements only narrow port traits and keeps its type names through aliases.
A static assertion per tier and one shared example module prove the surface compiles on both.
Wi-Fi and MQTT go first (upstream request U2g), the other domains follow as their own features.

**Tech Stack:** Rust 1.95 (`esp` toolchain, native `async fn` in traits, `core::net`), `esp-idf-svc 0.53`, `esp-hal 1.2.2` + `esp-radio 1.0.0-beta.1`, embassy (`embassy-time 0.5.1`, `embassy-time-driver 0.2.2`, `embassy-executor 0.10.0`).

## Global Constraints

- ADR 022 decisions 1–9 hold for every step; ADR 021 holds (libraries never select executor, console, panic handler, critical section or scheduler).
- Additive or deprecating changes only, unless a step names a breaking change the maintainer approved.
- Every step ends with `just fmt` then `just verify`; steps touching hal also run `just clippy-hal`; steps touching examples run `just build-example` for every touched example.
- Host-testable first: a branch in a tier adapter that is not an error or event mapping is logic and moves into `juggler`; ports are narrow (one driver call or event mapping per method); time and wake-ups are injected.
- Precedent to copy: `juggler::wifi::supervisor` (`LinkSupervisor<R: LinkRadio>`, tested against `MockRadio`) and its IDF port `IdfRadio`.
- `docs/ROADMAP.md` and feature-doc growth go through the `technical-writer` agent.
- The user commits; each step ends with a suggested one-line commit message.

## How this plan is used

This file is the program plan: one step is one reviewable PR.
Before a step starts, it gets its own detailed TDD plan (`docs/features/tier-parity-v1-step-<N>.md`, deleted once the step is merged) with exact code.
Review points marked **R:** are decisions to settle when that step's detailed plan is reviewed.

---

## Phase 0 — Tracking and quick wins

### Step 0: Feature doc and roadmap entry

- [x] Create `docs/features/tier-parity-v1.md` (gate, decisions from ADR 022, one State box per step below).
- [x] Roadmap: add the tier-parity item; replace the mid-term "WifiDriver async/sync trait ADR" item (covered by ADR 022) and retarget "Contract tests in juggler wifi" at the new traits.
- Done when: `just lint-docs` passes and the maintainer has visually checked the roadmap diagram.

### Step 1: IDF `MqttHandle::state()` and stale docs

- [x] IDF `MqttHandle::state() -> MqttConnectionState`, driven by the events that already maintain `is_connected()` and `connect_count()` (`Connecting` per attempt, `Connected` exactly when `is_connected()`, `Disconnected` after a lost session or failed attempt; never `ShuttingDown`, since the session ends when the last handle drops).
- [x] Fix the `MqttConnectionState::Disconnected` doc so it holds for both tiers.
- [x] Deprecate `juggler::wifi::WifiDriver` (`#[deprecated(note = "use the ADR 022 traits")]`), fix its implementor list; keep the re-exports with `#[allow(deprecated)]`.
- [x] Log the state in `idf_esp32s3_mqtt_lwt` next to `connect #<n>`.
- Done when: host tests cover the state mapping; `idf_esp32s3_mqtt_lwt` builds.
- Gate: rides along with the open U2f hardware session (runbook R1 shows `Connecting → Connected → Disconnected → Connected`).

## Phase 1 — The async IDF question, answered on hardware

### Step 2: Async IDF spike (`idf_c3_async_mqtt`)

- [x] Variant A, `idf_c3_async_mqtt`: `embassy-executor` (`arch-std`) on the main task, the `esp-idf-svc` `embassy-time-driver` feature in the example's dev-dependencies, the existing `WiFiManager` and `MqttHandle` driven from async code through minimal async waits over their worker threads, `Timer::after` ticks.
- [x] Variant B, `idf_c3_async_mqtt_native`: the same app on `AsyncWifi` and `EspAsyncMqttClient` / `EspAsyncMqttConnection`, with reconnect and the online flag done by the example.
- [x] Compare both: code a tier port would need, how each fits behind the decision 10 ports, reconnect and callback behaviour against ADR 017, stack and heap high-water marks; amend ADR 022 decision 2 with the winner.
- [ ] Record in ADR 022's open questions: executor choice, lock unification (`embassy-time-driver 0.2.x`, `embassy-sync`), stack and heap high-water marks versus `idf_c3_mqtt_*`.
- Done when: both variants build, `cargo tree -i embassy-time-driver` shows one version, a C3 run publishes ticks with each, and the maintainer has accepted the ADR amendment.
- Decided 2026-11-01 by the maintainer: spike both instead of assuming the adapter.
- Decided 2026-11-01 by the maintainer after the hardware run: variant A, async wrappers over the existing worker threads (ADR 022 decision 2 amended).
- Both variants create their MQTT client only after the station has an IP (bug 010 interim fix), so `wifi up after` means the IP on both.
- Both variants share `examples/common/spike.rs`: one bounded inbound queue with a drop counter, one `rx` drain, and one resource report (heap, minimum heap, stack high-water of `main` and of each worker task).
- Hardware run (C3), identical for both: `just flash <example>`, `just monitor`, `just mqtt-watch`; record `wifi up after`, `mqtt up after`, the 5-minute resource lines, one `cmd` round trip (`just mqtt-pub`), a 50-message burst (`just mqtt-burst cmd 50`, then `rx` and `dropped rx`), and a 2-minute router pause.

#### Step 2 desk findings (2026-11-01)

- Both variants build for the C3 without warnings; the IDF graph holds one `embassy-time-driver` (0.2.2, 1 MHz tick like esp-rtos) and no duplicate embassy crates.
- Executor: `esp_idf_svc::hal::task::block_on` runs the app with no executor dependency, but then the esp-idf-svc time driver needs `embassy-time-queue-utils/generic-queue-N`; `embassy-executor` would bring the integrated queue instead.
- The timer-queue choice is a binary-owned provider in the sense of ADR 021: the library must not select it.
- `embassy-sync` channels with `CriticalSectionRawMutex` need the firmware's `critical-section` provider (the examples already select esp-idf-hal's).
- Variant A leaves the library unchanged; its glue is a readiness wait and an inbound channel, and its code is 1.4 KB larger than the blocking `idf_c3_mqtt`.
- Variant B's port would have to rebuild what the library already does: Wi-Fi reconnect and back-off, suspend and resume (not in the spike), session readiness and connect work, ack correlation, inbound buffering.
- Variant B's port needs the library's connection-epoch pattern too: a session can drop while its subscribe or `online` publish is awaited, and only `ConnectionEpoch::confirm` keeps it from being reported ready.
- esp-idf-svc 0.53's async client has no non-blocking publish: B's ticks await the send (logged as `published in N ms`), while A's `try_publish` hands the message to esp-mqtt's outbox (`queued`); a B port would need its own queue to offer `try_publish`.
- Variant B reintroduces the ADR 017 hazard in a new form: `EspAsyncMqttConnection` releases an event only on the next `next()`, so the event pump must await nothing else and publishing needs its own future.
- `juggler::mqtt::SessionPhase` drops into variant B unchanged, which supports decision 10: the cores work over either foundation.
- Variant B's code is about 80 KB smaller, but it leaves out the library runtime, so the sizes are not like-for-like.
- Preliminary lean: variant A, pending the hardware numbers (heap, stack, reconnect under an outage).

#### Step 2 hardware results (C3, 2026-11-01)

| Measure | A (library threads) | B (esp-idf-svc async) |
|:--|:--|:--|
| wifi up (IP) / mqtt up | 4 682 / 4 752 ms | 4 664 / 4 730 ms |
| heap free / min free, steady | 145 156 / 130 324 B | 160 224 / 155 388 B |
| worker stacks, unused | wifi-link 3 024, mqtt-events 11 180 of 12 KB | MQTT Sending ta 3 188 |
| esp-mqtt task stack, unused | 6 360 B | 7 380 B |
| burst of 50: received / dropped | 4 / 46 | 4 / 46 |
| outage: IP to MQTT up | 14 s (esp-mqtt retry) | 10.7 s (esp-mqtt retry) |
| min free drop across the outage | 9 060 B (bug 013) | 588 B |
| tick publish | `queued`, never waits | `published in 3 ms`, awaits the send |

- Start-up, message load and LWT behaviour are equal; both foundations recover through esp-mqtt's retry timer, so bug 010 applies to both.
- B's 15 to 25 KB heap advantage is the library runtime it leaves out: the 12 KB MQTT event-loop thread uses about 1 KB of its stack, and a 12 KB helper thread starts on every connect (bug 013).
- B gives up what the library already provides: reconnect and suspend, session confirmation, `try_publish` without waiting, disconnect reasons, and the ADR 017 guard.

## Phase 2 — The shared contract and host-tested cores

### Step 3: Extraction inventory

- [x] Table in `tier-parity-v1-inventory.md`: every branch in the Wi-Fi and MQTT tier code that is logic rather than mapping, with its target core and port method.
- [x] Known candidates: esp-hal MQTT runner (link watch, stall deadline, socket timeout, reconnect pacing), IDF MQTT (connection epoch confirm, `try_publish` refusal while disconnected, startup message), hal `WifiLink::run` select loop (park recheck, disconnect timeout).
- Done when: the maintainer has reviewed the table; it sets the scope of Steps 5 and 6.

### Step 4: Public traits and mocks in `juggler`

- [x] `juggler::wifi::NetworkLink` (no_std): `type Error`; `state`, `is_connected` (default: `Connected`), `is_suspended`, `connect_count`, `suspend`, `resume`; `async fn wait_connected` (associated and IP).
- [x] `juggler::mqtt::MqttClient` (no_std): `type Error`, `type Message: MqttMessage`; `state`, `is_connected`, `connect_count`, `try_publish` / `try_publish_retained` (defaults) / `try_publish_with` with `&[u8]` payloads; `async fn publish` (waits for the client to take the message, not for the PUBACK), `wait_connected`, `receive`.
- [x] `MqttMessage` accessor trait (`topic`, `payload`, `retained`, `qos`), so each tier keeps its own message storage.
- [x] Mocks behind `mock`: `juggler::wifi::mock::MockLink`, `juggler::mqtt::mock::MockMqttClient`, for firmware host tests.
- [x] Host test `one_app_both_tiers`: one generic function over both traits, run against the mocks (`just test-wifi`).
- Decided 2026-11-01 by the maintainer: the cores are sans-IO state machines (ADR 022 decision 10 amended); conformance suites therefore test the machines in Steps 5 and 6.
- Review answers: names `NetworkLink` / `MqttClient`; the message stays tier-owned behind `MqttMessage`; `wait_connected` returns `()`; acknowledged publish is not in the trait (only IDF offers it today).

### Step 5: Link core

- [x] `LinkMachine` (sans-IO, no_std): `LinkInput::{NetworkUp, NetworkDown}`, `is_ready()` (associated and IP, cleared whenever the state leaves `Connected`), `connect_count()` (not counting an association that lands while suspended), `restart()` keeping the count.
- [x] `LinkHandover` passes `is_ready` and `connect_count` through and restarts with `LinkMachine::restart`, so a cancelled hal `run` keeps the count.
- [x] `LinkSupervisor` (std): `LinkEvent::{NetworkUp, NetworkDown}` behind the stop fence, `is_ready`, `connect_count`, and `wait_ready` (a `ReadyWait` future) woken by the worker (no polling); a dropped wait unregisters itself, a repoll replaces its waker.
- [x] Host tests: readiness, stray inputs, count rules, restart, worker wake-up (mutation-checked), suspend and resume around `wait_ready`; `just test-wifi` stable over 5 runs.
- Left to the tier runners as adapter-contract steps (Steps 7 and 8): feed `NetworkUp` / `NetworkDown` from the driver, fast resume while still associated (feed `Connected` instead of connecting), the park recheck timer, the bounded hal disconnect.
- Done when: `just test` and `just clippy-pure` pass.

### Step 6: MQTT session core

- [ ] MQTT session machine in `juggler` (sans-IO, no_std) behind both tiers' `MqttClient` handles: state machine (existing `MqttConnectionState` transitions), connect count, `try_publish` refusal while not connected, ack waiter, bounded inbound queue with drop counter, one connect work order for both tiers (subscribe, startup message, then ready; bug 014), and a network gate: no attempt while the link has no IP or is suspended (hal behaviour, bug 010).
- [ ] Host tests run the MQTT conformance suite plus the moved rules from the Step 3 inventory (stall deadline, epoch confirm) with injected time.
- Done when: `just test` passes; no tier code changed yet.

## Phase 3 — Tier adapters (thin by construction)

### Step 7: esp-hal adopts the cores

- [ ] `WifiLink` and the MQTT runner become thin runners of the link and session machines and implement `NetworkLink` / `MqttClient`; the extracted branches are deleted from the tier.
- [ ] Static parity assertion in the crate (`const _: () = { fn check<T: NetworkLink>() {} ... }`), compiled by `just clippy-hal`.
- Done when: `just clippy-hal`, the seven hal examples and `hal_esp32s3_mqtt_lwt` build; the U2f supervisor and runner behaviour is unchanged (same host tests, now in `juggler`).
- Gate: CrowPanel S3 rerun of the esp-hal MQTT runbook T5a/T5b (link loss) because runner logic moved.

### Step 8: IDF adopts the cores

- [ ] `WiFiManager` gets `NetworkLink` through the link core (inherent `state()` added, `link_state()` deprecated); `MqttHandle` runs the session machine on its event thread and implements `MqttClient` (ADR 017 callback contract unchanged; inherent `&str` publish methods stay).
- [ ] Bug 013: right-size the MQTT event-loop thread stack (12 KB, about 1 KB used) and stop spawning a 12 KB helper thread on every connect; compare the minimum free heap with the Step 2 numbers.
- [ ] Bug 010: the IDF port starts esp-mqtt only once the station has an IP and stops it while the link is down or suspended, driven by the core's network gate; close the bug after the hardware run.
- Done when: `just verify` (static assertion included) and `idf_esp32s3_mqtt_lwt` build pass.
- Gate: rides with Step 9's hardware run.

## Phase 4 — Proof and release checkpoint

### Step 9: One app, two tiers

- [ ] Shared module `examples-shared/lwt_app.rs` (included with `#[path]` by both S3 LWT examples): `async fn run(link: &impl NetworkLink, mqtt: &impl MqttClient)` with status, ticks, `cmd` echo and the suspend demo.
- [ ] Both examples shrink to setup plus `lwt_app::run`.
- [ ] Capability table (ADR 022 decision 7) in both crate READMEs, starting from `docs/wifi-suspend-and-deep-sleep.md`: radio while suspended, MQTT reconnect latency, broker address, subscriptions.
- Done when: both examples build; the CrowPanel S3 passes runbook R1 and R3 on both tiers with identical app logs.
- Gate: hardware (knob maintainer).
- Checkpoint: release 0.7.0 with U2f and Steps 1–9.

## Phase 5 — Remaining Wi-Fi and MQTT shape alignment

### Step 10: Wi-Fi names and value types

- [ ] One config and owner naming scheme (**R:** `WiFiManager` on both versus a new shared name), `wait_connected` on hal replacing `wait_for_ip` (deprecated alias), `core::net::Ipv4Addr` for addresses.

### Step 11: MQTT shape

- [ ] `with_startup_message(topic, payload, qos, retain)` on IDF (**R:** new name plus deprecation versus a 0.x break), IDF runtime/inbound parity items not covered by the trait.

## Phase 6 — Other domains (one feature doc each, planned when reached)

- Each domain starts with an extraction inventory and cores in `juggler`, like Steps 3–6.
- Provisioning parity: `ap_ip` type, async `wait_committed`, `on_event` shape, boot helpers on hal.
- OTA: one manager name, MQTT OTA runtime on hal.
- ESP-NOW: one type name and constructor scheme, matching `send_and_wait`.
- Behaviour gaps: DNS on hal, runtime subscribe on hal, IDF MQTT reconnect on link-up.
- LoRa: hal driver beyond the stub.
