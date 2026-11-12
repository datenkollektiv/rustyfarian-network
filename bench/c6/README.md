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
