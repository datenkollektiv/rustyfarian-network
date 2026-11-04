# ADR 023: Shared tier logic follows a three-rung ladder

## Status

Accepted (2026-11-04).
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
  `LinkSupervisor<R: LinkRadio>` has an IDF port, `IdfRadio`, that maps four `esp_wifi_*` calls, and it is host-tested against `MockRadio`; `TxCore<R, N>` follows the same shape.
- The cores that drive the machines may hold injected wake-up primitives (`embassy-sync`, stored `Waker`s), as [ADR 022](022-tier-parity-one-async-api.md) decision 10 already allows.
  The machines themselves stay free of async.
- `juggler` uses `alloc` (the MQTT outbox, chunk reassembly), so "allocation-free" applies to the machines only where practical.

## Decision

1. Shared logic takes the lowest rung that fits:
   - **Rung 1 — generic driver.** The flow lives in `juggler`, generic over `embedded-hal` or `embedded-storage` traits.
     No shared instance exists in `juggler` today.
     The nearest shape is tier-local: the hal `ProvisioningStore<F: NorFlash>` is generic over `embedded-storage`, but the IDF tier keeps its own store.
   - **Rung 2 — event machine.** The core decodes samples: `update(sample, now) -> Option<Event>`.
     A tier, an ISR, or an `embedded-hal` adapter feeds it.
     No rung-2 instance exists today.
   - **Rung 3 — action machine.** The core emits I/O requests: `poll(&mut self, input) -> Action`, and a thin loop per tier performs them.
     Examples are `LinkMachine` (`juggler::wifi::link`), `SessionMachine` (`mqtt::session`), `ScanMachine` (`espnow::scan`), and `HealthMachine` and `ReporterMachine` (`ota::runtime`).
2. Rung 3 is used only when a flow meets one of these triggers:
   - It must run under more than one execution model, such as threads, events, or cancel-safe async.
   - The tiers perform I/O outside `embedded-hal`, such as `esp_wifi_*` or esp-radio calls, but must share the order of the steps.
3. Flat logic stays as pure functions or stateless types in `juggler`, not machines.
4. Time is an input: a machine never reads a clock; the caller passes `now` or reports elapsed waits.
   Wake-ups are injected the same way.
5. A rung-3 machine answers out-of-phase input with an explicit no-op action, so a host test of a tier loop can detect it.
   `LinkAction::None` and `SessionAction::None` currently mean both "nothing to do" and "input ignored".
   The rule applies to new machines; an existing machine adopts it when a change touches it, with no retrofit now.
6. Every rung-3 flow reaches a terminal action, and the tier loop bounds its iterations or its time budget.
   A machine that follows decision 5 lets the loop treat the no-op action as an error exit.
7. Review rule (from [ADR 022](022-tier-parity-one-async-api.md) decision 10): a branch in a tier adapter that is not an error or event mapping is logic and moves into `juggler`.
8. Per-chip behaviour is a runtime value passed in by the tier, not a `cfg` gate inside a machine.
   The ESP32-C3 TX-power clamp lives in the hal adapter, and the SoftAP gate (`esp_idf_esp_wifi_softap_support`) is tier build configuration.
9. Tier-specific facts stay in the adapters: esp-mqtt `api_lock` and callback threading ([ADR 017](017-mqtt-callback-threading-contract.md)), worker threads, radio register calls, and provider selection ([ADR 021](021-binary-owned-providers.md)).
10. A port trait is narrow: one driver call or one event mapping per method, with no decisions.
    A generic runner over a wide port is not used, because the port would have to model the whole flow.
11. Moving a flow up a rung is demand-driven: the first change that needs it pays for it.

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
