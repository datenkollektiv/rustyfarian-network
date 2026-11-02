---
id: 010
title: IDF MQTT client starts connecting before Wi-Fi has an IP
captured-on: 2026-11-01
doc-version: 4
status: closed
kind: defect
---

# Bug 010: IDF MQTT client starts connecting before Wi-Fi has an IP

## Symptom
On every boot with `ConnectMode::NonBlocking`, the first MQTT attempt fails with `esp-tls: connect() error: Host is unreachable`, `Error transport connect` and `[mqtt] error: ESP_FAIL`.

## Suspected Cause
`MqttBuilder::build` creates the esp-mqtt client, which starts its task and first attempt at once, whether or not the station has an IP.
Nothing in the IDF tier gates MQTT attempts on the network, so esp-mqtt also retries every `reconnect_timeout` (10 s) while the link is down or suspended (runbook R3 calls this "expected noise").
The esp-hal runner already waits for link-up and an IPv4 config before every attempt (`mqtt/runner.rs`, `is_link_up() && is_config_up()`), so this is also a tier-parity gap (ADR 022).

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/mqtt/mod.rs` (`MqttBuilder::build`)
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_async_mqtt.rs` (no `get_ip` wait before `build`)
- `docs/features/tier-parity-v1.md`, behaviour gap "IDF reconnect on link-up"

## Reproduction Confidence
high

## Severity
medium

## Environment
ESP32-C3, ESP-IDF v5.3.3, `esp-idf-svc 0.53`, branch `reconnect-rework`, example `idf_c3_async_mqtt` (tier-parity Step 2 spike).

## Expected Behaviour
A normal boot logs no MQTT error: the first attempt starts once the station has an IP, and attempts pause while the link is down or suspended, as on esp-hal.

## Actual Behaviour
- The first attempt runs about 30 ms after `build`, 3.5 s before the station gets its IP (`sta ip` at 4.2 s), and fails.
- The session only comes up at esp-mqtt's next retry, 10 s later, so the first connect is delayed by up to `reconnect_timeout`.
- The same happens after an outage: during a 2-minute router block esp-mqtt retried about every 15 s, each time with `Host is unreachable` and `[mqtt] error: ESP_FAIL`.
- On recovery its last retry ran at 801.5 s, after association but 0.8 s before the IP (802.3 s), so the session came back only at the next retry (816.7 s), 14 s after the IP.
- The other IDF examples hide this with `wifi.get_ip(10_000)` before `build`, which still fails the same way when the IP takes longer.

## Reproduction Steps
1. `just flash idf_c3_async_mqtt` on an ESP32-C3 with a reachable broker.
2. `just monitor`.
3. Observe `Error transport connect` and `[mqtt] error: ESP_FAIL` before `sta ip:`, and `mqtt up after` about 10 s later than the IP.

## Suggested Fix Area
- Library: start esp-mqtt only once the station has an IP, and stop or pause it while the link is down or suspended, driven by the Wi-Fi link state (IP events); a host-tested gate belongs in `juggler` (ADR 022 decision 10).
- Interim example fix: wait for the IP before `build` in the spike examples (proper order: Wi-Fi up with IP, then MQTT).

## Fix
Implemented as tier-parity Step 8b-2 (2026-11-01); the bench run below still gates closing this bug.

- `MqttBuilder::build(&wifi)` / `build_and_wait(&wifi, &mut led)` gate the session on the link (mandatory; the maintainer approved the break).
- The esp-mqtt client is created at the first readiness instead of in `build`: `EspMqttClient::new` starts the client task, which was the attempt that failed before the IP.
- `juggler::mqtt::NetworkGate` maps link readiness to exactly one `NetworkUp`/`NetworkDown` per transition; `LinkSupervisor::subscribe_ready` feeds it, so there is no poll and no second event source.
- esp-mqtt auto-reconnect is off; a new `mqtt-net` runner thread performs every `SessionAction` (`Connect` → create once, later `esp_mqtt_client_reconnect`; `Close` → `esp_mqtt_client_disconnect`; `Wait` → its own deadline) and paces retries with `MqttConfig::reconnect_timeout_ms` (default 10 s).
- Publishes made before the IP wait in `juggler::mqtt::Outbox` (four slots, oldest dropped and counted in `MqttHandle::dropped_publish`).
- `idf_c3_mqtt` no longer waits for the IP before `build`, so the boot order that reproduced the bug is the example's default path.

## Hardware Verification (2026-11-02, ESP32-C3, `idf_c3_async_mqtt`)

Boot — the order the bug was filed against:

```
7765  sta ip: 192.168.31.180
7785  [mqtt] session created; the gate opens when the link is ready
7805  [mqtt] client started
8065  [mqtt] connected
8085  [example] mqtt up after 7498 ms
```

The client is created 40 ms after the IP and connects 300 ms later; no `Error transport connect`, no `esp-tls` line and no MQTT line before `sta ip:`.
The bug measured the first attempt at `build + 30 ms`, 3.5 s early, with the session arriving on esp-mqtt's retry 10 s later.

Outage — a ~2 minute router block:

- The back-off ladder climbed 1000→30000 ms and never higher; every tick reported `would block`.
- No MQTT line of any kind appeared between the teardown error and the recovery: no retry, no `Host is unreachable`, no `[mqtt] error` after the first one.
  The bug had a retry every ~15 s, each with both errors.
- Recovery: `sta ip:` → `[mqtt] network up, connecting` → `[mqtt] connected` in 250 ms.
  The bug reconnected 14 s after the IP.
- Broker: `status` flipped `offline` when the will fired and back to `online` after recovery.

## Owner
Claude (tier-parity work)

## Links
- `docs/adr/022-tier-parity-one-async-api.md`
- `docs/features/tier-parity-v1-plan.md` (Step 2 spike, Phase 6 behaviour gaps)
- `docs/runbooks/wifi-reconnect-hardware-test.md` (R3 expected-noise note)

## Session Log
- 2026-11-01 — Captured as a defect via /bug dialog from the Step 2 spike run on a C3
- 2026-11-01 — Updated: outage evidence from the variant A run (retries during the outage, reconnect 14 s after the IP)
- 2026-11-01 — Shared rule implemented in `juggler::mqtt::SessionMachine` (tier-parity Step 6); the IDF fix lands when IDF adopts it (Step 8)
- 2026-11-01 — Fixed in code (Step 8b-2): link-gated client creation, `NetworkGate` + `subscribe_ready` feed, esp-mqtt auto-reconnect off with runner-paced attempts; status `fixed-in-code` until the bench run confirms the boot log has no transport error and the outage shows zero MQTT lines
- 2026-11-02 — Closed on hardware: `idf_c3_async_mqtt` on an ESP32-C3 created the client 40 ms after `sta ip:` and connected 300 ms later with no transport error, and a ~2 minute router block produced no MQTT line at all until the recovery 250 ms after the next IP.
