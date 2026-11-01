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

- [ ] IDF `MqttHandle::state() -> MqttConnectionState`, driven by the events that already maintain `is_connected()` and `connect_count()` (`Connecting` until the first CONNACK, `Disconnected` on loss, `ShuttingDown` on drop).
- [ ] Fix the `MqttConnectionState::Disconnected` doc so it holds for both tiers.
- [ ] Deprecate `juggler::wifi::WifiDriver` (`#[deprecated(note = "use the ADR 022 traits")]`), fix its implementor list; keep the re-exports with `#[allow(deprecated)]`.
- [ ] Log the state in `idf_esp32s3_mqtt_lwt` next to `connect #<n>`.
- Done when: host tests cover the state mapping; `idf_esp32s3_mqtt_lwt` builds.
- Gate: rides along with the open U2f hardware session (runbook R1 shows `Connecting → Connected → Disconnected → Connected`).

## Phase 1 — The async IDF question, answered on hardware

### Step 2: Async IDF spike (`idf_c3_async_mqtt`)

- [ ] New example: `embassy-executor` (`arch-std`) on the main task, `esp-idf-svc` `embassy-time-driver` feature enabled in the example's dev-dependencies, the existing `WiFiManager` and `MqttHandle` driven from async code, `Timer::after` ticks.
- [ ] Record in ADR 022's open questions: executor choice, lock unification (`embassy-time-driver 0.2.x`, `embassy-sync`), stack and heap high-water marks versus `idf_c3_mqtt_*`.
- Done when: the example builds, `cargo tree -i embassy-time-driver` shows one version, and a C3 run publishes ticks.
- **R:** adapter over the existing worker threads (async waits on top of `LinkSupervisor` and the MQTT event thread) versus a rewrite on `AsyncWifi` / `EspAsyncMqttClient`.
  The plan assumes the adapter: smaller, keeps the hardware-validated reconnect logic, and still satisfies ADR 022 decision 2 because the surface is async; the spike confirms or refutes this.

## Phase 2 — The shared contract and host-tested cores

### Step 3: Extraction inventory

- [ ] Table in the feature doc: every branch in the Wi-Fi and MQTT tier code that is logic rather than mapping, with its target core and port method.
- [ ] Known candidates: esp-hal MQTT runner (link watch, stall deadline, socket timeout, reconnect pacing), IDF MQTT (connection epoch confirm, `try_publish` refusal while disconnected, startup message), hal `WifiLink::run` select loop (park recheck, disconnect timeout).
- Done when: the maintainer has reviewed the table; it sets the scope of Steps 5 and 6.

### Step 4: Public traits, port traits and mocks in `juggler`

- [ ] Public traits (no_std): `juggler::wifi::NetworkLink` (`type Error`; `state`, `is_connected`, `is_suspended`, `connect_count`, `suspend`, `resume`; `async fn wait_connected(&self)`) and `juggler::mqtt::MqttClient` (`type Error`; `state`, `is_connected`, `connect_count`, `try_publish`, `try_publish_retained`, `try_publish_with` with `&[u8]` payloads and `TryPublishError<Self::Error>`; `async fn publish_acked(...)`; `async fn receive(&self) -> InboundMessage`).
- [ ] Port traits from the Step 3 inventory (for example `MqttPort`: enqueue, subscribe, and the event mapping into a shared `MqttEvent`), each with a mock behind `mock`.
- [ ] Reusable conformance suites (`run_link_contract`, `run_mqtt_contract`) that any `NetworkLink` / `MqttClient` must pass.
- Done when: `just test` runs the suites against the mocks.
- **R:** trait names; whether `InboundMessage` moves from the hal crate to `juggler`; `wait_connected` return type (`()` versus the IP).

### Step 5: Link core

- [ ] Generic link core in `juggler` implementing `NetworkLink` over the existing `LinkHandover` (no_std) and `LinkSupervisor` (std): counters, `is_suspended`, and an `embassy-sync` waiter behind `wait_connected`.
- [ ] Host tests run the link conformance suite over the core with `MockRadio`, including concurrent suspend/resume while a `wait_connected` is pending (executor: `embassy-futures::block_on`, dev-dependency).
- Done when: `just test` and `just clippy-pure` pass.

### Step 6: MQTT session core

- [ ] Generic `MqttSession<P: MqttPort>` in `juggler` implementing `MqttClient`: state machine (existing `MqttConnectionState` transitions), connect count, `try_publish` refusal while not connected, ack waiter, bounded inbound queue with drop counter, startup message.
- [ ] Host tests run the MQTT conformance suite plus the moved rules from the Step 3 inventory (stall deadline, epoch confirm) with injected time.
- Done when: `just test` passes; no tier code changed yet.

## Phase 3 — Tier adapters (thin by construction)

### Step 7: esp-hal adopts the cores

- [ ] `WifiLink` and the MQTT runner become ports plus aliases over the cores (`pub type MqttHandle = MqttSession<HalMqttPort>`); the extracted branches are deleted from the tier.
- [ ] Static parity assertion in the crate (`const _: () = { fn check<T: NetworkLink>() {} ... }`), compiled by `just clippy-hal`.
- Done when: `just clippy-hal`, the seven hal examples and `hal_esp32s3_mqtt_lwt` build; the U2f supervisor and runner behaviour is unchanged (same host tests, now in `juggler`).
- Gate: CrowPanel S3 rerun of the esp-hal MQTT runbook T5a/T5b (link loss) because runner logic moved.

### Step 8: IDF adopts the cores

- [ ] `WiFiManager` gets `NetworkLink` through the link core (inherent `state()` added, `link_state()` deprecated); `MqttHandle` becomes `MqttSession<IdfMqttPort>`, the port wrapping the esp-mqtt client and event thread (ADR 017 callback contract unchanged; inherent `&str` publish methods stay).
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
