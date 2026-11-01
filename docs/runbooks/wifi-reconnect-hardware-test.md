# Runbook: Wi-Fi Reconnect Hardware Test (both tiers)

Run this after any change to `juggler::wifi::LinkMachine`, the IDF `WiFiManager` reconnect worker or the esp-hal `WifiLink`, and for the `rustyfarian-knob` M8 outage protocol.
It exercises a router-side outage (R1, R2) and the radio suspend/resume cycle (R3) on the two S3 LWT examples and records one result per tier.

**Reference device:** CrowPanel 1.28" HMI ESP32-S3 (`hal_esp32s3_mqtt_lwt` and `idf_esp32s3_mqtt_lwt`, console on the USB-Serial-JTAG port).

## Prerequisites

- Everything under "Prerequisites" and "Environment variables" in `docs/runbooks/esp-hal-mqtt-hardware-test.md`; the IDF twin reads the same keys.
- Default client id and prefix differ per tier (`hal-s3-mqtt` / `rustyfarian/hal-mqtt` and `idf-s3-mqtt` / `rustyfarian/idf-mqtt`), so both can share one broker.
- A router that can pause one device (per-device Wi-Fi block, parental control or MAC filter) for about 3 minutes.

## Build, flash, monitor

```bash
just mosquitto up
just mqtt-watch
just flash hal_esp32s3_mqtt_lwt
just monitor
```

Repeat with `idf_esp32s3_mqtt_lwt` for the ESP-IDF tier.
Rebuild after every `.env` change; the values are compiled in.

## Scenarios

Serial lines quoted below are prefixes; numbers vary.

### R1 — 3-minute router pause

1. Wait for `status online` on `just mqtt-watch` and at least three `tick #<n> queued` lines, then pause the device on the router.
2. Pass: within seconds the console shows the link loss (`[wifi] disconnected —` on hal, `WiFi disconnected — reason` on IDF) and then `reconnecting in 1000 ms`, `2000 ms`, `4000 ms`, up to `30000 ms`, never more.
3. Pass: every tick while paused reports `would block`; no `tick #<n> failed:`, no panic, no reboot.
4. Pass: `just mqtt-watch` shows `<prefix>/status offline` about 45 s after the pause (1.5 × keep-alive); the broker fires the will because the client sent no DISCONNECT.
5. Un-pause after about 3 minutes; pass: the link comes back on the next attempt (`[wifi] link up #2` on hal, `WiFi connect requested` then an IP log on IDF) without a reboot.
6. Pass: MQTT reconnects on its own (`[mqtt] connected` on both tiers), `status online` is retained again and ticks are `queued` again.
   On IDF no `Error transport connect` or `[mqtt] error` line may appear while the link is paused (since Step 8b-2 the session is gated on the link and makes no attempt); the reconnect follows the `sta ip:` line within about a second.
   On IDF, at most the ticks `queued` between the pause and the `[mqtt] disconnected` line may arrive after the reconnect (esp-mqtt outbox, QoS 1); any tick logged `would block` must never reach the broker.
   Pass on both tiers: `[example] mqtt state Connected -> Disconnected` after the pause and `-> Connected` again after the un-pause.
   The example samples the state every 5 s, so it may miss a short `Connecting` phase; check each retry by its per-attempt log instead (`[mqtt] tcp connect failed` on hal, `[mqtt] reconnecting in` plus `Transport connected` on IDF).
7. Record the time from un-pause to the first `queued` tick; expect under 45 s on hal (back-off at most 30 s plus the MQTT connect) and under 5 s on IDF (at most one Wi-Fi back-off plus the connect, with no MQTT retry wait).

### R2 — wrong credentials (optional)

1. Build with a wrong `WIFI_PASS`, flash, watch the console for 3 minutes.
2. Pass: attempts keep coming with the delay capped at `30000 ms`; no panic, no reboot, heap stable.

### R3 — suspend/resume demo (20 s every 60 s)

Set `MQTT_SUSPEND_DEMO=1` in `.env`, rebuild and flash; the example suspends at every full minute of uptime and resumes 20 s later.
Watch at least three cycles per tier.

1. Pass on suspend, hal: `[example] demo: suspend for 20 s`, `[wifi] suspend requested`, `[wifi] suspended —`, `[example] demo: is_suspended = true`, `[example] wifi link Connected -> Suspended`, then `[mqtt] disconnected (`…`), waiting for network` within about 0.5 s.
2. Pass on suspend, IDF: `[example] demo: suspend for 20 s`, `WiFi suspending`, `WiFi disconnected — reason 8`, `WiFi suspended`, `[example] wifi link Connected -> Suspended`, then `[mqtt] disconnected` within about a second.
3. Pass while suspended: no `[wifi] reconnecting in` or `WiFi reconnect in` line, no connect request, and every tick reports `would block`.
   On IDF no MQTT line at all may appear while suspended (since Step 8b-2 the session is gated on the link, so esp-mqtt makes no attempt and logs no transport error); a `[mqtt] reconnecting in` or `[mqtt] error` line is a failure.
   On IDF a tick `queued` between the suspend and `[mqtt] disconnected` may still reach the broker after the resume; a `would block` tick never does.
4. Pass on the broker: `just mqtt-watch` shows `<prefix>/status offline` once per cycle, right at the reconnect and immediately followed by `online` (client-id takeover: the 20 s suspend is shorter than the 45 s will timeout, so the broker publishes the will as it drops the old session).
5. Pass on resume: `[example] demo: resume`, then `[wifi] resume requested` and `[wifi] link up #<n>` on hal, or `WiFi resumed, connect requested` and ESP-IDF's own `sta ip:` line on IDF.
   Exactly one connect per resume: no `WiFi connect requested` from the worker and no `reconnecting in` / `reconnect in` line.
6. Pass after resume: `[mqtt] connected`, `[example] connect #<n>` one higher than the cycle before, `status online` retained again, and the first tick after `[mqtt] connected` is `queued` (within 5 s); hal reconnects MQTT within seconds of link up, IDF at esp-mqtt's next retry (up to about 10 s later).
7. Pass overall: no panic, no reboot, and the minute heap log stays stable across cycles.

## Results

One line per tier; copy the block into the feature doc session log.

| Scenario             | Tier    | Board | Result      | Evidence (serial / broker line) |
|:---------------------|:--------|:------|:------------|:--------------------------------|
| R1 router pause      | esp-hal |       | pass / fail |                                 |
| R1 router pause      | ESP-IDF |       | pass / fail |                                 |
| R2 wrong credentials | esp-hal |       | pass / fail |                                 |
| R2 wrong credentials | ESP-IDF |       | pass / fail |                                 |
| R3 suspend/resume    | esp-hal |       | pass / fail |                                 |
| R3 suspend/resume    | ESP-IDF |       | pass / fail |                                 |

R1 timings: pause → `status offline`: `__ s`; un-pause → first `queued` tick: `__ s`. Git rev: `_______`.
R3 timings per tier: `demo: resume` → `[mqtt] connected`: `__ s`.

Attach the serial logs (`tmp/<tier>-reconnect-serial.log`); the examples never print the Wi-Fi password.
