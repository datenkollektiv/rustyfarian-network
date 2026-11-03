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

## Requirements
- Outcome classification: the shared error type exposes one portable classification of every send result, `Acked`, `NoAck` (transmitted, no MAC-layer ACK) or `Local` (queue full, no sender runner, invalid peer, driver error, timed out before transmission).
- Rename `juggler::espnow::SendOutcome::Error` to `Local` and map every tier error onto `SendOutcome` through a trait method, so the generic helper never matches tier error variants.
- Esp-hal `Timeout` means the frame never started (ADR 020), so `probe_outcome` maps it to `Local` instead of today's `NoAck`.
- Cancellation: a caller that stops waiting never receives a stale completion, and a started transmission always runs to its end (esp-hal `run_sender` already does this); the ESP-IDF `espnow-tx` worker must match it with request ids.
- Timeout: the timeout bounds queueing and the start of transmission; a transmission that has started is waited out and reports its real result (esp-hal semantics, ADR 020).
- Scan ownership: a scan owns the radio; a concurrent scan fails at once with `Busy`, and an application send during a scan fails at once with a `Local` outcome instead of changing the channel or queueing in front of probes.
- Scanning stays limited to a driver-owned radio on both tiers (ESP-IDF `init_with_radio*`, esp-hal `new_owned`); on a Wi-Fi-associated device the router owns the channel and `scan_for_peer` is refused.
- Receive is part of the shared trait (`async` receive returning the sender MAC), because the peer link counts received frames and portable apps must not need a tier receive path.

## Open Questions
- [ ] Trait and method names: `send_async` / `recv_async` (esp-hal today) or `send` / `receive`?
- [x] Does the trait cover receive, or does each tier keep its own receive path? — yes, see Requirements
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

<details><summary>Acceptance</summary>

- One unchanged generic application compiles against both tiers (parity compile check plus an example pair, like the tier-parity LWT pair).
- Host tests: cancellation without stale results, queue pressure classified `Local`, sends during a scan refused.
- Hardware: mixed-tier fleet in both directions (ESP-IDF peer with esp-hal coordinator and the reverse).

</details>

## Session Log
- 2026-11-03 — Feature doc created via /feature dialog, split from the peer-link request
- 2026-11-03 — Design review folded in: outcome classification, cancellation and timeout, scan ownership, receive in the trait
