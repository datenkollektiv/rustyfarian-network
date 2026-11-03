---
gate: none
desk-work: available
---

# Feature: ESP-NOW Peer Link v1

Goal: one tested policy decides when a peer scans for its coordinator and follows a channel change, on both tiers.
Requested by rustbox-rgb-puzzle (2026-10-31); depends on [espnow-tier-parity-v1](espnow-tier-parity-v1.md) for the helper, not for the core.

## Decisions

| Decision                                                                   | Reason                                         | Rejected Alternative              |
|:---------------------------------------------------------------------------|:-----------------------------------------------|:----------------------------------|
| Sans-IO `PeerLink` in `juggler::espnow`                                    | Host-tested, `no_std`, same code on both tiers | Hand-written policy per firmware  |
| Types `PeerLinkEvent` / `PeerLinkAction` / `PeerLinkState`                 | `juggler::wifi::LinkEvent` already exists      | The request's `LinkEvent` names   |
| Events `Boot`, `Acked`, `NotAcked`, `PeerHeard`, `ScanFound`, `ScanFailed` | A received frame proves the link too           | ACKs as the only proof            |
| Rescan backoff as plain config                                             | Failed scans spaced out without a trait        | `RescanStrategy` trait (consumer) |
| Rescan after N consecutive `NotAcked`, default 20                          | The consumer's tuned value at 1 s sends        | A time window                     |
| One generic async helper over the parity trait                             | The same helper on both tiers                  | Per-tier helpers                  |
| The driver keeps the pinned channel                                        | Decided in espnow-tier-parity-v1               | `PeerLink` owns the pin           |

## Constraints
- The core is `no_std`, allocation-free, and takes time as `now_ms`.
- The core never loops on its own, like `ScanMachine`.
- A `Scan` action is issued at most once until `ScanFound` or `ScanFailed` arrives.
- The ESP-IDF tier's blocking `send` hides a missing ACK, so the helper uses ACK-reporting sends.

## Open Questions
- [ ] Backoff defaults (first delay, cap, factor)?
- [ ] When is the state `Lost`: at the failure threshold, or only after a failed rescan?
- [ ] Helper shape: a loop that owns the sends, or an observer the app feeds its send results?
- [ ] Does `PeerLink` absorb or sit beside the existing `PeerTracker` heartbeat tracker?
- [ ] The consumer's in-project core (`rgb-puzzle-link`) was not found locally; compare once it is shared.

## State
- [x] Design approved
- [ ] `PeerLink` core with host tests and mutation checks
- [ ] Generic helper over the espnow-tier-parity-v1 trait
- [ ] Peer examples on both tiers use the helper
- [ ] Documentation updated
- [ ] Hardware check: the peer follows a coordinator channel change on both tiers

## Session Log
- 2026-11-03 — Feature doc created via /feature dialog from `review-queue/rustyfarian-network-espnow-peer-link-v1.md`
