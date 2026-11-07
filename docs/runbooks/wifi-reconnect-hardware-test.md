# Runbook: Wi-Fi Reconnect Hardware Test (both tiers)

Run this after any change to `juggler::wifi::LinkMachine`, the IDF `WiFiManager` reconnect worker or the esp-hal `WifiLink`, and for the `rustyfarian-knob` M8 outage protocol.
It exercises a router-side outage (R1, R2) and the radio suspend/resume cycle (R3) on the two S3 LWT examples and records one result per tier.

Both S3 LWT examples compile the same application body (`examples-shared/lwt_app.rs`, ADR 022 Step 9), so every line the app itself writes starts with `[app]`; setup and library lines keep their `[wifi]`, `[mqtt]`, `WiFi …` and `[example]` prefixes.

**Reference device:** CrowPanel 1.28" HMI ESP32-S3 (`hal_esp32s3_mqtt_lwt` and `idf_esp32s3_mqtt_lwt`, console on the USB-Serial-JTAG port).

## Prerequisites

- Everything under "Prerequisites" and "Environment variables" in `docs/runbooks/esp-hal-mqtt-hardware-test.md`; the IDF twin reads the same keys.
- Default client id and prefix differ per tier (`hal-s3-mqtt` / `rustyfarian/hal-mqtt` and `idf-s3-mqtt` / `rustyfarian/idf-mqtt`), so both can share one broker.
- `just mqtt-watch` watches the hal prefix; use `just mqtt-watch idf` for the IDF twin.
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

1. Wait for `status online` on `just mqtt-watch` and at least three `[app] tick #<n> queued` lines, then pause the device on the router.
2. Pass: within seconds the console shows the link loss (`[wifi] disconnected —` on hal, `WiFi disconnected — reason` on IDF) and then `reconnecting in 1000 ms`, `2000 ms`, `4000 ms`, up to `30000 ms`, never more.
3. Pass: every tick while paused reports `[app] tick #<n> would block (mqtt Disconnected)`; no `[app] tick #<n> failed: <tier error>`, no panic, no reboot.
4. Pass: `just mqtt-watch` shows `<prefix>/status offline` about 45 s after the pause (1.5 × keep-alive); the broker fires the will because the client sent no DISCONNECT.
5. Un-pause after about 3 minutes; pass: the link comes back on the next attempt (`[wifi] link up #2` on hal, `WiFi connect requested` then an IP log on IDF) without a reboot.
6. Pass: MQTT reconnects on its own (`[mqtt] connected` on both tiers), `status online` is retained again and ticks are `queued` again.
   On IDF one `[mqtt] error` line when the link drops is the transport dying and is expected; after that the session is gated on the link, so no `Error transport connect` and no `[mqtt] reconnecting in` line may follow; the reconnect follows the `sta ip:` line within about a second.
   On IDF, at most the ticks `queued` between the pause and the `[mqtt] disconnected` line may arrive after the reconnect (esp-mqtt outbox, QoS 1); any tick logged `would block` must never reach the broker.
   Pass on both tiers: `[app] mqtt state Connected -> Disconnected` after the pause and `-> Connected` again after the un-pause.
   The example samples the state every 5 s, so it may miss a short `Connecting` phase; check each retry by its per-attempt log instead (`[mqtt] tcp connect failed` on hal, `[mqtt] reconnecting in` plus `Transport connected` on IDF).
7. Record the time from un-pause to the first `queued` tick; expect under 45 s on hal (back-off at most 30 s plus the MQTT connect) and under 35 s on IDF (at most one 30 s Wi-Fi back-off plus the connect, with no MQTT retry wait).

### R2 — wrong credentials (optional)

1. Build with a wrong `WIFI_PASS`, flash, watch the console for 3 minutes.
2. Pass: attempts keep coming with the delay capped at `30000 ms`; no panic, no reboot, heap stable.

### R3 — suspend/resume demo (20 s every 60 s)

Set `MQTT_SUSPEND_DEMO=1` in `.env`, rebuild and flash; the example suspends at every full minute of uptime and resumes 20 s later.
Watch at least three cycles per tier.

1. Pass on suspend, hal: `[app] demo: suspend for 20 s`, `[wifi] suspend requested`, `[wifi] suspended —`, `[app] demo: is_suspended = true`, `[app] wifi link Connected -> Suspended`, then `[mqtt] disconnected (`…`), waiting for network` within about 0.5 s.
2. Pass on suspend, IDF: `[app] demo: suspend for 20 s`, `WiFi suspending`, `WiFi disconnected — reason 8`, `WiFi suspended`, `[app] wifi link Connected -> Suspended`, then `[mqtt] disconnected` within about a second.
3. Pass while suspended: no `[wifi] reconnecting in` or `WiFi reconnect in` line, no connect request, and every tick reports `[app] tick #<n> would block (mqtt Disconnected)`.
   On IDF one `[mqtt] error` and one `[mqtt] disconnected` line at the moment the radio stops are the transport's dying gasp and are expected; a `[mqtt] reconnecting in` line or any connect attempt is a failure.
   On IDF a tick `queued` between the suspend and `[mqtt] disconnected` may still reach the broker after the resume; a `would block` tick never does.
4. Pass on the broker: `just mqtt-watch` shows `<prefix>/status offline` once per cycle, right at the reconnect and immediately followed by `online` (client-id takeover: the 20 s suspend is shorter than the 45 s will timeout, so the broker publishes the will as it drops the old session).
5. Pass on resume: `[app] demo: resume`, then `[wifi] resume requested` and `[wifi] link up #<n>` on hal, or `WiFi resumed, connect requested` and ESP-IDF's own `sta ip:` line on IDF.
   Exactly one connect per resume: no `WiFi connect requested` from the worker and no `reconnecting in` / `reconnect in` line.
6. Pass after resume: `[mqtt] connected`, `[app] connect #<n> free heap: <N> B` one higher than the cycle before, `status online` retained again, and the first tick after `[mqtt] connected` is `[app] tick #<n> queued` (within 5 s); hal reconnects MQTT within seconds of link up, IDF at esp-mqtt's next retry (up to about 10 s later).
7. Pass overall: no panic, no reboot, and the minute `[app] heap free: <N> B, connects: <n>` log stays stable across cycles.

## Results

One line per tier; copy the block into the feature doc session log.

| Scenario             | Tier    | Board        | Result | Evidence (serial / broker line)                                                     |
|:---------------------|:--------|:-------------|:-------|:------------------------------------------------------------------------------------|
| R1 router pause      | esp-hal | CrowPanel S3 | pass   | back-off 1→30 s, 0 ticks `queued` while down, MQTT back on the next 30 s attempt    |
| R1 router pause      | ESP-IDF | CrowPanel S3 | pass   | back-off 1→30 s, ticks `would block`, MQTT back 270 ms after `sta ip:`              |
| R2 wrong credentials | esp-hal | ESP32-C3     | pass   | hint on `FourWayHandshakeTimeout`, ladder capped at 30000 ms (`tmp/c3-r2-hal3.log`) |
| R2 wrong credentials | ESP-IDF | ESP32-C3     | pass   | hint on `reason 15`, none on `205`, capped at 30000 ms (`tmp/c3-r2-idf-async4.log`) |
| R3 suspend/resume    | esp-hal | CrowPanel S3 | pass   | 3 cycles, `link up #2/#3`, no retry line, broker offline→online                     |
| R3 suspend/resume    | ESP-IDF | CrowPanel S3 | pass   | 3 cycles, gate parks before the drop, broker offline→online                         |

R1 timings: pause → `status offline`: about 45 s (the keep-alive will, not timestamped); un-pause → first `queued` tick: 33.5 s on IDF, spent waiting out one 30 s back-off, then 0.3 s from `sta ip:` to `[mqtt] connected`. Git rev: `c5146ca`.
R3 timings per tier: `demo: resume` → `[mqtt] connected`: esp-hal sub-second (same serial tick); ESP-IDF 3.6 s, of which 3.5 s is association and 80 ms the MQTT connect.
R3 note: on IDF the suspend still logs one `[mqtt] error: ESP_FAIL` when the radio stops under esp-mqtt, plus the expected `[mqtt] disconnected`; suppressing it would need `suspend` to wait for the client to close, which no handshake offers today.

### Step 9 rerun: one shared application, CrowPanel 1.28" ESP32-S3, 2026-11-03

Working tree at `f81d83e` plus the uncommitted Step 9 change; both examples now compile `examples-shared/lwt_app.rs`, so the app lines start with `[app]`.

- R1 esp-hal: `[wifi] disconnected — AuthenticationLeave`, back-off `1000 … 30000 ms`, `[app] wifi link Connected -> Backoff`, `[app] mqtt state Connected -> Disconnected`, every tick `would block (mqtt Disconnected)`, broker `offline`.
  One attempt after the unblock still failed `AuthenticationExpired` (the router had not applied the removal yet); the next 30 s attempt reached `[wifi] link up #2`, `[mqtt] connected`, `[app] connect #2 free heap: 25880 B`, and `[app] tick #38 queued` immediately after.
- R1 ESP-IDF: `WiFi disconnected — reason 2 (AUTH_EXPIRE)` then `reason 205 (CONNECTION_FAIL)`, ladder `1000 … 30000 ms (attempt 8)`, broker `offline`.
  Recovery `sta ip: 192.168.31.179` about 21 s after the unblock, `[mqtt] network up, connecting` 110 ms later, `[app] connect #2 free heap: 205724 B`, `[app] tick #95 queued`.
- R3 esp-hal: three cycles, `link up #2/#3/#4`, `connect #2/#3/#4`, heap flat at `25928 B`, no retry line while suspended.
- R3 ESP-IDF: three cycles, `connect #2/#3/#4`, heap `206764 → 206568 → 206392 B`, one expected `[mqtt] error: ESP_FAIL` and one `mqtt_client: Writing failed: errno=119` per suspend, no retry line while suspended.
- T6 soak ESP-IDF: `10 min`, `123` ticks with no gap and no disconnect, but the heap drifted `2148 B` (`206836 → 204688 B`), outside the esp-hal MQTT runbook's 1 KiB criterion; the last sample alone dropped `1152 B`, so a longer run is needed to tell a leak from sampling noise.
- T6 soak esp-hal: `4 h 28 min`, `3210` ticks, one external outage of about 25 min (paced `10000 ms` retries while the link still read `Connected`, then `BeaconTimeout` and `link up #2`), after which 3 h were clean and the heap sat at `25928 B`.
- Both tiers: the `[app]` template sets are identical under `sed -E 's/[0-9]+/N/g'`, except the per-tier topic prefix in the `rx` line and which intermediate state the 5 s sample catches.
  Link states seen are `{Connected, Connecting, Backoff}` on both; MQTT states are `{Connected, Disconnected}` on both (ESP-IDF's `Connecting` completed inside one sample).

Attach the serial logs (`tmp/<tier>-reconnect-serial.log`); the examples never print the Wi-Fi password.
