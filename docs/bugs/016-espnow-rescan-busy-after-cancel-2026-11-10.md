---
id: 016
title: An ESP-NOW scan started right after a cancelled scan is refused as Busy
captured-on: 2026-11-10
doc-version: 3
status: open-defect
kind: defect
---

# Bug 016: An ESP-NOW scan started right after a cancelled scan is refused as Busy

## Symptom
In runbook gate C3-I7 (`ESPNOW_SCENARIO=cancel` on `idf_c3_espnow_scout`), the full scan that follows a dropped `scan_for_peer` future failed with `ESP-NOW radio is busy` five times in about 30 s, so `ESPNOW-GATE C3-I7 FAIL: scan after cancel failed` was logged.

## Suspected Cause
- `juggler::espnow::worker` keeps the scan slot after the future is dropped until the worker has waited out the probe already on the air (fail-closed), and `admit_scan` answers `WorkerError::Busy` meanwhile.
- The unit test `a_second_scan_is_busy_and_a_caller_managed_radio_cannot_scan` pins that "a running scan or its unread result holds the slot", so this may be the intended contract rather than a defect.
- The gate expects the opposite: "the cancelled scan ends without a send, and the next scan finds the coordinator".
- The esp-hal gate C3-H6 reads its state 50 ms after the drop "so a deferred teardown can finish", which hints that the hal tier hits the same window.
- Decision needed for both tiers (ADR 022): a new scan waits for the teardown instead of failing, or the contract documents `Busy` after a cancel and the scenario retries.

## Linked Artefact
- `crates/juggler/src/espnow/worker.rs` (`Worker::scan_for_peer`, `admit_scan`)
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_espnow_scout.rs` (`scenario_cancel`, which rescans at once)
- Runbook `docs/runbooks/release-0.7.0-hardware-test.md` gates C3-I7 and C3-H6; check `bench/c3/espnow-cancel.toml`
- Log `bench/c3/logs/idf_c3_espnow_scout-*` of 2026-11-10 (C3 scout, hal C6 `hal_c6_espnow_sta_coordinator` as coordinator)

## Reproduction Confidence
high

## Severity
high

## Environment
- ESP32-C3 Super Mini, ESP-IDF tier, `idf_c3_espnow_scout` built with `ESPNOW_SCENARIO=cancel`, branch `prepare-release-bench`.
- Coordinator: hal C6 `hal_c6_espnow_sta_coordinator` on router channel 11.
- The esp-hal tier shares `juggler::espnow::ScanGate`, so it is expected to behave the same.

## Expected Behaviour
A `scan_for_peer` started right after a dropped one waits until the dropped scan's restore has run, is then admitted, and finds the peer.

## Actual Behaviour
`ScanGate::admit` sees the slot still held by the unwinding scan and refuses with `GateRefusal::Busy`, which reaches the caller as `ESP-NOW radio is busy`.

## Reproduction Steps
1. Run a coordinator on another board.
2. Build and flash `idf_c3_espnow_scout` with `ESPNOW_SCENARIO=cancel` and `COORDINATOR_MAC` set.
3. Run `just bench c3 espnow-cancel --port <c3 port>` (or watch the serial log).
4. Within seconds the scout logs `ESPNOW-GATE C3-I7 FAIL: scan after cancel failed: ESP-NOW radio is busy`.

## Decision
- Approved 2026-11-10 by the maintainer: a scan that meets a cancelled scan still unwinding waits for its restore instead of failing.
- `ScanGate::admit` gets a refusal for that case (for example `GateRefusal::Unwinding`), separate from `Busy`.
- Both tiers wait on it and admit again; the wait lasts no longer than the restore (one probe on the air plus the teardown) and ends on shutdown or driver drop like every other wait.
- `Busy` stays for a running scan of another caller and for an untaken result.
- Rejected: documenting `Busy` after a cancel and retrying in every caller (leaves the gate contract unmet and the retry to consumers).

## Suggested Fix Area
- `crates/juggler/src/espnow/scan_job.rs` (`ScanGate::admit`, `GateRefusal`)
- `crates/juggler/src/espnow/worker.rs` (`Worker::scan_for_peer`, used by the ESP-IDF tier)
- `crates/rustyfarian-esp-hal-network/src/espnow/driver.rs` (`EspHalEspNow::scan_for_peer`)

## Owner
Claude session on `prepare-release-bench`

## Links
- Runbook gates C3-I7 and C3-H6 in `docs/runbooks/release-0.7.0-hardware-test.md`
- ADR 022 (tier parity)

## Session Log
- 2026-11-10 — Observation captured via /bug dialog from the first C3-I7 bench run; blocks C3-I7
- 2026-11-10 — Promoted to defect with the approved decision: wait for an unwinding cancelled scan instead of `Busy`
- 2026-11-10 — Fixed on both tiers (`GateRefusal::Unwinding`, wait and re-admit; host tests plus two mutation checks); the C3-I7 rerun showed no `radio is busy` across all cancel cycles; close after the gate passes
