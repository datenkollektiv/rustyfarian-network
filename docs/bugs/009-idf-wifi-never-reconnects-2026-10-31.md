---
id: 009
title: IDF WiFiManager never reconnects after a link loss
captured-on: 2026-10-31
doc-version: 1
status: closed
kind: defect
---

# Bug 009: IDF WiFiManager never reconnects after a link loss

## Symptom
After a router-side Wi-Fi pause the ESP-IDF station logs one `WiFi disconnected — reason ...` line and stays down until reboot, in both connect modes.

## Suspected Cause
Confirmed: `WiFiManager::new` issues `esp_wifi_connect` exactly once, and the `StaDisconnected` subscription (non-blocking mode only) just logs the reason.
ESP-IDF never retries a connect on its own, so nothing re-issues the request.
The esp-hal tier did not have the defect, but only because every example carried its own `wifi_task` reconnect loop; the library offered none.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/wifi/mod.rs` (`WiFiManager::new`, the `StaDisconnected` subscription).
- Seven `crates/rustyfarian-esp-hal-network/examples/hal_*.rs` with a private `wifi_task` loop.
- Request U2f: `review-queue/rustyfarian-network-u2f-reconnect-and-suspend-resume.md`, finding 1 at `6cd6710`.
- Feature doc: `docs/features/wifi-reconnect-and-suspend-v1.md`.

## Reproduction Confidence
high

## Severity
high

## Environment
- Reported by `rustyfarian-knob` on the CrowPanel 1.28" ESP32-S3, ESP-IDF v5.3.3, `esp-idf-svc` 0.53.0, `rustyfarian-esp-idf-network` at `6cd6710`.
- Affects every IDF consumer since the first release; MQTT auto-reconnect (bug 005) cannot help while the station is down.

## Expected Behaviour
The station reconnects on its own with a bounded back-off, `is_connected()` is `false` for the whole outage, and esp-mqtt reconnects once the link is back.

## Actual Behaviour
One disconnect log line, then silence; `is_connected()` stays `false` and the device must be power-cycled.

## Reproduction Steps
1. Flash `idf_esp32s3_mqtt_lwt` (or any IDF example) and wait for the MQTT connect.
2. Pause the device on the router for about 3 minutes.
3. Un-pause and watch `just monitor`: no `WiFi connect requested` line ever appears.

## Suggested Fix Area
Applied: `juggler::wifi::LinkMachine` owns the policy (1 s back-off doubling to 30 s, reset on connect); the IDF manager runs it on a worker thread fed by the event subscription, the esp-hal tier in `WifiLink::run`, so the examples no longer carry a loop.
Runbook: `docs/runbooks/wifi-reconnect-hardware-test.md`.

## Owner
Maintainer; hardware re-check by the `rustyfarian-knob` maintainer (CrowPanel S3).

## Links
- `docs/features/wifi-reconnect-and-suspend-v1.md`
- Bug 005 (MQTT never auto-reconnected) for the matching MQTT-side defect.

## Session Log
- 2026-10-31 — Captured as a defect from request U2f; fixed in the same change (PR 1 of U2f), hardware re-check by the knob maintainer pending
- 2026-11-02 — Suspend/resume gate (R3) run on the CrowPanel S3 for both tiers, 3 cycles each, pass; it exposed that `suspend` reported readiness only after stopping the radio, so the MQTT session scheduled a retry it then had to park — fixed by reporting readiness before the stop; the scenario this bug reports (R1, router pause) still needs a device pause on the router.
