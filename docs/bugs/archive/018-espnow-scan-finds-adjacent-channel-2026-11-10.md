---
id: 018
title: An ESP-NOW scan at close range reports the coordinator on an adjacent channel
captured-on: 2026-11-10
doc-version: 4
status: closed
kind: observation
---

# Bug 018: An ESP-NOW scan at close range reports the coordinator on an adjacent channel

## Symptom
With the C3 scout and the C6 coordinator centimetres apart, `scan_for_peer` found the coordinator (on router channel 11) on channel 10, then on 11, so C3-I7 logged `FAIL: scan after cancel found the coordinator on channel 11 (earlier Some(10))`.

## Suspected Cause
- The scan treats the first MAC-layer ACK as the peer's channel and walks the channels upwards from 1.
- 2.4 GHz channels are 5 MHz apart while the signal is 20 MHz wide, so at close range the coordinator decodes and ACKs a probe sent on channel 10.
- Which neighbour still gets through varies from scan to scan, so the reported channel flaps between 10 and 11.
- `ScanConfig::probe_confirmations` does not help: the adjacent-channel ACK repeats as reliably as the real one.
- The TX-power run (C3-I10) showed the same effect once: scan 3 reported channel 10.
- A possible fix: after a hit on channel N, probe N+1 as well and prefer the higher channel with a confirmed ACK; design question, not for 0.7.0.

## Linked Artefact
- `crates/juggler/src/espnow/scan.rs` (`ScanMachine`, first-ACK rule)
- Runbook `docs/runbooks/release-0.7.0-hardware-test.md` gates C3-I7 and C3-I10
- Logs `bench/c3/logs/idf_c3_espnow_scout-*` of 2026-11-10 (C3 Super Mini scout, hal C6 `hal_c6_espnow_sta_coordinator` on channel 11)

## Reproduction Confidence
medium

## Session Log
- 2026-11-10 — Observation captured via /bug dialog from the C3-I7 rerun; first check is a rerun with the boards metres apart
- 2026-11-11 — Related observation: with the C3 scout and the hal C6 coordinator close on the bench the scout found nothing on any channel for about six minutes (the coordinator's `rx` counter stood still, then read 0 after a reset) and found it on channel 11 within seconds of the boards being moved to opposite edges of the table; a replug of both boards happened in the same minute, so distance and replug are not separated
- 2026-11-11 — The `espnow-cancel` run at 01:45 found the coordinator on channel 10 (C6 on 11) with the boards at opposite edges of the table, so the adjacent-channel find is not only a close-range effect
- 2026-11-11 — Fix landed (ADR 025, feature [espnow-probe-reply-channel-v1](../features/espnow-probe-reply-channel-v1.md)): the scan sends a `0xF3` probe and trusts the coordinator's `0xF4` reply carrying its channel over the MAC-layer ACK, which stays the fallback; the shared echo app answers on both coordinators; closes after the bench rerun of C3-I7 and C3-H6 with the found channel equal to the coordinator's
- 2026-11-11 — Closed: with the probe reply (ADR 025) both tiers found the coordinator on channel 11 in every scan after a cancel (`just bench c3 espnow-cancel --unattended` 2/2 twice per tier), and the txpower, flood and shutdown reruns showed no adjacent-channel find; the only wandering finds left are the boot-time roaming case while a freshly powered coordinator still scans for its access point, recorded in ADR 025
