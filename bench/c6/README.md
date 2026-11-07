# Bench — ESP32-C6-DevKitC-1

No wiring: the WS2812 on GPIO8 is on board; connect the USB port wired to the chip's USB-Serial-JTAG.

| Check      | Examples                                                    | Verdict                                                         |
|:-----------|:------------------------------------------------------------|:----------------------------------------------------------------|
| `connect`  | `hal_c6_connect_async_led`, `idf_c6_connect_nonblocking_rgb` | DHCP lease, LED colours confirmed, 20 s without a reboot       |
| `mqtt-lwt` | `hal_c6_mqtt_lwt`                                           | connect #1, ticks, `mqtt-watch` confirmed, connect #2 after a router block |
| `soak`     | `hal_c6_mqtt_lwt` | 10 min (shorter at the prompt), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_c6_mqtt`                                               | retained `online`, heartbeats, reconnect after a router block        |

```sh
just bench c6
```
