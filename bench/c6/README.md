# Bench — ESP32-C6-DevKitC-1

No wiring: the WS2812 on GPIO8 is on board; connect the USB port wired to the chip's USB-Serial-JTAG.

| Check      | Examples                                                    | Verdict                                                         |
|:-----------|:------------------------------------------------------------|:----------------------------------------------------------------|
| `connect`  | `hal_c6_connect_async_led`, `idf_c6_connect_nonblocking_rgb` | DHCP lease, LED colours confirmed, 20 s without a reboot       |
| `mqtt-lwt` | `hal_c6_mqtt_lwt`                                           | connect #1, ticks, retained `online` and live ticks probed at the broker, connect #2 after a router block |
| `mqtt-scenarios` | `hal_c6_mqtt_lwt`                                     | C6-H2 T1 to T5: connect, inbound, LWT on power cut, broker restart and router pause (T5a, AP off, is not run on a mesh); T1, T2 and T4 unattended against the local `just mosquitto` broker; needs a newer runner and `MQTT_HOST` set to this machine (see [../README.md](../README.md)) |
| `dns-subscribe` | `hal_c6_mqtt_lwt`                                    | C6-H5: broker by hostname (`MQTT_HOST=bench-broker`), runtime subscribe, delivery before and after a broker restart; no person |
| `dns-unresolvable` | `hal_c6_mqtt_lwt`                                 | C6-H5: an NXDOMAIN broker name logs `dns lookup … failed` and retries every 10 s; no person |
| `provision` | `hal_c6_provision_mqtt`                               | C6-H1: blank store opens the portal, `just portal-save` with two extras commits, the record survives a USB power cut (`extras=2`); a person joins the SoftAP and replugs |
| `soak`     | `hal_c6_mqtt_lwt` | 10 min (Ctrl-C ends it early), every tick queued; heap trend read from the log |
| `mqtt`     | `idf_c6_mqtt`                                               | retained `online`, heartbeats, reconnect after a router block        |

```sh
just bench c6
```
