# Bench — CrowPanel 1.28" HMI (ESP32-S3)

No wiring: everything is on board.

| Check      | Examples                                        | Verdict                                                               |
|:-----------|:------------------------------------------------|:----------------------------------------------------------------------|
| `connect`  | `hal_esp32s3_connect_async`                     | DHCP lease, 20 s without a reboot, link up #2 after a router block      |
| `mqtt-lwt` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt`  | connect #1, ticks, retained `online` and live ticks probed at the broker, connect #2 after a router block |
| `soak`     | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | 10 min (Ctrl-C ends it early), every tick queued; heap trend read from the log |
| `mqtt-scenarios` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | T1 startup message retained, T4 broker restart, T5b router pause (R1; T5a, AP off, is not run on a mesh); S3-H1, S3-B1 R1 and the S3-B2 startup message |
| `suspend`  | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | three R3 cycles with `MQTT_SUSPEND_DEMO=1`: `wait_suspended`, no reconnect while suspended, one connect per resume, will and `online` at the broker; S3-B1 R3 and S3-B2 |
| `provision` | `hal_esp32s3_provision_mqtt`                   | blank store opens the portal, `just portal-save` with two extras, record survives a USB power cut; S3-H2 (erase first, `PORTAL_SSID_OVERRIDE=bench-s3-portal`) |
| `dns-subscribe` | `hal_esp32s3_mqtt_lwt`                       | broker reached by a LAN hostname (`MQTT_HOST=bench-broker`), `online` and ticks probed at the broker, reconnect after a broker restart; the S3-B2 esp-hal DNS line |
| `dns-unresolvable` | `hal_esp32s3_mqtt_lwt`                    | `MQTT_HOST=no-such-broker.invalid`: two failed lookups 10 s apart, never connects, no reboot; the S3-B2 esp-hal DNS line, failure half |
| `ota`      | `hal_esp32s3_ota`                               | boot 1 downloads, verifies and activates the served image, boot 2 runs slot 1 and marks it valid; S3-H3 happy path, run via `just bench-ota s3 ota …` (image server started by the check) |
| `ota-rollback` | `hal_esp32s3_ota`                           | unhealthy served image never marks itself valid, a USB reset rolls back to slot 0 and the target is not retried, a second reset repeats that; S3-H3 rollback half, run via `just bench-ota s3 ota-rollback …` |

The `mqtt-lwt`, `soak`, `mqtt-scenarios`, `suspend` and `dns-*` examples run the same `examples-shared/lwt_app.rs`, so one set of steps judges both tiers.
`mqtt-scenarios` needs a build without `MQTT_SUSPEND_DEMO`, `suspend` one with it; the runner rebuilds per run, so set the variable on the `just bench` command.

```sh
just bench s3
```

The two `ota` checks need the served image and its SHA-256 at build time, so run them through `just bench-ota` (see the check headers); inside a plain `just bench s3` they fail at the first step with `nothing to do`.

## If it hangs

- Output ends in `waiting for download`: unplug and replug the board (without BOOT) while the run keeps waiting.
