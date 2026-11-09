---
id: 018
title: An ESP-NOW scan at close range reports the coordinator on an adjacent channel
captured-on: 2026-11-10
doc-version: 1
status: open-observation
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
