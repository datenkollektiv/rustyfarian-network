# Bench — ESP32-C6-DevKitC-1

Connect the USB port wired to the chip's USB-Serial-JTAG (the DevKitC has two).

| Check | Example(s) | What it proves | Prerequisites |
|:--|:--|:--|:--|
| `connect` | `hal_c6_connect_async_led`, `idf_c6_connect_nonblocking_rgb` | DHCP lease, LED colours, 20 s stable | a person confirms the LEDs |
| `mqtt` | `idf_c6_mqtt` | retained `online`, heartbeats, reconnect after a router block | |
| `mqtt-lwt` | `hal_c6_mqtt_lwt` | connect, ticks probed at the broker, reconnect after a router block | |
| `mqtt-scenarios` | `hal_c6_mqtt_lwt` | connect, inbound, LWT on power cut, broker restart, router pause | `just mosquitto`, `MQTT_HOST` = this machine |
| `soak` | `hal_c6_mqtt_lwt` | 10 min MQTT run, heap trend | |
| `dns-subscribe` | `hal_c6_mqtt_lwt` | broker by hostname, runtime subscribe, delivery across a broker restart | `MQTT_HOST=bench-broker` |
| `dns-unresolvable` | `hal_c6_mqtt_lwt` | unresolvable broker name retries every 10 s | |
| `provision` | `hal_c6_provision_mqtt` | portal record with two extras survives a power cut | a person joins the SoftAP and replugs |
| `ddi-idf` | `idf_c6_ddi` | DDI OTA against a lab server: forced update, `up_to_date` and chip gate (1007), cancel and reset mid-download, rollback (1004) and its sticky refusal, wrong sha256 (1002), each row probed on the server | lab DDI server (`DDI_MGMT_*`), local broker, seven bundles published with `just ddi-publish`, see header |
| `ddi-hal` | `hal_c6_ddi` | the `ddi-idf` chain step for step on the esp-hal tier, same patterns, only the example names differ | as `ddi-idf`, with `hal_*` bundles |

The `ddi-idf` and `ddi-hal` checks of the C3 and the C6 can run at the same time because they share the lab proxy; the first run to end leaves it up for the other, and a run that died without its teardown leaves a file in `tmp/bench-ddi-proxy.users/`, so delete that directory; a proxy that was already running when the first run started is borrowed and never stopped by a run.
