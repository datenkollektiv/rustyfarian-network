# ADR 025: ESP-NOW Probe Reply Carries the Coordinator's Channel

## Status

Accepted (2026-11-11).
Supersedes the "No protocol changes are needed" consequence of [ADR 009](009-espnow-channel-scanning.md) and extends the system-tag table of [ADR 010](010-espnow-command-frame-without-transport-metadata.md).

## Context

[ADR 009](009-espnow-channel-scanning.md) chose active probe scanning: the scan walks the channel list, sends a unicast probe on each and takes the first MAC-layer ACK as the coordinator's channel.
The ACK is produced by the 802.11 hardware of whichever radio decodes the frame, and that is not the same thing as the channel the coordinator is parked on.

[Bug 018](../bugs/archive/018-espnow-scan-finds-adjacent-channel-2026-11-10.md) showed this on the 0.7.0 bench: with the C3 scout and the C6 coordinator on router channel 11, `scan_for_peer` found the coordinator on channel 10, then on 11, and gate C3-I7 failed its same-channel step.
2.4 GHz channels are 5 MHz apart while a frame is 20 MHz wide, so a probe sent on an adjacent channel is decoded and ACKed, at close range reliably and at opposite ends of a table still often enough to flap.
`ScanConfig::probe_confirmations` does not help, because the adjacent-channel ACK repeats as reliably as the real one.
The scan walks the list upwards from 1, so the adjacent hit below the real channel wins.

[ADR 012](012-espnow-unassociated-sta-channel-stability.md) had already recorded a false positive of the same family: during the promiscuous-bracket attempts "the scan produced false positives (e.g. coordinator found on channel 9 when it was on channel 1) because the background scanner moved the physical channel between `esp_wifi_set_channel` returning and the probe being transmitted".
The roaming-peer entry in the project lore is a third case: a peer's STA ACKs unicast frames on every channel it visits during an AP scan.
All three share one root: a MAC-layer ACK proves that some radio decoded the frame, not which channel the peer lives on.

The fix needs the coordinator to state its channel.
Doing that requires a frame the coordinator answers, which is the protocol change ADR 009 said was not needed.

### Approaches evaluated

**Probe N+1 after a hit and prefer the higher channel** (the bug's first idea): keeps the protocol but only moves the ambiguity; a peer on channel 6 is ACKed from 5 and 7 alike, and the rule picks wrong when the hit comes from above.

**Reply mandatory**: the scan trusts only a reply and treats an ACK alone as not found; every coordinator in a fleet must upgrade before any peer does.

**App forwards replies**: the coordinator answers, but the peer's application loop must hand reply frames back to the scan; a bare `scan_for_peer` without a receive loop degrades silently, and the blocking IDF scout has no loop to hand them to.

**Reply wins, ACK stays the fallback, driver-side tap** (chosen): the scan sends a tagged probe, a coordinator that understands it answers with its channel, and the driver intercepts that answer below the application; a coordinator that does not answer is still found by the ACK rule of ADR 009.

## Decision

### Wire format

Two system tags from the `0xF0..=0xFF` range of ADR 010, defined as `ProbeFrame` and `ProbeReply` in `juggler::espnow` (`no_std`, no alloc, `encode` and `parse`, wrong tag or length rejected):

| Frame | Bytes | Direction | Meaning |
|:------|:------|:----------|:--------|
| Probe | `[0xF3, seq, tx_channel]` | peer to coordinator, unicast | `seq` is a per-scan sequence number, `tx_channel` the channel the probe was sent on |
| Reply | `[0xF4, seq, tx_channel, home_channel]` | coordinator to peer, unicast | `seq` and `tx_channel` echo the probe, `home_channel` is the coordinator's current primary channel (`EspNowLink::current_channel()`) |

A reply is matched against the pending probe by `seq` and `tx_channel`; a stale or foreign reply, or a `home_channel` outside `1..=14`, is ignored.
The reply carries no RSSI, because the ESP-IDF receive callback of `esp-idf-svc 0.53` exposes none.

### Scan rule: reply wins, ACK is the fallback

1. The scan sends the `0xF3` probe on each channel of `ScanConfig::channels`; `ScanConfig::probe_data` is deprecated and ignored, `ScanConfig::for_peer()` builds the config.
2. After the MAC-layer ACK the scan machine issues `ScanAction::AwaitReply { until }` with `until` = ACK time + `ScanConfig::reply_timeout` (default 50 ms); the driver feeds `ScanInput::Reply(ProbeReply)` or `Completed` on timeout.
3. A matching reply ends the scan with `found = home_channel`; the coordinator has stated its channel, so no further probe is sent.
4. An ACK without a reply nominates the first such channel as the candidate and the sweep continues; a later reply still wins over the candidate.
5. When the channel list or the burst budget is spent and no reply arrived, the scan returns to the candidate and settles it with the confirmation rule of ADR 009 (`probe_confirmations`, `confirmation_gap`).
6. No ACK anywhere is `NotFound`, as before.

`ScanMachine` stays pure and clock-injected; `max_polls` grows by one poll per channel (the reply wait), plus a fixed candidate phase of `3 + 3 * probe_confirmations` polls and one home-channel switch, and the confirmations no longer count per channel.
Each scan continues the probe sequence of the previous one (`ScanMachine::with_first_seq`), so a reply left over from the last scan never matches a new probe.

### Per-tier reply tap

Replies to an outstanding probe never reach the application (approach A of the feature doc).
The tap is armed from a probe's send until the scan moves past its reply wait; a `0xF4` that arrives between probes (a late or MAC-retransmitted one) is handed to the application like any other frame.

- ESP-IDF: the receive callback offers every frame to the scan's reply tap first; a `0xF4` from the probed MAC while a reply wait is armed is delivered to the `espnow-tx` worker through a mailbox in its `Shared` state and the existing condvar, every other frame is queued for `receive` and `try_recv` unchanged.
- esp-hal: the tap sits in `receive()`; when no `receive()` is pending, the scan leases the receiver itself and sets every non-reply frame aside in a `HOLD_CAPACITY` (4) frame drop-oldest holding buffer that `receive()` and `try_recv` drain first, with the drops counted by `held_dropped()`.

### Coordinator side

`EspNowLink::current_channel()` exists on both tiers (`esp_wifi_get_channel` behind both).
The shared echo app in `examples-shared/espnow_echo_app.rs`, compiled into `idf_c3_espnow_coordinator` and `hal_c6_espnow_sta_coordinator`, answers a `0xF3` with `ProbeReply::answering(&probe, current_channel())` and echoes every other payload verbatim.

## Consequences

### Positive

- A peer scanning against an upgraded coordinator pins the coordinator's own channel; an adjacent-channel ACK cannot win over a reply.
- Old and third-party coordinators stay reachable through the ACK fallback; no fleet-wide flag day.
- The probe is now a recognisable system frame, so a coordinator can tell a scan from application traffic.
- Both tiers land the same names (`ProbeFrame`, `ProbeReply`, `current_channel()`, `reply_timeout`), per ADR 022.

### Negative

- A scan against a coordinator that does not reply is slower than before: it sweeps the whole list (one `reply_timeout` per ACKed channel) before it settles the candidate, instead of returning on the first ACK.
- The reply is a protocol change; a coordinator built before 0.7.0 is found by the fallback only, with bug 018's adjacent-channel risk intact.
- On esp-hal, application frames that arrive during a scan with no `receive()` pending are held in a four-frame buffer and may be dropped under load; the drop is counted, not silent.
- The wait for a reply adds up to `reply_timeout` to a scan that found the coordinator by ACK on the right channel but gets no answer.
- A reply is trusted without a confirmation round, and `home_channel` is the coordinator's live radio channel at that instant.
  A coordinator whose own station is mid-roam (an AP outage, the lore's roaming-peer case) can decode a probe during a scan dwell and name that transient channel; the scan pins it, where the ACK rule would have rejected the transient hit through the `confirmation_gap`.
  The roaming protection moves from the scan to the coordinator, and nothing on the coordinator side guards it yet; the peer recovers through `PeerLink`'s rescan after unacknowledged sends.

### Neutral

- Bug 018 stays open until the 0.7.0 bench reruns C3-I7 and C3-H6 with the shared echo app on the coordinator; the gates now require the found channel to equal the coordinator's.
- The rgb-puzzle coordinator may adopt the echo app or stay ACK-only; the fallback covers it either way.

## References

- [ADR 009 — ESP-NOW Channel Scanning](009-espnow-channel-scanning.md) (consequence superseded)
- [ADR 010 — ESP-NOW Command Frame Without Transport Metadata](010-espnow-command-frame-without-transport-metadata.md) (system-tag table extended)
- [ADR 012 — ESP-NOW Unassociated STA Channel Stability](012-espnow-unassociated-sta-channel-stability.md) (earlier false positives)
- [ADR 022 — Tier Parity, One Async API](022-tier-parity-one-async-api.md)
- [Bug 018 — An ESP-NOW scan at close range reports the coordinator on an adjacent channel](../bugs/archive/018-espnow-scan-finds-adjacent-channel-2026-11-10.md)
- [Feature: ESP-NOW Probe Reply Carries the Coordinator's Channel v1](../features/espnow-probe-reply-channel-v1.md)
- Runbook [release 0.7.0 hardware test](../runbooks/release-0.7.0-hardware-test.md), gates C3-I7 and C3-H6
- `crates/juggler/src/espnow/command.rs` (`ProbeFrame`, `ProbeReply`), `crates/juggler/src/espnow/scan.rs` (`ScanMachine`)
