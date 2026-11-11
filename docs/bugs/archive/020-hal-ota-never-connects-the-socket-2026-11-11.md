---
id: 020
title: esp-hal OTA never opened the TCP connection, every fetch failed as ServerUnreachable
captured-on: 2026-11-11
doc-version: 2
status: closed
kind: defect
---

# Bug 020: esp-hal OTA never opened the TCP connection, every fetch failed as ServerUnreachable

## Symptom
`hal_esp32s3_ota` (and `hal_c3_ota`) logged `[ota] fetching http://…` and, in the same second, `[ota] update failed, boot slot unchanged: Update server unreachable`, while the image server never saw a request.

## Suspected Cause
Nothing in the esp-hal OTA path called `TcpSocket::connect`.
The example hands `EspHalOtaManager::fetch_and_apply` a fresh `TcpSocket::new(...)`, and the manager's connect step parsed the URL and wrote the `GET` straight onto the unconnected socket.
embassy-net rejects the write at once, and `fetch_get` maps every write error to `OtaError::ServerUnreachable` without a log line, so the failure looked like a network drop.

## Linked Artefact
- `crates/rustyfarian-esp-hal-network/src/ota/manager.rs` (`EspHalOtaManager::connect`)
- `crates/rustyfarian-esp-hal-network/src/ota/http.rs` (`fetch_get`)
- `crates/rustyfarian-esp-hal-network/examples/common/ota_app.rs`
- Runbook gates C3-H5 and S3-H3 in `docs/runbooks/release-0.7.0-hardware-test.md`

## Reproduction Confidence
high

## Severity
high

## Environment
CrowPanel ESP32-S3, `prepare-release-bench` at `37488a7`, `just flash-ota-baseline hal_esp32s3_ota 1.0.0 2.0.0 192.168.40.146`, image server `just ota-serve` on the Mac, router rule for TCP 8000 in place.
The 2026-11-02 C3 harness run (`hal_c3_ota_check`, `ServerUnreachable`) showed the same symptom and was attributed to the router at the time.

## Expected Behaviour
Boot 1 opens a TCP connection to the configured IPv4 host and port, downloads the image, verifies it, activates the other slot and resets.

## Actual Behaviour
Boot 1 logs the fetch and the `Update server unreachable` error in the same second on every reset; the server log stays empty; the boot slot never changes.

## Reproduction Steps
1. `just ota-image 2.0.0 hal_esp32s3_ota`, `just ota-serve` on a host the board can reach.
2. `just flash-ota-baseline hal_esp32s3_ota 1.0.0 2.0.0 <host>` and `just monitor`.
3. Read `[ota] update failed, boot slot unchanged: Update server unreachable` right after `[ota] fetching`, with no request in the server log.

## Suggested Fix Area
`EspHalOtaManager::connect`: parse the host as an IPv4 literal (this tier resolves no names), `socket.connect(IpEndpoint)` under the connect budget, log the embassy-net `ConnectError`, map a connect timeout to `DownloadTimeout` and a refused or failed connect to `ServerUnreachable`, and take the connect time off the header-phase budget.

## Owner
fluffi

## Links
- Bug 004 (erase-once write path) is the next thing the same round trip checks on real flash.
- `docs/features/ladder-closure-v1.md` (hal `fetch_and_apply` flow, gates C3-H5 and S3-H3).

## Session Log
- 2026-11-11 — Captured during the S3-H3 bench run: three resets after the router rule was confirmed open all failed in the same second; the source has no `connect` call (`git log -S'.connect('` on the ota module finds none, so this is not a regression of the ladder retrofit).
- 2026-11-11 — Fix applied in `manager.rs` (connect inside the `DownloadAction::Connect` step); `just check-ota-hal-embassy` passes.
- 2026-11-11 — With the connect in place the S3 reached the server and failed on `OTA HTTP error: BadStatusLine`: `parse_status_line` accepted only `HTTP/1.1 `, while Python's `http.server` behind `just ota-serve` answers `HTTP/1.0 200 OK`; the parser now takes both (host test `status_line_http10_is_accepted`). Hardware confirmation pending on the S3-H3 round trip.
- 2026-11-11 — Confirmed on hardware: with both fixes the S3-H3 round trip passed end to end (2.0.0 download in 6 s, unhealthy image rolled back by the bootloader, reset check, healthy 3.0.0); ready to close.
- 2026-11-11 — Closed on the user's instruction after the fix passed on the bench: `just bench-ota s3 ota` 3/3 (02:12), `just bench-ota s3 ota-rollback` 5/5 (02:13) and `s3 ota` again with 3.0.0 3/3 (02:15) on the CrowPanel S3; lore recorded under "OTA MVP — Three-Crate Triad"
