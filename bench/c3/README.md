# Bench — ESP32-C3 Super Mini

No wiring; connect the board over its native USB port.

| Check      | Examples                | Verdict                                                          |
|:-----------|:------------------------|:-----------------------------------------------------------------|
| `connect`  | `hal_c3_connect_async`  | DHCP lease, 20 s without a reboot, link up #2 after a router block |
| `mqtt-lwt` | `hal_c3_mqtt_lwt`       | connect #1, ticks, retained `online` and live ticks probed at the broker, connect #2 after a router block |
| `soak`     | `hal_c3_mqtt_lwt` | 10 min (Ctrl-C ends it early), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_c3_mqtt`           | retained `online`, heartbeats, reconnect after a router block         |
| `reentry`  | `idf_c3_provision_mqtt` | untouched boot loads, short BOOT reopens the portal, long BOOT erases |
| `espnow-cancel` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | C3-I7 and C3-H6: coordinator off, cancelled and not-found scans keep the pinned channel for 2 min; coordinator on, found and refound on one channel; a person powers the coordinator on |
| `espnow-flood` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | C3-I8 and C3-H7: three floods of 8 with 1 ms and 400 ms deadlines, each followed by a good send |
| `espnow-shutdown` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | C3-I9 and C3-H8: shutdown mid-scan, then 10 s up with no reboot |
| `espnow-shutdown-queued` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | C3-I9 and C3-H8: shutdown with 8 queued sends, all `Shutdown`, then 10 s up |
| `espnow-txpower` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | C3-I10 and C3-H9: three scans each read back the TX power unchanged |
| `espnow-soak` | `hal_c3_espnow_peer` | C3-H2 (G-E1): default scenario, 30 min at 100 ms with the failure rate below 1 % on the final counter line; attended half: router to channels 1, 6 and 11, the peer rescans and finds the coordinator |

```sh
just bench c3
```

## If it hangs

- `reentry` needs a provisioned board, and leaves it unprovisioned: provision again before the next run.
- Holding BOOT through the reset enters download mode; press RST again without BOOT.

The `espnow-*` checks run the ESP-IDF scout and the hal peer in turn and need a running coordinator on another board plus the build variables for the run: `ESPNOW_SCENARIO=<cancel|flood|shutdown|shutdown-queued|txpower> COORDINATOR_MAC=<coordinator STA MAC> ESPNOW_PEER_MAC=<the same MAC> just bench c3 espnow-<scenario> --port <c3 port>`.
`just bench hal_c3_espnow_peer` (or `idf_c3_espnow_scout`) with the same variables runs one tier through every `espnow-*` check; the gate ids differ per tier (C3-I7 to C3-I10 on the scout, C3-H6 to C3-H9 on the hal peer).
