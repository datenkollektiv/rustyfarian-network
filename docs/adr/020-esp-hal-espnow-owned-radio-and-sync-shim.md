# ADR 020: ESP-NOW on the esp-hal Tier — Owned Radio, Sync Shim, Shared Scan Machine

## Status

Accepted — 2026-10-30 by the maintainer; the implementation is in place and the hardware gates G-E1 and G-E2 in [esp-hal-espnow-v1.md](../features/esp-hal-espnow-v1.md) stay open.
Decision 1 and the Consequences were amended in place on 2026-10-30 after an external review (the settle window was replaced by the sender runner `EspHalEspNow::run_sender`) and on 2026-10-31 after a third review (probe deadlines, in-flight transmissions waited out, a single runner enforced).

## Context

ADR 007 deferred a bare-metal ESP-NOW crate until a use case materialised.
A downstream escape-room project now plans esp-hal twins of its brain (ESP32-C6, Wi-Fi station plus ESP-NOW), matrix and nunchuk (ESP32-C3, ESP-NOW only) firmwares, and mixed idf/hal fleets must interoperate frame for frame.

`esp-radio 1.0.0-beta.1` already ships ESP-NOW behind its `esp-now` feature: `WifiController::esp_now()` returns an `EspNow` that splits into `EspNowManager`, `EspNowSender` and `EspNowReceiver`.
Line references in this section are into upstream `esp-radio 1.0.0-beta.1`, not into this repository.
Three facts shape the design:

- `EspNowSender::send(&mut self)` returns a `SendWaiter` whose `wait` and `Drop` busy-spin until the send callback fires, and the docs warn that a lock which masks interrupts blocks forever (`esp_now/mod.rs:645-679`).
- `EspNowReceiver::receive(&self)` pops a static 10-frame queue, and `receive_async(&mut self)` / `send_async` exist (`:694`, `:984`, `:992`).
- `WifiController::new` starts the station without connecting and without any network stack (`wifi/mod.rs:3082`); the `esp-radio` init panics without a running preemptive scheduler (`lib.rs:316`).

The idf tier's `scan_for_peer` (ADR 009, with the confirmation gating of the roaming-peer fix) is the behavioural parity target; it is a loop over blocking calls and `std::thread::sleep`, which an embassy task cannot reuse.

## Decision

1. **Same contract.** `rustyfarian_esp_hal_network::espnow::EspHalEspNow` (feature `espnow`, chips `esp32c3` and `esp32c6`) implements the sync `juggler::espnow::EspNowDriver` with `&self`, backed by `critical_section::Mutex<RefCell<..>>` cells.
   The sender cell is leased for the duration of a sync send and returned afterwards, so no critical section spans the TX wait and a concurrent sync send receives `Busy`.
   Async sends and scan probes never touch the cell: they queue their frame for `EspHalEspNow::run_sender`, a runner that is spawned once in a task that is never cancelled, polls every send to completion and never drops a `SendFuture`.
   Only one runner runs per handle; a second concurrent runner logs an error and idles.
   Async callers serialize on an async mutex and wait for their own completion, so they receive `NoSenderRunner` when no runner is active, not `Busy`.
   Inherent async helpers (`recv_async`, `send_async`, `send_async_timeout`, an async `scan_for_peer`) serve embassy tasks; a timeout is an absolute `embassy_time` deadline that bounds queueing and the start of transmission.
   The runner drops a request whose deadline passed before its transmission started, unsent; a transmission that has already started is waited out, and the caller returns its real result even when that is later than the deadline.
2. **Two constructors.** Caller-managed (the controller of a joined station, brain) and owned radio (unassociated station, no SSID, no `embassy-net`; matrix and nunchuk).
   Both need `esp_rtos::start` and a heap; neither needs an embassy executor for the sync API.
3. **One scan algorithm.** `juggler::espnow::ScanMachine` is a pure, clock-injected state machine (`ScanInput` in, `ScanAction` out) that reproduces `EspIdfEspNow::scan_for_peer` exactly.
   The hal tier drives it with `send_async_timeout` (through `send_probe`) and `embassy_time`; the idf tier has adopted it without behaviour change.
4. **Channel stability is gated, not assumed.** The owned radio ships as an unassociated station; the mitigation ladder (per-send `set_channel`, hidden AP+STA, promiscuous bracket) is applied only if gate G-E1 fails, because ADR 012 showed the idle-scan drift is a blob property and the SoftAP-only answer changes the peer MAC.
5. **Tuning.** `new_owned` disables AMPDU RX/TX through `ControllerConfig`, and `from_controller` leaves that to the caller; `esp-radio/coex` stays off; TX power is a quarter-dBm passthrough to `WifiController::set_max_tx_power`, applied by `new_owned` (default 13 dBm) and by `from_controller` only when the config sets a level.

## Consequences

### Positive

- Consumers program against one trait and one scan algorithm on both tiers; the scan semantics (stale-peer removal, confirmation gap, burst budget, pin restore) are covered by host tests instead of hardware only.
- No dummy SSID or DHCP stack on ESP-NOW-only boards.
- The idf tier is untouched.

### Negative

- `send` blocks for the TX duration (about 1-30 ms) because `SendWaiter::drop` spins; async tasks should prefer `send_async`, which needs the sender runner.
- The receive queue is 10 frames and drop-oldest; it cannot be enlarged without an upstream `esp-radio` change.
- Everything rests on `#[instability::unstable]` `esp-radio` APIs pinned at `=1.0.0-beta.1`.
- Channel drift on the unassociated station is unproven until G-E1 passes.
- A cancelled caller only stops waiting: the runner finishes the transmission and discards its result, so a late send callback cannot be taken as the next send's completion (`esp-radio` has one global send-callback flag and no sequence number).
  A timed-out caller whose transmission has started waits it out and gets the real result, and a request still queued at its deadline is dropped unsent, so the timeout bounds queueing and the start of transmission, not an in-flight transmission.
  The cost is a firmware requirement: `run_sender` must be spawned once (a second runner is rejected), and without it the async senders return `NoSenderRunner`.
- Fail-closed: if the radio never fires the send callback, `run_sender` waits forever and with it every async sender, like `esp-radio`'s own send waiter.
  No timeout can abandon that completion without reopening the attribution hazard.
- Dropping the `run_sender` future by mistake clears the running flag, and frames already queued then never complete.

## Alternatives Considered

- **Async-only `EspNowDriver`** — breaks the mock, `PeerTracker` call sites and the idf impl for one tier's convenience.
- **Holding the critical section across `SendWaiter::wait`** — deadlocks, per the `esp-radio` documentation.
- **A second, hand-written scan loop on the hal tier** — doubles the test matrix and invites the divergence ADR 009 spent effort removing.
- **Hidden SoftAP as the first design** — changes the transmit MAC from the station MAC, which the consumer uses to address peers on both tiers.
- **Promiscuous bracket as the first design** — measured at 0-20 % loss in ADR 012.

## References

- [ADR 007](007-espnow-abstraction.md), [ADR 009](009-espnow-channel-scanning.md), [ADR 012](012-espnow-unassociated-sta-channel-stability.md)
- [ADR 019](019-esp-hal-async-mqtt-client.md) — the other esp-hal tier decision on the same radio stack
- `docs/features/esp-hal-espnow-v1.md` — feature doc, decisions and gates
- Upstream `esp-radio 1.0.0-beta.1`: `src/esp_now/mod.rs`, `src/wifi/mod.rs:2791,3189,3206`
