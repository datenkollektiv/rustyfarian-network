---
gate: none
desk-work: available
---

# Feature: ESP-NOW Tier Parity v1

Goal: ESP-NOW consumer code is exchangeable between the tiers ([ADR 022](../adr/022-tier-parity-one-async-api.md), Phase 6 of the [tier-parity plan](tier-parity-v1-plan.md)).
Prerequisite for [espnow-peer-link-v1](espnow-peer-link-v1.md).

## Decisions

| Decision                                               | Reason                                            | Rejected Alternative                    |
|:-------------------------------------------------------|:--------------------------------------------------|:----------------------------------------|
| Shared async trait in `juggler::espnow`                | One app on both tiers (ADR 022 decision 4)        | Per-tier helpers with diverging shapes  |
| The driver keeps the pinned channel                    | One `scan_for_peer(&mac, &config)` shape          | Caller-held `&mut Option<u8>` (esp-hal) |
| ESP-IDF blocking scan becomes `scan_for_peer_blocking` | The async scan takes the shared name; a 0.x break | A new async name on both tiers          |
| ESP-IDF async runs on one `espnow-tx` worker           | Fixed stack like `mqtt-net` (ADR 022 variant A)   | A thread per call (the bug 013 pattern) |
| The async send reports the MAC-layer ACK               | A link policy needs it; ESP-IDF `send` hides it   | Fire-and-forget send in the trait       |

## Constraints
- ADR 021 holds: the firmware owns the executor, the timer queue and the providers.
- ADR 020 holds: on esp-hal the trait sits on the `run_sender` runner.
- ADR 012 holds: scanning needs a driver-owned radio on ESP-IDF.
- esp-hal ESP-NOW is unreleased, so its API changes freely.
- ESP-IDF changes are additive except the approved `scan_for_peer` rename.
- Every step ends with `just fmt`, `just verify`, `just clippy-hal` and `just build-example` for touched examples.

## Open Questions
- [ ] Trait and method names: `send_async` / `recv_async` (esp-hal today) or `send` / `receive`?
- [ ] Does the trait cover receive, or does each tier keep its own receive path?
- [ ] Do the sync `send_and_wait` forms converge (ESP-IDF takes `timeout_ms`, esp-hal does not)?
- [ ] Stack and heap cost of the `espnow-tx` worker, measured against bug 013.
- [ ] Hardware gate: mixed IDF/hal fleet run, shared with the esp-hal-espnow-v1 validation plan.

## State
- [x] Design approved
- [ ] Extraction inventory (tier logic that is not mapping moves to `juggler`)
- [ ] Shared trait and mock in `juggler`
- [ ] esp-hal implements the trait, pin moved into the driver
- [ ] ESP-IDF `espnow-tx` worker and async surface, blocking scan renamed
- [ ] Parity compile check on both tiers
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-03 — Feature doc created via /feature dialog, split from the peer-link request
