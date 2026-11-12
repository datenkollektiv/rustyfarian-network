# Runbook: esp-hal MQTT Hardware Test

Run this after any change to `rustyfarian-esp-hal-network::mqtt` or a `minimq` bump, and before every release that ships the bare-metal `mqtt` feature.
Scenarios T1 to T6 cover connect, LWT, reconnect, resubscribe, publish under Wi-Fi loss and a soak; the bench checks encode all of them except T5a.

**Reference devices:** CrowPanel 1.28" HMI ESP32-S3 (`hal_esp32s3_mqtt_lwt`, console on the CH340 UART) and any ESP32-C6 board (`hal_c6_mqtt_lwt`).

## Setup

- The prerequisites of the [bench README](../../bench/README.md); `mosquitto`, `mosquitto_sub` and `mosquitto_pub` on the test machine (`brew install mosquitto`), or Docker.
- `just mosquitto up` starts a local anonymous broker without persistence and prints `MQTT_HOST=<lan-ip>`; put that IPv4 into `.env`, never `localhost` or `127.0.0.1`.
- Docker alternative: `docker run --rm -p 1883:1883 eclipse-mosquitto:2 mosquitto -c /mosquitto-no-auth.conf`.
- `just mqtt-watch` prints every topic of the example, `just mqtt-pub cmd hello` publishes to `<prefix>/cmd`, `just mqtt-status` reads the retained status; all read `MQTT_*` from `.env`.
- `just run hal_esp32s3_mqtt_lwt` (or `hal_c6_mqtt_lwt`) builds, flashes and monitors; `just flash` fetches the IDF-built bootloader for Xtensa on first use (a one-time IDF build).
- The examples capture these variables at build time, so rebuild after every `.env` change; a direnv `.envrc` exporting the same key overrides `.env`, so keep MQTT keys in `.env` only.
- The examples never print the Wi-Fi password, so serial logs can be attached as they are.

| Variable            | Default                       | Notes                                                          |
|:--------------------|:------------------------------|:---------------------------------------------------------------|
| `WIFI_SSID`         | required                      | 2.4 GHz AP                                                     |
| `WIFI_PASS`         | required                      | Wi-Fi password                                                 |
| `MQTT_HOST`         | required                      | Broker IPv4 literal or LAN hostname (resolved through the DHCP DNS server, see `c6/dns-subscribe`) |
| `MQTT_PORT`         | `1883`                        | Broker port                                                    |
| `MQTT_CLIENT_ID`    | `hal-s3-mqtt` / `hal-c6-mqtt` | ≤ 23 bytes                                                     |
| `MQTT_USER`         | unset                         | Optional username                                              |
| `MQTT_PASS`         | unset                         | Optional password                                              |
| `MQTT_TOPIC_PREFIX` | `rustyfarian/hal-mqtt`        | Topics are `<prefix>/status`, `<prefix>/cmd`, `<prefix>/tick`  |

## Scenarios

- T1 connect and retained `online`, T2 inbound subscription, T3 LWT on a USB power cut, T4 broker restart with reconnect and resubscribe, T5b per-device router pause: `c6/mqtt-scenarios`; T1, T4 and T5b on both S3 tiers: `s3/mqtt-scenarios`.
- T6 10-minute soak: `c6/soak`, `s3/soak`, `c3/soak`, `esp32/soak`.

Facts the checks do not encode:

- T1: record the `[app] connect #1 free heap: <N> B` value as the heap after connect.
- T4: a broker process stop yields an immediate reset (`session error`); a dead broker host whose TCP reset cannot be routed ends with `[mqtt] disconnected — transport stalled` and `[mqtt] reconnecting in 10000 ms` within about 75 s instead.
- T5: serial and broker timestamps come from different clocks unless `just mqtt-watch` runs on the broker host.
- T5b: a half-open path on the router may delay `status offline`, which is a router property, not a client failure; for packet-id matching keep the serial log around the first reconnect (`[mqtt] dropped publish on`) and run the broker with `mosquitto -v`.
- T6: the shared app prints heap and connect count only, no `dropped rx/tx` counters; the last `heap free` line is the heap after soak.

### T5a — AP power off (or MAC block at the AP)

No check: on a mesh the board roams to another AP of the same SSID, so switching one AP off is no outage.

1. Start `just mqtt-watch`, note the last `[app] tick #<n> queued`, switch the AP off for at least 90 s.
2. Pass: `[wifi] disconnected —`, then `[wifi] reconnecting in 1000 ms` doubling up to `30000 ms`, and within one tick of the first line `[mqtt] waiting for network` and `[mqtt] disconnected — network lost` (the network watch, not the socket timeout).
3. Pass: the next tick logs `[app] mqtt state Connected -> Disconnected`, every tick while down reports `[app] tick #<n> would block (mqtt Disconnected)`; no `queued`, no `failed`, no panic, no reboot.
4. Pass: `status offline` at the broker within about 60 s (1.5 × keep-alive after the last packet plus the broker's check interval).
5. Switch the AP on; pass: `[wifi] link up #<k+1>` on the next back-off attempt (at most 30 s), optionally one `would block (mqtt Connecting)`, then `[mqtt] connected (session present: no, 1 subscriptions)` and `[app] connect #<m> free heap: <N> B` within 15 s of the link-up line, with no `[mqtt] waiting for network` on this path.
6. Pass: `status online` again and ticks `queued` again; at most one tick accepted as `queued` before the `[mqtt] disconnected` line may be missing at the broker (see the delivery note in the crate README).
7. Record the Wi-Fi down, `[mqtt] disconnected` and `[mqtt] connected` timestamps and the first tick `queued` again.

## History

Earlier results on the CrowPanel S3, all `minimq 0.13.5`; the 0.7.0 results are in the [release runbook](release-0.7.0-hardware-test.md).

- 2026-10-16, `d9e3b62`, run by the `rustyfarian-knob` maintainer; T2, T4 and T6 from the 2026-10-14 run in `docs/features/archive/esp-hal-mqtt-v1.md`.
  T1 pass; T3 pass with borderline timing, `status offline` 45 to 51 s after the cut against ≤ 45 s; T5a not run.
  T5b before the fix: state stayed `Connected` for 3 min 7 s, ticks 46 to 50 `queued` after the drop and 46 to 48 lost, recovery 32 s after link-up via `Transport(ConnectionReset)`; fixed by the network watch and the socket timeout (`docs/features/archive/esp-hal-mqtt-v1.md`, Session Log 2026-10-16).
  Heap 25 928 B after connect, 26 668 B during the outage, 25 880 B after the reconnect.
- 2026-10-18, `dceb1a3`, T5 against the network-loss fix, outage by pausing the whole Wi-Fi network for 2 to 4 minutes: `[mqtt] disconnected (network lost)` in the same second as the Wi-Fi loss, 0 ticks `queued` while down, `[mqtt] connected` 11 s after link-up (one run 16 s: the first TCP connect after link-up timed out, then the 10 s back-off).
  The LWT showed as retained `offline` on the first read after the resume, 6 s before the reconnect; tick 182 (in flight) was lost, 183 to 185 (queued during the beacon timeout) arrived in order after `online`, 186 and later reported `would block`.
  On that network the test machine reached the broker only through the paused Wi-Fi, so poll the retained status once per second to catch the LWT; suggestion: after a network-lost disconnect, retry the first TCP connect sooner than the 10 s back-off.
- 2026-10-31, `8add8a8` (includes `3a81a57`, ADR 021), knob maintainer: CrowPanel ESP32-S3 rev v0.2, 16 MB, MAC `1c:db:d4:b3:6d:40`, USB-Serial-JTAG (303A:1001), IDF v5.3.3 bootloader, broker `192.168.31.185:31883`, keep-alive 30 s, firmware calls `esp_rtos::start` and `with_wifi_scheduler_running(WIFI)`.
  Console output without a library-selected console, firmware-owned scheduler, connect, 11 of 11 ticks, LWT `offline` on reflash and `online` on boot, heap 25 928 B after connect: all pass.
  The first ~2 s after reset are not captured on USB-Serial-JTAG; ticks 3 and 4 arrived together; before the fix the knob's `features = ["mqtt", "esp32s3"]` failed the esp-println build script with `Currently enabled: uart, auto`; `2fd189e` had passed the console checks.
  Not covered: `with_ap_wifi_scheduler_running` and the error path with the library-started scheduler; the C3 side is in `docs/features/archive/esp-hal-wifi-init-with-running-scheduler-v1.md`.
- 2026-11-03, `f81d83e` plus the shared app (`examples-shared/lwt_app.rs`): T1, T2, T5a (as a MAC block at the AP) and T5b pass, heap 25 928 → 26 588 → 25 880 B; T6 ran 4 h 28 min (3210 ticks).
  From tick 878 MQTT could not reconnect for about 25 min (`session error`, `tcp connect timed out`, `mqtt connect timed out`, retried every 10 s) while the link still read `Connected`, until `[wifi] disconnected — BeaconTimeout`, `link up #2` and `connect #5`; the remaining 3 h had 0 disconnects, 3 ticks `would block (mqtt Connected)`, heap 25 928 B apart from one sample at 24 224 B (spread 1828 B, first and last equal); the outage was external.
