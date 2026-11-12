---
gate: none
desk-work: available
---

# Feature: ESP-NOW Peer Link v1

Goal: one tested policy decides when a peer scans for its coordinator and follows a channel change, on both tiers.
Requested by rustbox-rgb-puzzle (2026-10-31); depends on [espnow-tier-parity-v1](espnow-tier-parity-v1.md) for the helper, not for the core.

## Decisions

| Decision                                                                   | Reason                                                    | Rejected Alternative              |
|:---------------------------------------------------------------------------|:----------------------------------------------------------|:----------------------------------|
| Sans-IO `PeerLink` in `juggler::espnow`                                    | Host-tested, `no_std`, same code on both tiers            | Hand-written policy per firmware  |
| Types `PeerLinkEvent` / `PeerLinkAction` / `PeerLinkState`                 | `juggler::wifi::LinkEvent` already exists                 | The request's `LinkEvent` names   |
| Events `Boot`, `Sent(SendOutcome)`, `PeerHeard`, `ScanFound`, `ScanFailed` | A received frame proves the link too                      | ACKs as the only proof            |
| Rescan backoff as plain config                                             | Failed scans spaced out without a trait                   | `RescanStrategy` trait (consumer) |
| Rescan after N consecutive `NoAck`, default 20                             | The consumer's tuned value at 1 s sends                   | A time window                     |
| One generic async helper over the parity trait                             | The same helper on both tiers                             | Per-tier helpers                  |
| The driver keeps the pinned channel                                        | Decided in espnow-tier-parity-v1                          | `PeerLink` owns the pin           |
| An `Acked` send proves the link like `PeerHeard`                           | After a failed scan the radio is back on the last channel | ACKs count only while `Linked`    |
| `rescans()` counts every scan issued while `Lost`, retries included        | A health metric for how hard the peer is searching        | Only threshold-triggered scans    |

## Constraints
- The core is `no_std`, allocation-free, and takes time as `now_ms`.
- The core never loops on its own, like `ScanMachine`.
- A `Scan` action is issued at most once until `ScanFound` or `ScanFailed` arrives.
- The ESP-IDF tier's blocking `send` hides a missing ACK, so the helper uses ACK-reporting sends.

## Requirements
- Send results reach the core as `Sent(SendOutcome)`; only `NoAck` counts toward the rescan threshold, and `Local` is counted separately and never triggers a scan.
- Timer contract: `poll(now_ms, Option<PeerLinkEvent>) -> PeerLinkAction`, with actions `None`, `Scan { id }` and `WaitUntil(ms)`, so a failed boot scan retries after its backoff even without application traffic.
- The helper sleeps until the earlier of `WaitUntil` and the next send result or received frame, like the `ScanMachine` driver loops.
- Ordering: each `Scan` carries an id, and `ScanFound` / `ScanFailed` with another id are ignored as stale.
- Ordering: a send result submitted before the current scan started is ignored, so an old `NoAck` cannot count against the new channel.
- Only frames whose sender is the configured coordinator MAC produce `PeerHeard`; unrelated frames are ignored.
- A `PeerHeard` during a scan resets the failure count but does not cancel the scan; if that scan then fails, the link stays `Linked` on the pinned channel instead of `Lost`.

## Open Questions
- [x] Backoff defaults (first delay, cap, factor)? — 2 s first retry, doubling, capped at 60 s (`PeerLinkConfig`)
- [x] When is the state `Lost`: at the failure threshold, or only after a failed rescan? — from the threshold-triggered rescan until a scan succeeds or the coordinator is heard or ACKs on the last channel
- [x] Helper shape: a loop that owns the sends, or an observer the app feeds its send results? — a `PeerSession` handle that owns the sends plus a `run` loop (plan Step 6)
- [x] Does `PeerLink` absorb or sit beside the existing `PeerTracker` heartbeat tracker? — beside it; `PeerTracker` stays the heartbeat-timeout tracker
- [ ] The consumer's in-project core (`rgb-puzzle-link`) was not found locally; compare once it is shared (deferred to the consumer).

## State
- [x] Design approved
- [x] `PeerLink` core with host tests and mutation checks
- [x] Generic helper over the espnow-tier-parity-v1 trait
- [x] Peer examples on both tiers use the helper
- [x] Documentation updated

<details><summary>Acceptance</summary>

- Host tests: unrelated received frames, recovery during backoff, stale scan results, stale send results, `Local` outcomes never trigger a scan, boot-scan retry without traffic.
- Mutation check on the threshold counting and the stale-id guard.
- Hardware: bench check `c3/espnow-peer-session`.

</details>

## Session Log
- 2026-11-03 — Feature doc created, design review folded in, `PeerLink` core implemented (`SendOutcome::Error` renamed `Local`, esp-hal probe timeouts count as `Local`), helper shape settled as `PeerSession`
- 2026-11-11 — `PeerSession` landed in `juggler::espnow::session` and both C3 peers run the shared `examples-shared/espnow_peer_app.rs`; review pass applied (a `run` dropped mid-scan hands the scan back, `EmbassyClock` clamps its deadline, local send errors log at `debug`)
- 2026-11-12 — Bench check `c3/espnow-peer-session` passed on both tiers; archived ahead of espnow-tier-parity-v1, which followed the same day
