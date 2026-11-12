# Runbook: Wi-Fi Reconnect Hardware Test (both tiers)

Run this after any change to `juggler::wifi::LinkMachine`, the IDF `WiFiManager` reconnect worker or the esp-hal `WiFiManager` supervisor, and for the outage protocol.
It covers a router-side outage (R1, R2) and the radio suspend/resume cycle (R3) on the two S3 LWT examples, one result per tier.

Both examples compile `examples-shared/lwt_app.rs` (ADR 022, one application on two tiers), so the app's own lines start with `[app]`, both tiers print the same `[wifi]` and `[mqtt]` library lines, and the tier shows only in the `[app] start <example> (esp-hal|esp-idf)` boot line.

**Reference device:** CrowPanel 1.28" HMI ESP32-S3 (`hal_esp32s3_mqtt_lwt` and `idf_esp32s3_mqtt_lwt`, console on the USB-Serial-JTAG port).

## Setup

- The setup of the [esp-hal MQTT runbook](esp-hal-mqtt-hardware-test.md); the IDF twin reads the same keys.
- Default client id and prefix differ per tier (`hal-s3-mqtt` / `rustyfarian/hal-mqtt` and `idf-s3-mqtt` / `rustyfarian/idf-mqtt`), so both share one broker; `just mqtt-watch` watches the hal prefix, `just mqtt-watch idf` the IDF one.
- A router that can pause one device (per-device block, parental control or MAC filter) for about 3 minutes.

## Scenarios

- R1, 3-minute router pause: `s3/mqtt-scenarios --attended` (steps T5b/R1), both tiers.
- R3, suspend/resume demo (20 s at every full minute, built with `MQTT_SUSPEND_DEMO=1`): `s3/suspend`, both tiers.

Facts the checks do not encode:

- R1, IDF: the reconnect follows the `sta ip:` line within about a second, with no `Error transport connect` and no `[mqtt] reconnecting in` line, because the session is gated on the link; ticks `queued` between the pause and `[mqtt] disconnected` may arrive after the reconnect (esp-mqtt outbox, QoS 1), a `would block` tick never does.
- R1: the app samples the MQTT state every 5 s and may miss a short `Connecting` phase; check each retry by its per-attempt lines (`[mqtt] connecting to`, `[mqtt] reconnecting in`, plus `[mqtt] tcp connect failed` on hal and `Transport connected` on IDF).
- R1: record the time from the un-pause to the first `queued` tick; expect under about 75 s on both tiers (up to two 30 s back-off periods when the router has not re-admitted the device yet, plus the MQTT connect; on hal a timed-out first TCP connect adds the 10 s MQTT retry).
- R3, IDF: the suspend logs `[wifi] disconnected — reason 8` and one `[mqtt] error` plus `[mqtt] disconnected` as the transport's dying gasp; a tick `queued` between the suspend and `[mqtt] disconnected` may still reach the broker after the resume.
- R3: on resume IDF reconnects at esp-mqtt's next retry (up to about 10 s), hal within seconds; the minute `[app] heap free` line must stay stable across cycles.

### R2 — wrong credentials (optional)

No check.

1. Build with a wrong `WIFI_PASS`, flash, watch the console for 3 minutes.
2. Pass: attempts keep coming with the delay capped at `30000 ms`; no panic, no reboot, heap stable.

## History

Earlier results; the 0.7.0 results are in the [release runbook](release-0.7.0-hardware-test.md).

- `c5146ca`: R1 pass on both tiers (CrowPanel S3): back-off 1 to 30 s, 0 ticks `queued` while down on hal; on IDF ticks `would block` and MQTT back 270 ms after `sta ip:`.
  Pause to `status offline` about 45 s (the keep-alive will, not timestamped); un-pause to the first `queued` tick 33.5 s on IDF, spent waiting out one 30 s back-off, then 0.3 s from `sta ip:` to `[mqtt] connected`.
- R2 pass on an ESP32-C3: hal with the hint on `FourWayHandshakeTimeout` and the ladder capped at 30000 ms (`tmp/c3-r2-hal3.log`); IDF with the hint on `reason 15`, none on `205`, capped at 30000 ms (`tmp/c3-r2-idf-async4.log`).
- R3 pass on both tiers (CrowPanel S3): 3 cycles each, broker `offline` then `online`; hal `link up #2/#3`, no retry line, `demo: resume` to `[mqtt] connected` sub-second; IDF gate parks before the drop, 3.6 s from `demo: resume` to `[mqtt] connected` (3.5 s association, 80 ms MQTT connect).
  On IDF the suspend still logs one `[mqtt] error: ESP_FAIL` when the radio stops under esp-mqtt; suppressing it would need `suspend` to wait for the client to close, which no handshake offers today.
- 2026-11-03, `f81d83e` plus the shared app:
  - R1 hal: `[wifi] disconnected — AuthenticationLeave`, back-off 1000 to 30000 ms, `wifi link Connected -> Backoff`, `mqtt state Connected -> Disconnected`, ticks `would block`, broker `offline`; one attempt after the unblock still failed `AuthenticationExpired` (the router had not applied the removal yet), the next reached `link up #2`, `connect #2 free heap: 25880 B` and `tick #38 queued`.
  - R1 IDF: `reason 2 (AUTH_EXPIRE)` then `reason 205 (CONNECTION_FAIL)`, ladder up to 30000 ms (attempt 8), broker `offline`; `sta ip: 192.168.31.179` about 21 s after the unblock, `[mqtt] network up, connecting` 110 ms later, `connect #2 free heap: 205724 B`, `tick #95 queued`.
  - R3 hal: three cycles, `link up` and `connect` #2 to #4, heap flat at 25 928 B, no retry line while suspended.
  - R3 IDF: three cycles, `connect` #2 to #4, heap 206 764 → 206 568 → 206 392 B, one `[mqtt] error: ESP_FAIL` and one `mqtt_client: Writing failed: errno=119` per suspend, no retry line while suspended.
  - T6 IDF: 10 min, 123 ticks, no gap, no disconnect, but heap drift 2148 B (206 836 → 204 688 B), the last sample alone 1152 B (later traced to `PendingAcks::early`); T6 hal: see the [esp-hal MQTT runbook](esp-hal-mqtt-hardware-test.md) history.
  - Both tiers: the `[app]` template sets are identical under `sed -E 's/[0-9]+/N/g'`, except the per-tier topic prefix in the `rx` line and which intermediate state the 5 s sample catches; link states seen `{Connected, Connecting, Backoff}` on both, MQTT states `{Connected, Disconnected}` on both (IDF's `Connecting` completed inside one sample).
