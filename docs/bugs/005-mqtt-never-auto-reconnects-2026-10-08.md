---
id: 005
title: MQTT client never auto-reconnects with the default configuration
captured-on: 2026-10-08
doc-version: 1
status: open-defect
kind: defect
---

# Bug 005: MQTT client never auto-reconnects with the default configuration

## Symptom
After a broker outage the device logs one `[mqtt] disconnected` and stays offline until it reboots, with no esp-mqtt connect attempt or error in between.

## Suspected Cause
Confirmed: `MqttManager::new` and `MqttBuilder` set `reconnect_timeout: config.reconnect_timeout_ms.map(Duration::from_millis)`, which is `None` unless the app calls `with_reconnect_timeout()`.
`esp-idf-svc` (0.52.1 and 0.53.0, `src/mqtt/client.rs` lines 214–218) maps `reconnect_timeout: None` to `disable_auto_reconnect = true`.
Its own default is `Some(0)`, which esp-mqtt turns into `MQTT_RECON_DEFAULT_MS` (10 s); our mapping overwrote that default.
The first connect still works because it is part of `esp_mqtt_client_start`, so the bug only shows after a mid-session disconnect.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` (both `MqttClientConfiguration` sites).
- Introduced by `13e0e2d` (2026-03-26, "Add configurable reconnect interval for MQTT client").
- Shipped in `v0.4.0` and `v0.5.0`.
- Evidence: `tmp/ota-campaign/evidence-mqtt-stall-full.log` (local, not committed; disconnect at line 4445).

## Reproduction Confidence
high

## Severity
high

## Environment
- ESP32-C3 rev 0.4, ESP-IDF v5.3.3, `esp-idf-svc` 0.53.0, `rustyfarian-esp-idf-network` at `main` (2026-10-08).
- Reproduced with `idf_c3_ota_runtime` and with the plain `idf_c3_mqtt` example, so the OTA runtime is not involved.
- Affects every consumer that does not call `with_reconnect_timeout()`, including rgb-clock in the field.

## Expected Behaviour
esp-mqtt retries every 10 s while the broker is down and reconnects, re-subscribes and resumes publishing once it is back.

## Actual Behaviour
After `transport_read(): EOF` and `[mqtt] disconnected` the client never attempts to connect again.
The board stays reachable on Wi-Fi (ping answered), and a broker that is up again is never contacted (watched for 9 minutes).
A reboot restores the connection.

## Reproduction Steps
1. Run `just mosquitto up` and build the example against it (`MQTT_HOST` / `MQTT_PORT` as printed).
2. Flash `idf_c3_mqtt` (or `idf_c3_ota_runtime`) and wait for `[mqtt] connected`.
3. Run `just mosquitto down`, wait 20 s, then `just mosquitto up`.
4. Watch `just monitor`: one `[mqtt] disconnected`, then silence.

## Suggested Fix Area
Pass `Some(config.reconnect_timeout_ms.map_or(Duration::ZERO, Duration::from_millis))` at both sites; `ZERO` keeps auto-reconnect on with the esp-mqtt default.
Applied in the working tree and verified on hardware: the device logs `Error transport connect` about every 15 s during the outage, reconnects within 10 s after the broker returns, and its subscription and heartbeats resume.
A consumer that wants no reconnect has no API for it today; add one only if someone asks.

## Owner
Florian Waibel

## Links
- Found in `docs/runbooks/ota-hardware-test.md` P17 (broker scaled down), 2026-10-08.
- Lore: `docs/project-lore.md` "MQTT Event Loop".
- Ships with the fix in 0.6.0, after the end-to-end OTA runbook pass.

## Session Log
- 2026-10-08 — Captured as a defect via /bug with root cause and a hardware-verified fix; stays open until the fix is committed and released in 0.6.0
