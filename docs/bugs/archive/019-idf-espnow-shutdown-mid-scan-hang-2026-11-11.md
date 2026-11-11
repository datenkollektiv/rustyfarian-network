---
id: 019
title: ESP-IDF scout hung once after a shutdown 15 ms into a scan, no PASS and no 10 s FAIL line
captured-on: 2026-11-11
doc-version: 4
status: closed
kind: observation
---

# Bug 019: ESP-IDF scout hung once after a shutdown 15 ms into a scan, no PASS and no 10 s FAIL line

## Symptom
On 2026-11-11 01:05 the bench check `c3/espnow-shutdown` on `idf_c3_espnow_scout` (`ESPNOW_SCENARIO=shutdown`, gate C3-I9) hung after `espnow.shutdown()` 15 ms into a scan: the log ends with `ESP-NOW scan: probe on channel 1 failed: Worker(Shutdown)`, and within 120 s neither the `ESPNOW-GATE C3-I9` PASS line nor the scenario's own 10 s FAIL line (`queued requests did not end within 10 s of shutdown (hang)`) appeared, with no panic, no watchdog and no reboot.

## Root Cause (found 2026-11-11)
- Not a hang: a JTAG halt showed `main` in `park_forever` (`scenario_shutdown` had finished) and the worker thread in `pthread_exit`.
- The gate line was written but stayed in the USB Serial/JTAG TX FIFO: the console only hands a partial 64-byte packet to the host on a flush, and after the line the scout wrote nothing more; three JTAG resets each delivered the previous boot's gate line first.
- Second finding from the recovered lines: one run ended the scan with `Err(Shutdown)` instead of `Err(Scan(Aborted))` (the caller saw the shutdown flag before the worker published the abort), which the scenario counted as a problem although the juggler host test accepts both.
- Fix: `park_forever` calls `fsync(1)` and logs a heartbeat every 30 s; the scenario accepts `Shutdown` for the scan result; the C3-I9 gate text follows.

## Suspected Cause (before the root cause)
- The scenario runs inside `esp_idf_svc::hal::task::block_on`, which parks the task until a waker fires, wrapped in an embassy `with_timeout(10 s)`; a lost wake-up after the shutdown looks exactly like this, because the `espnow-tx` worker thread has exited and nothing wakes the task again.
- Candidates: the wait-list hand-off between `begin_shutdown`'s `wake_all` and the pending send and scan futures (a registration after the wake), or the embassy-time alarm not waking the parked task.
- Not a regression from the bug 016 fix: no crate changed since `e07193a`, the same scenario passed on 2026-11-10 00:48, and `hal_c3_espnow_peer` passed its mid-scan shutdown (C3-H8) at 01:08 the same night.

## Linked Artefact
- `bench/c3/logs/idf_c3_espnow_scout-20261111-010527.log` (the hang) and `bench/c3/logs/idf_c3_espnow_scout-20261110-004856.log` (the pass)
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_espnow_scout.rs` (`scenario_shutdown`)
- `crates/juggler/src/espnow/worker.rs` (`begin_shutdown`, `admit_scan`, `ScanTicket::poll`, `run_frame`)
- Runbook gate C3-I9 in `docs/runbooks/release-0.7.0-hardware-test.md`

## Reproduction Confidence
low

## Session Log
- 2026-11-11 — Observation captured via /bug dialog from the C3 bench night; next step: rerun `just bench c3 espnow-shutdown` a few times and, if it repeats, take a backtrace of the main task
- 2026-11-11 — Root cause found with openocd + gdb on the parked board and three JTAG resets under an independent serial reader; scout fixed (`fsync` + heartbeat, `Shutdown` accepted for the scan), rerun of `c3/espnow-shutdown` pending
- 2026-11-11 — Rerun of `c3/espnow-shutdown` passed with the fix (gate line delivered, `scan Some(Err(Shutdown))` accepted); ready to close once the hal half of the rerun is in
- 2026-11-11 — Closed on the user's word after the `c3/espnow-shutdown` rerun passed on both tiers (scout 01:31, hal peer 01:37); lore recorded under "Flashing & Serial" and "ESP-NOW"
