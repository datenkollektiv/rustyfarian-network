# Bench — ESP32-C3 Super Mini

No wiring; connect the board over its native USB port.

| Check      | Examples                | Verdict                                                          |
|:-----------|:------------------------|:-----------------------------------------------------------------|
| `connect`  | `hal_c3_connect_async`  | DHCP lease, 20 s without a reboot, link up #2 after a router block |
| `mqtt-lwt` | `hal_c3_mqtt_lwt`       | connect #1, ticks, `mqtt-watch` confirmed, connect #2 after a router block |
| `soak`     | `hal_c3_mqtt_lwt` | 10 min (shorter at the prompt), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_c3_mqtt`           | retained `online`, heartbeats, reconnect after a router block         |
| `reentry`  | `idf_c3_provision_mqtt` | untouched boot loads, short BOOT reopens the portal, long BOOT erases |

```sh
just bench c3
```

## If it hangs

- `reentry` needs a provisioned board, and leaves it unprovisioned: provision again before the next run.
- Holding BOOT through the reset enters download mode; press RST again without BOOT.
