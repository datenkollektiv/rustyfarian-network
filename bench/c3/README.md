# Bench — ESP32-C3 Super Mini

No wiring; connect the board over its native USB port.

| Check      | Examples                | Verdict                                                          |
|:-----------|:------------------------|:-----------------------------------------------------------------|
| `connect`  | `hal_c3_connect_async`  | DHCP lease, 20 s without a reboot, link up #2 after a router block |
| `mqtt-lwt` | `hal_c3_mqtt_lwt`       | connect #1, ticks, retained `online` and live ticks probed at the broker, connect #2 after a router block |
| `soak`     | `hal_c3_mqtt_lwt` | 10 min (Ctrl-C ends it early), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_c3_mqtt`           | retained `online`, heartbeats, reconnect after a router block         |
| `reentry`  | `idf_c3_provision_mqtt` | untouched boot loads, short BOOT reopens the portal, long BOOT erases |
| `espnow-cancel` | `idf_c3_espnow_scout` | C3-I7: coordinator off, cancelled and not-found scans keep the pinned channel for 2 min; coordinator on, found and refound on one channel; a person powers the coordinator on |
| `espnow-flood` | `idf_c3_espnow_scout` | C3-I8: three floods of 8 with 1 ms and 400 ms deadlines, each followed by a good send |
| `espnow-shutdown` | `idf_c3_espnow_scout` | C3-I9: shutdown mid-scan, then 10 s up with no reboot |
| `espnow-shutdown-queued` | `idf_c3_espnow_scout` | C3-I9: shutdown with 8 queued sends, all `Shutdown`, then 10 s up |
| `espnow-txpower` | `idf_c3_espnow_scout` | C3-I10: three scans each read back the TX power unchanged |

```sh
just bench c3
```

## If it hangs

- `reentry` needs a provisioned board, and leaves it unprovisioned: provision again before the next run.
- Holding BOOT through the reset enters download mode; press RST again without BOOT.

The `espnow-*` checks need a running coordinator on another board and two build variables for the run: `ESPNOW_SCENARIO=<cancel|flood|shutdown|shutdown-queued|txpower> COORDINATOR_MAC=<coordinator STA MAC> just bench c3 espnow-<scenario> --port <c3 port>`.
