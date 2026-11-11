---
id: 017
title: The ESP-IDF scout's full ESP-NOW scan cannot set channel 12
captured-on: 2026-11-10
doc-version: 3
status: closed
kind: observation
---

# Bug 017: The ESP-IDF scout's full ESP-NOW scan cannot set channel 12

## Symptom
On the C3 Super Mini, every not-found scan of `idf_c3_espnow_scout` ended with `peer not found after probing 11 of 13 configured channels (first failure: set channel 12)`, so a scan with the coordinator off reports a local failure instead of a clean not-found.

## Suspected Cause
- The radio's regulatory domain allows only channels 1 to 11, while the scout scans `juggler::espnow::DEFAULT_SCAN_CHANNELS` (1 to 13, EU/JP).
- The rustdoc of `DEFAULT_SCAN_CHANNELS` already warns that US/CA domains stop at 11 and suggests `ScanConfig::with_channels`.
- What sets the 1-to-11 domain on this board is unknown: a country setting in `sdkconfig.defaults`, the ESP-IDF default, or the STA adopting the router's country (802.11d).
- Open question: should a channel the domain forbids count as skipped rather than as the scan's first failure?
- The C3-I7 rerun logged `failed to set channel 12: ESP_ERR_INVALID_ARG (error code 258)`, so ESP-IDF itself rejects the channel, which fits a country limited to channels 1 to 11.

## Linked Artefact
- `crates/juggler/src/espnow/mod.rs` (`DEFAULT_SCAN_CHANNELS`)
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_espnow_scout.rs` (`ScanConfig::new(b"scout-probe")`)
- Runbook `docs/runbooks/release-0.7.0-hardware-test.md` gate C3-I7 (not-found scans)
- Log `bench/c3/logs/idf_c3_espnow_scout-*` of 2026-11-10

## Reproduction Confidence
high

## Session Log
- 2026-11-10 — Observation captured via /bug dialog from the first C3-I7 bench run
- 2026-11-10 — Updated: ESP-IDF answers `ESP_ERR_INVALID_ARG` for channel 12
- 2026-11-11 — Closed: the country is now applied on both tiers (`juggler::wifi::Country`, default `Country::WORLD` = `01`, channels 1-11) and the ESP-NOW scan skips channels the country forbids, counting them in `NotFound::not_allowed` (ADR 026, [feature doc](../../features/wifi-country-v1.md)); regression test `world_country_with_the_default_list_probes_only_1_to_11` in `crates/juggler/src/espnow/scan.rs` (the default list probes 1 to 11 and ends `NotFound` with two not allowed); the bench rerun of C3-I7 is still owed
