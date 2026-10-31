# Runbook: Wi-Fi Reconnect Hardware Test (both tiers)

Run this after any change to `juggler::wifi::LinkMachine`, the IDF `WiFiManager` reconnect worker or the esp-hal `WifiLink`, and for the `rustyfarian-knob` M8 outage protocol.
It exercises a router-side outage on the two S3 LWT examples and records one result per tier.

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
6. Pass: MQTT reconnects on its own (`[mqtt] connected` on both tiers, from esp-mqtt's own retry on IDF), `status online` is retained again and ticks are `queued` again.
   On IDF, at most the ticks `queued` between the pause and the `[mqtt] disconnected` line may arrive after the reconnect (esp-mqtt outbox, QoS 1); any tick logged `would block` must never reach the broker.
7. Record the time from un-pause to the first `queued` tick; expect under 45 s (back-off at most 30 s plus the MQTT connect).

### R2 — wrong credentials (optional)

1. Build with a wrong `WIFI_PASS`, flash, watch the console for 3 minutes.
2. Pass: attempts keep coming with the delay capped at `30000 ms`; no panic, no reboot, heap stable.

## Results

One line per tier; copy the block into the feature doc session log.

| Scenario             | Tier    | Board | Result      | Evidence (serial / broker line) |
|:---------------------|:--------|:------|:------------|:--------------------------------|
| R1 router pause      | esp-hal |       | pass / fail |                                 |
| R1 router pause      | ESP-IDF |       | pass / fail |                                 |
| R2 wrong credentials | esp-hal |       | pass / fail |                                 |
| R2 wrong credentials | ESP-IDF |       | pass / fail |                                 |

R1 timings: pause → `status offline`: `__ s`; un-pause → first `queued` tick: `__ s`. Git rev: `_______`.

Attach the serial logs (`tmp/<tier>-reconnect-serial.log`); the examples never print the Wi-Fi password.
