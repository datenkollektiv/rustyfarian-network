# Bench — ESP32 WROOM-32 DevKit / Adafruit Feather V2

No wiring; the console is UART0 through the board's USB-UART bridge, which also resets the chip.
Record the exact board in the runbook's results table.

| Check      | Examples                  | Verdict                                                               |
|:-----------|:--------------------------|:----------------------------------------------------------------------|
| `connect`  | `hal_esp32_connect_async` | DHCP lease, 20 s without a reboot, link up #2 after a router block      |
| `mqtt-lwt` | `hal_esp32_mqtt_lwt`      | connect #1, ticks, `mqtt-watch` confirmed, connect #2 after a router block |
| `soak`     | `hal_esp32_mqtt_lwt` | 10 min (shorter at the prompt), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_esp32_mqtt`          | retained `online`, heartbeats, reconnect after a router block              |

```sh
just bench esp32
```
