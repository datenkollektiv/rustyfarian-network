# Bench — ESP32 WROOM-32 DevKit / Adafruit Feather V2

The console is UART0 through the USB-UART bridge, which also resets the chip.
Record the exact board (WROOM-32 DevKit or Feather V2) with each result.

| Check | Example(s) | What it proves | Prerequisites |
|:--|:--|:--|:--|
| `connect` | `hal_esp32_connect_async` | DHCP lease, 20 s stable, relink after a router block | |
| `mqtt` | `idf_esp32_mqtt` | retained `online`, heartbeats, reconnect after a router block | |
| `mqtt-lwt` | `hal_esp32_mqtt_lwt` | connect, ticks probed at the broker, reconnect after a router block | |
| `soak` | `hal_esp32_mqtt_lwt` | 10 min MQTT run, heap trend | |
