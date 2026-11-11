# ADR 023: Shared tier logic follows a three-rung ladder

## Status

Accepted (2026-11-04).
Decisions 5 and 6 amended (2026-11-05): decision 5 for the explicit `Ignored` variants, decision 6 to cover finite and persistent lifecycles; the closure evidence is in [ladder-closure-v1](../features/ladder-closure-v1.md).
Decisions 1 and 10 amended (2026-11-05): decision 1 for the flattened machines and the IDF ESP-NOW worker core, decision 10 for the port instance.
Decisions 1, 5, 6, 9 and 10 amended (2026-11-11, ADR 022 Step 8c): decision 1 for `ConnectWorkMachine`, decision 5 for its explicit `Ignored`, decision 6 for its finite lifecycle and the connect-step exception, decision 9 for the runner's lock invariant, decision 10 for `ConnectPort`.
Refines [ADR 022](022-tier-parity-one-async-api.md) decision 10; it does not supersede it.
The review rule and the ports-and-adapters split stand, and "behaviour lives in sans-IO machines" becomes "behaviour lives in `juggler` on the lowest rung that fits".
`rustyfarian-power` ADR-004 and `rustyfarian-peripherals` ADR-010 adopt the same ladder.

## Context

Where both tiers sequence the same flow, each re-implements the order of the steps, and the two copies drift.
[ADR 022](022-tier-parity-one-async-api.md) decision 10 answers that drift with sans-IO state machines: inputs go in, actions come out, and a thin runner per tier performs them.
Not every shared flow needs a machine.
Where `embedded-hal` covers the I/O, a driver generic over its traits is simpler and has no second copy of the order.
Much of `juggler` is flat logic (`decide_offer`, `reconcile`, `ChunkAligner`, MQTT topic validation, disconnect-reason mapping), where a machine would be ceremony.

This repo differs from the siblings in three ways that change the wording, not the ideas.

- There is no standard trait for Wi-Fi, MQTT or ESP-NOW, so the cores take narrow custom ports.
  `LinkSupervisor<R: LinkRadio>` has an IDF port, `IdfRadio`, that maps four `esp_wifi_*` calls, and it is host-tested against `MockRadio`.
  `EspNowWorker<R: EspNowRadio>` follows the same shape: the IDF ESP-NOW `IdfRadio` maps six one-call methods, and the worker is host-tested against a mock radio.
- The cores that drive the machines may hold injected wake-up primitives (`embassy-sync`, stored `Waker`s), as [ADR 022](022-tier-parity-one-async-api.md) decision 10 already allows.
  The machines themselves stay free of async.
- `juggler` uses `alloc` (the MQTT outbox, chunk reassembly), so "allocation-free" applies to the machines only where practical.

## Decision

1. Shared logic takes the lowest rung that fits:
   - **Rung 1 — generic driver.** The flow lives in `juggler`, generic over `embedded-hal` or `embedded-storage` traits.
     The shared instances are `juggler::lora::sx1262::init`, generic over the `embedded-hal` SPI, GPIO and `DelayNs` traits, and `juggler::provisioning::flash_store::ProvisioningStore<F: NorFlash>`, behind juggler's `flash-store` feature.
     The hal tier uses that store through `rustyfarian_esp_hal_network::provisioning::ProvisioningStore`.
     The IDF tier keeps its own NVS-backed store, `rustyfarian_esp_idf_network::provisioning::ProvisioningStore`, as a tier-local store.
     That store reuses juggler's record logic through the `ProvisioningKv` port.
   - **Rung 2 — event machine.** The core decodes samples: `update(sample, now) -> Option<Event>`.
     A tier, an ISR, or an `embedded-hal` adapter feeds it.
     The instances are `juggler::mqtt::NetworkGate`, with `feed(ready) -> Option<SessionInput>`, and two IDF-only decoders.
     `ManifestBody::feed(n, elapsed) -> Feed` (`juggler/src/ota/runtime/fetch.rs`) takes the time as an input, the same shape as `update(sample, now)`.
     `ChunkAssembler::feed(chunk) -> Assembled` (`juggler/src/mqtt/chunks.rs`) takes no clock, as `NetworkGate` does.
     `NetworkGate` takes no clock because it only turns readiness edges into events.
   - **Rung 3 — action machine.** The core emits I/O requests: `poll(&mut self, input) -> Action`, and a thin loop per tier performs them.
     Examples are `LinkMachine` (`juggler::wifi::link`), `SessionMachine` (`juggler::mqtt::session`), `ScanMachine` and `ScanGate` (`juggler::espnow::scan`, `juggler::espnow::scan_job`), `DownloadMachine` (`juggler::ota::download`), `PortalMachine` (`juggler::provisioning::portal_flow`), `PeerLink` (`juggler::espnow::link`) and `TxCore` (`juggler::espnow::tx`).
   - Dated 2026-11-05: the closure audit re-judged five flows against the decision 2 triggers, and three of them are flat logic now, not machines.
     The LoRaWAN event tick runs in one blocking loop on one tier (the IDF examples), so `LorawanTicker` became the stateless `juggler::lora::LorawanTimeline::next_event` with the pure `classify_response` and `requested_deadline`; it was never released.
     The post-update health policy runs on the app's main thread of the IDF tier only, so `HealthMachine` became the pure `poll_decision`, `after_unhealthy`, `deadline_plan` and `SlotReads`, driven by sequential code.
     The `rolled_back` report delivery and the completion retry run on the IDF reporter thread only, so `ReporterMachine` and `CompletionRetry` became the plain-data `ReportPace` and `CompletionBackoff` with the pure `after_publish` and `completion_step`.
     The released machines (`HealthMachine`, `ReporterMachine`, `CompletionRetry`, with `HealthAction`, `HealthEvent` and `ViewStep`) stay as deprecated wrappers over the same functions for one release.
     `ScanGate` and `PortalMachine` stay at rung 3: the IDF tier runs both from threads (the `espnow-tx` worker and `httpd` handlers) and the hal tier from cooperative async, so decision 2 trigger one holds; the evidence is in [ladder-closure-v1](../features/ladder-closure-v1.md).
   - Dated 2026-11-05: the IDF ESP-NOW transmit and scan loop moved into `juggler::espnow::EspNowWorker<R: EspNowRadio>` (`std`), so the IDF adapter keeps only its one-call port.
     The worker runs on the IDF tier alone, as `LinkSupervisor` does, but it sits on `TxCore`, `ScanGate` and `ScanMachine`, which the hal tier also runs under its cooperative runner.
     Decision 2 trigger one therefore holds for the shared core, and the worker is its thread-side driver.
   - Dated 2026-11-11: the MQTT connect work moved into `juggler::mqtt::ConnectWorkMachine` (`crates/juggler/src/mqtt/connect_work.rs`), a rung-3 machine that owns the order of the connect steps, the stop at the first failed step, the closing step (`Finish`, which the IDF tier uses for the runtime catch-up and the hal tier for the SUBACK drain) before `WorkDone`, and the report or the abandon.
     Decision 2 holds on both triggers: the IDF tier runs the machine on the `mqtt-net` thread beside the event-loop thread's session inputs, the hal tier runs it under cancel-safe embassy async, and both perform esp-mqtt or minimq calls, which `embedded-hal` does not cover.
     The IDF adapter maps each action through the sync `ConnectPort` with `perform`, and the hal `serve` maps the same actions with one await each.
2. Rung 3 is used only when a flow meets one of these triggers:
   - It must run under more than one execution model, such as threads, events, or cancel-safe async.
   - The tiers perform I/O outside `embedded-hal`, such as `esp_wifi_*` or esp-radio calls, but must share the order of the steps.
3. Flat logic stays as pure functions or stateless types in `juggler`, not machines.
4. Time is an input: a machine never reads a clock; the caller passes `now` or reports elapsed waits.
   Wake-ups are injected the same way.
5. A rung-3 machine answers out-of-phase input with an explicit no-op action, so a host test of a tier loop can detect it.
   `LinkAction::None` and `SessionAction::None` currently mean both "nothing to do" and "input ignored".
   The ladder retrofit before the next release brings every existing machine onto this rule.
   Dated 2026-11-05: the retrofit added explicit `Ignored` variants to `LinkAction`, `SessionAction`, `ScanAction` and others (see the CHANGELOG), so `None` no longer means "input ignored" on those enums; `TxCore` has no `Ignored` variant yet, which stays open in [ladder-closure-v1](../features/ladder-closure-v1.md).
   `ScanGate` has no `Ignored` variant either: a stale epoch answers `Cancelled::Stale` and changes nothing, admission answers `GateRefusal`, and a stale `release` does nothing.
   Dated 2026-11-11: `ConnectWorkMachine` answers input after its terminal action, and a second `Start`, with an explicit `WorkAction::Ignored` that leaves its state unchanged; the IDF runner logs an error and drops the work, and the hal `serve` returns.
6. Every rung-3 flow has a defined lifecycle, and its tier loop bounds every wait.
   A loop-driven finite flow reaches a terminal action, such as `DownloadMachine` `Done` or `ScanMachine` `Done`; its loop treats a no-op after that action as an error exit.
   `ScanGate` is finite per scan: its exclusion ends on `finish`, `cancel` or `release`.
   `PortalMachine` is request-driven, so each request is one `poll` and there is no loop bound.
   Its terminal states (`Committed`, `FactoryResetPending`) answer later input with `Refused`, which both tiers map to HTTP 409, and a bad nonce answers `Forbidden`.
   A persistent service never terminates, such as `LinkMachine` (Wi-Fi), `SessionMachine` (MQTT) or `TxCore` (ESP-NOW worker), but every iteration between waits is bounded by a tick, a timeout or a budget, such as the 500 ms `NETWORK_POLL_MS` and `POLL_TICK` ticks or the Wi-Fi back-off deadline.
   Tier loops differ on `Ignored`: the IDF MQTT runner counts it and logs at 16 in a row, the esp-hal runner restarts; see [ladder-closure-v1](../features/ladder-closure-v1.md).
   Five waits are exceptions: a frame already on the air is waited out with no bound (ADR 020, on both ESP-NOW tiers), the IDF Wi-Fi and ESP-NOW workers block on their event source while idle, which relies on the driver event contract, the esp-hal `run_sender` blocks on its wake signal with no deadline while the queue is empty, and the IDF MQTT runner's connect step waits on esp-mqtt's `api_lock` and on the user's `on_connect` with no bound (dated 2026-11-11 below).
   Dated 2026-11-05: on esp-hal, `shutdown` wakes `run_sender`, which returns after the frame on the air has finished, so it is the one esp-hal persistent service with a defined exit; the esp-hal MQTT runner and `WifiLink::run` never return.
   Dated 2026-11-11: `ConnectWorkMachine` is finite per connect, and it reaches a terminal action, `Report` (`WorkDone` or `WorkFailed`) or `Abandon`, within its plan length plus two actions; the IDF tier drops a work that answers `Ignored` and logs an error, and the hal `serve` returns.
   The IDF connect step is the fifth exception: a step waits on esp-mqtt's `api_lock` for at most `MQTT_NETWORK_TIMEOUT_MS` on a dead link, and on the user's `on_connect` with no bound, since ADR 017 asks for a short callback and the tier warns past 1 s.
   On ESP-IDF the MQTT runner closes the session and returns when its last handle drops or its inputs close, `EspNowWorker::stop` joins the `espnow-tx` thread (also on drop), and `LinkSupervisor` joins its worker on drop.
   A second `run_sender` parks forever by design, after it logs an error.
   The exceptions and the open gaps are listed in [ladder-closure-v1](../features/ladder-closure-v1.md).
7. Review rule (from [ADR 022](022-tier-parity-one-async-api.md) decision 10): a branch in a tier adapter that is not an error or event mapping is logic and moves into `juggler`.
8. Per-chip behaviour is a runtime value passed in by the tier, not a `cfg` gate inside a machine.
   The ESP32-C3 TX-power clamp lives in the hal adapter, and the SoftAP gate (`esp_idf_esp_wifi_softap_support`) is tier build configuration.
   Dated 2026-11-05: no C3-specific clamp exists, because the hal adapter calls `set_tx_power_or_log` on every chip, always on the AP path and on the STA path only when `tx_power` is not the default; the drift is recorded in [ladder-closure-v1](../features/ladder-closure-v1.md).
9. Tier-specific facts stay in the adapters: esp-mqtt `api_lock` and callback threading ([ADR 017](017-mqtt-callback-threading-contract.md)), worker threads, radio register calls, and provider selection ([ADR 021](021-binary-owned-providers.md)).
    Dated 2026-11-11: the IDF MQTT runner takes the client lock only for one bounded connect step at a time, and never while a callback could be parking the mqtt task; the event-loop thread never takes it ([ADR 017](017-mqtt-callback-threading-contract.md)), and the runner owns the connect work's thread, so no connect helper thread exists.
10. A port trait is narrow: one driver call or one event mapping per method, with no decisions.
    A generic runner over a wide port is not used, because the port would have to model the whole flow.
    Dated 2026-11-05: `EspNowRadio` (`juggler::espnow::worker`) is the IDF ESP-NOW instance, with six methods (`send`, `set_channel`, `tx_power`, `set_tx_power`, `add_peer` and `remove_peer`) that each make one driver call, except that `send` with `repin` lets the tier wrap the send in its chip-specific re-pin bracket (a tier fact, ADR 023 decision 9).
    `IdfRadio` in the IDF adapter implements it.
    Dated 2026-11-11: `ConnectPort` (`juggler::mqtt::connect_work`) is the IDF MQTT instance, with four methods (`subscribe`, `publish_startup`, `run_on_connect` and `finish`); the first three make at most one esp-mqtt or `on_connect` call each, and `finish` sends one SUBSCRIBE per runtime request stored meanwhile before it marks the tracker live, the same exception form this decision already records for `EspNowRadio::send`.
    `perform` maps one `WorkAction` through the port.
    The hal tier has no `ConnectPort`, because its `serve` maps the same actions with one await each.
11. A one-time retrofit applies the ladder to all existing code before the next release, followed by one hardware run across the affected flows.
    After the retrofit, moving a flow up a rung is demand-driven: the first change that needs it pays for it.

## Consequences

### Positive

- Shared step orders are host-testable in `juggler`, and both tiers run the same order.
- Adapters shrink to I/O and error mapping, which the review rule can check.
- The ladder keeps machines out of flows that a generic driver or a pure function covers.
- The ESP-IDF and esp-hal tiers share one vocabulary for the rungs, so a reviewer can compare them point by point with the siblings.

### Negative

- Rung 3 adds one indirection: a reader follows an action from `juggler` into the tier loop.
- Host tests prove the order, not the hardware; the bench runs remain the arbiter.
- Constraints on the tier loop, such as running inside a critical section, holding a lock, or reading a pin late, are not checked by any type.

### Constraints

- Machines are `no_std` and free of async; inputs, actions and the machine are small `Copy` values where practical.
- Rung 2 and 3 machines need state-transition tests in `juggler`.
- A change to a rung-3 machine re-runs the bench for each flow it drives.
  Dated 2026-11-11: the bench rerun for the Step 8c change to `ConnectWorkMachine` is the S3 check set `suspend`, `mqtt-scenarios`, `mqtt-lwt`, `connect`, `dns-subscribe`, `dns-unresolvable` and `soak` on both tiers; the runs are pending, and the constraint is met only when they pass.

## Alternatives Considered

| Alternative                  | Pros                                | Cons                                                                 | Why Rejected                                                                |
|:-----------------------------|:------------------------------------|:---------------------------------------------------------------------|:----------------------------------------------------------------------------|
| Adopt power ADR-004 verbatim | Identical wording across the family | Rejects a generic runner over a port, which ADR 022 decision 10 uses | The narrow custom ports here are what make `LinkSupervisor<R>` acceptable   |
| Action machines everywhere   | One uniform shape                   | Ceremony for flat logic and `embedded-hal` drivers                   | No drift to fix; adds indirection for nothing                               |
| Generic drivers only         | Simplest; already in place          | A flow that must run in threads and async has no shared path         | Leaves the Wi-Fi and MQTT reconnect order to drift                          |
| Tier-owned sequencing        | Each tier tunes its own flow        | Two copies of the order drift apart                                  | The failure mode that [ADR 022](022-tier-parity-one-async-api.md) addresses |

## References

- [ADR 016](016-crate-consolidation-for-publishing.md), [ADR 017](017-mqtt-callback-threading-contract.md), [ADR 021](021-binary-owned-providers.md), [ADR 022](022-tier-parity-one-async-api.md) — the decisions this one builds on
- `rustyfarian-power` ADR-004 (`docs/adr/004-shared-logic-ladder.md`) — the sibling ladder in the power repo
- `rustyfarian-peripherals` ADR-010 (`docs/adr/010-shared-logic-ladder.md`) — the sibling ladder in the peripherals repo
