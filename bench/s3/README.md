# Bench — CrowPanel 1.28" HMI (ESP32-S3)

| Check | Example(s) | What it proves | Prerequisites |
|:--|:--|:--|:--|
| `connect` | `hal_esp32s3_connect_async` | DHCP lease, 20 s stable, relink after a router block | |
| `mqtt-lwt` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | connect, ticks probed at the broker, reconnect after a router block | |
| `soak` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | 10 min MQTT run, heap trend | |
| `mqtt-scenarios` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | retained startup message, broker restart, router pause | build without `MQTT_SUSPEND_DEMO` |
| `suspend` | `hal_esp32s3_mqtt_lwt`, `idf_esp32s3_mqtt_lwt` | three suspend/resume cycles, no reconnect while suspended | build with `MQTT_SUSPEND_DEMO=1` |
| `provision` | `hal_esp32s3_provision_mqtt` | portal record with two extras survives a power cut | erase first, `PORTAL_SSID_OVERRIDE=bench-s3-portal` |
| `dns-subscribe` | `hal_esp32s3_mqtt_lwt` | broker by hostname, reconnect after a broker restart | `MQTT_HOST=bench-broker` |
| `dns-unresolvable` | `hal_esp32s3_mqtt_lwt` | unresolvable broker name, never connects, no reboot | `MQTT_HOST=no-such-broker.invalid` |

If output ends in `waiting for download`, unplug and replug the board without BOOT.
