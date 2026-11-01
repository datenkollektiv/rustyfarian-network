---
id: 012
title: First MQTT session of variant A took 15 s although MQTT starts after the IP
captured-on: 2026-11-01
doc-version: 2
status: closed
kind: observation
---

# Bug 012: First MQTT session of variant A took 15 s although MQTT starts after the IP

## Symptom
After the bug 010 interim fix (MQTT built only once the station has an IP), `idf_c3_async_mqtt` still logged `mqtt up after 15251 ms` on the C3, about 11 s after the IP.

## Suspected Cause
- The 11 s gap matches one failed attempt plus esp-mqtt's 10 s `reconnect_timeout`, so the first attempt after the IP probably still failed.
- Candidates: the broker host is not reachable yet right after DHCP (ARP, gateway), or `get_ip` reports the IP before lwIP can route.
- The boot lines between `wifi up after` and `mqtt up after` were not captured, so the cause is unconfirmed.

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/examples/idf_c3_async_mqtt.rs` (IP wait before `MqttBuilder::build`)
- Bug 010 (the library-side gate would face the same question)

## Reproduction Confidence
medium

## Session Log
- 2026-11-01 — Observation captured via /bug dialog from the Step 2 variant A run; next run: capture the boot log from `wifi up after` to `mqtt up after`
- 2026-11-01 — Closed: the board still ran the build from before the bug 010 interim fix; the fixed build logged `mqtt up after 4752 ms`, 70 ms after the IP
