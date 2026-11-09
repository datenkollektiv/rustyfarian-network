# Bench — CrowPanel 1.28" HMI (ESP32-S3)

No wiring: everything is on board.

| Check      | Examples                                        | Verdict                                                               |
|:-----------|:------------------------------------------------|:----------------------------------------------------------------------|
| `connect`  | `hal_esp32s3_connect_async`                     | DHCP lease, 20 s without a reboot, link up #2 after a router block      |
| `mqtt-lwt` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt`  | connect #1, ticks, retained `online` and live ticks probed at the broker, connect #2 after a router block |
| `soak`     | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | 10 min (Ctrl-C ends it early), every tick queued; heap trend read from the log |

Both `mqtt-lwt` examples run the same `examples-shared/lwt_app.rs`, so one set of steps judges both tiers.

```sh
just bench s3
```

## If it hangs

- Output ends in `waiting for download`: unplug and replug the board (without BOOT) while the run keeps waiting.
