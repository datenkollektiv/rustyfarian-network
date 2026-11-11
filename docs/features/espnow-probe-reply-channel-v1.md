---
gate: none
desk-work: available
---

# Feature: ESP-NOW Probe Reply Carries the Coordinator's Channel v1

Fix for bug 018: a scan accepts a MAC-layer ACK as "found", and an adjacent channel decodes and ACKs the probe, so the peer pins channel 10 while the coordinator sits on 11.
Ships inside release 0.7.0 (user decision 2026-11-11).

## Decisions
| Decision | Reason | Rejected Alternative |
|:----------|:--------|:----------------------|
| Reply wins, ACK stays the fallback | Old and third-party receivers stay reachable | Reply mandatory (fleet must upgrade first) |
| Driver-side tap, replies to the outstanding probe never reach the app (approach A) | Works for the blocking scout and needs nothing from consumers; a `0xF4` between probes is an application frame | App forwards replies (silent degradation); scan owns the receiver (IDF callback cannot be owned) |
| Wire: probe `[0xF3, seq, tx_channel]`, reply `[0xF4, seq, tx_channel, home_channel]` | ADR 010 envelope, 3 and 4 bytes, stale and foreign replies rejected by `seq` and `tx_channel` | Magic-prefixed free payload; RSSI in the reply (esp-idf-svc 0.53 exposes none) |
| A matching reply ends the scan with `found = home_channel`, no re-probe | The coordinator states its channel, so a probe on it proves nothing more; accepted cost: a coordinator mid-roam can name a transient channel (ADR 025 negative consequence) | Jump to `home_channel` and probe again |
| ACK without a reply is a candidate, settled after the whole list with today's confirmation rule | Roaming protection and legacy behaviour kept, adjacent hits lose to a later reply | First ACK-only hit wins (bug 018 as today) |
| New `ScanAction::AwaitReply { until }` and `ScanInput::Reply(ProbeReply)`, `Completed` on timeout | Same action and input shape as `WaitUntil`, poll budget stays bounded (3 per channel, plus `3 + 3 * probe_confirmations` for the candidate phase and one home-channel switch) | Fold the reply into `SendOutcome` (shared with app sends) |
| `ScanConfig::reply_timeout`, default 50 ms after the ACK | A coordinator answers within a few ms; tunable like `probe_timeout` | Reuse `confirmation_gap` (150 ms, a different purpose) |
| IDF tap in the receive callback, mailbox in the worker's `Shared`, woken through the existing condvar | Wi-Fi task context delivers the reply whether or not the app receives | Worker polls `RxQueue` (single consumer, app competes) |
| hal tap in `receive()`; the scan leases the receiver itself when no `receive()` is pending, non-reply frames go to a 4-frame drop-oldest holding buffer that `receive()` and `try_recv` drain first | A bare `scan_for_peer` sees replies and app frames survive a scan | Require a receive loop during a scan (undocumented degradation) |
| `EspNowLink::current_channel()` on both tiers (`esp_wifi_get_channel`, esp-radio `channel()`) | The echo app needs the live channel, parity (ADR 022) | Pass the channel into the echo app from the Wi-Fi link |
| Shared echo app (parity plan Step 5) moves into 0.7.0; 0xF3 answered with 0xF4, other frames echoed verbatim | Both coordinators must answer, the IDF one echoes nothing today | Patch each coordinator example separately |
| `ScanConfig::probe_data` deprecated, the scan sends the 0xF3 frame | One probe format, callers keep compiling | Remove the parameter (breaking in a minor) |
| New ADR 025 supersedes ADR 009's "no protocol changes are needed" and extends ADR 010's tag table | Durable protocol rationale belongs in an ADR | Amend ADR 009 in place |

## Constraints
- Both tiers land the same names and shapes (ADR 022): `ProbeReply`, `current_channel()`, the tap behaviour.
- `ScanMachine` stays pure and clock-injected; `poll_bounded` stays bounded at 3 polls per channel, plus `3 + 3 * probe_confirmations` for the candidate phase and one home-channel switch (the `max_polls` rustdoc in `scan.rs`).
- The reply tap consumes only `0xF4` frames from the scan's target MAC while a probe is outstanding (from its send until the scan moves past its reply wait); everything else, a `0xF4` between probes included, reaches the application unchanged.
- Each scan continues the previous scan's probe sequence (`ScanMachine::with_first_seq`, both tiers), so a reply left over from the last scan never matches.
- The hal holding buffer drops oldest and counts drops, like esp-radio's own 10-frame queue.
- `probe_timeout` semantics are unchanged; `reply_timeout` starts at the ACK.
- Host tests first: the `Peer` mock gains adjacent-channel ACKs and reply or no-reply behaviour; mutation checks on the tag guard, the `seq` match, the `home_channel` byte and the candidate fallback.
- `just fmt`, `just verify`, `just clippy-hal`, `just test-espnow`, and `just build-example` for both coordinators, both C3 peers and the scout stay green.
- Bench: `just bench c3 espnow-*` five checks per tier rerun with the hal C6 coordinator; C3-I7 and C3-H6 pass their same-channel step; bug 018 closes against the run.

## Open Questions
- [x] hal `current_channel()` source: the esp-radio controller's `channel()` or the ESP-NOW manager; the driver may need the controller handle kept. Resolved: the Wi-Fi driver's `esp_wifi_get_channel` read (the query behind `WifiController::channel`), which works on both constructors without a controller lease.
- [ ] `reply_timeout` default: measure the ACK-to-reply gap on the bench before fixing 50 ms.
- [ ] Does the rgb-puzzle coordinator adopt the echo app in this round or stay ACK-only (fallback covers it)?

## State
- [x] Design approved (brainstorm 2026-11-11, approach A, reply-wins)
- [x] Wire format and `ProbeReply` in `juggler::espnow::command` with tests
- [x] `ScanMachine`: `AwaitReply`, `Reply`, candidate fallback, host tests and mutation checks
- [x] IDF tap, mailbox and `current_channel()`
- [x] hal tap, holding buffer, scan-side receiver lease and `current_channel()`
- [x] Shared echo app on both coordinators; scout and peers send 0xF3
- [x] ADR 025, CHANGELOG, runbook C3-I7/C3-H6 criteria, parity plan Step 5 note
- [x] Bench rerun and bug 018 closed (2026-11-11: five checks per tier, every scan on channel 11; the hal attended half of the cancel check is still owed for gate C3-H6)

## Session Log
- 2026-11-11 — Feature doc created via /feature after the bug 018 brainstorm; implementation starts in a workflow
- 2026-11-11 — Workflow implementation done through the docs step: ADR 025 written, ADR 009 consequence superseded, ADR 010 system-tag table added, runbook C3-I7/C3-H6 criteria and bug 018 updated, lore entry recorded; open: bench rerun, roadmap (technical-writer)
- 2026-11-11 — Review pass applied: hal `await_reply` retries the receiver lease and checks the deadline first (both tiers), scans continue the probe sequence (`with_first_seq`), echo app forgets the oldest sender instead of refusing a ninth, poll-budget, tap-window and roaming wording corrected in ADR 025 and here, `ScanConfig::reply_timeout` recorded as breaking
- 2026-11-11 — Bench rerun passed on both tiers (flood, txpower, shutdown, shutdown-queued, cancel unattended), bug 018 closed; the cancel check now splits into an unattended coordinator-on half and an attended off-and-on half with a 15 s settle step after the power cycle
