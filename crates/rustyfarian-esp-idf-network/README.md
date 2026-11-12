# rustyfarian-esp-idf-network

ESP-IDF (std) drivers for Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning on ESP32.

This crate provides tested, production-ready implementations for connectivity on ESP-IDF targets (ESP32, ESP32-C3, ESP32-C6, ESP32-S3).
All features are **opt-in** and fully compatible with each other.

**Tier:** ESP-IDF (std, FreeRTOS-based)

## Features

Domain features are opt-in; `default = []` means you explicitly declare which domains you need:

| Feature        | What it gates                                                  | Enables juggler feature | Chip support | Notes                                                      |
|:---------------|:---------------------------------------------------------------|:------------------------|:-------------|:-----------------------------------------------------------|
| `wifi`         | Wi-Fi STA/AP manager, LED status feedback                      | `wifi`                  | All          | Blocking API; includes softAP lifecycle.                   |
| `mqtt`         | MQTT client with builder and automatic reconnection            | `mqtt`, `std`           | All          | Requires `wifi` for bootstrap (compile-checked).           |
| `lora`         | SX1262 radio driver, LoRaWAN OTAA/ADR support                  | `lora`                  | All          | Firmware selects one `critical-section` provider (below).  |
| `espnow`       | ESP-NOW peer-to-peer messaging, broadcast mode                 | `espnow`                | All          | No Wi-Fi required; works in AP or isolation.               |
| `ota`          | Over-the-air firmware update (streaming, SHA-256 verified)     | `ota`                   | All          | Blocking API; downloads firmware from HTTP only (ADR 011). |
| `provisioning` | SoftAP captive-portal Wi-Fi and device credential provisioning | `provisioning`          | All          | Requires `wifi` feature; NVS credential store.             |

**Special notes:**

- All features use **blocking APIs** — no async/await.
- `mqtt` and `provisioning` implicitly depend on `wifi` for bootstrap (compile error if `wifi` is omitted).
- Dual SPI support via `esp-idf-hal` abstractions; no board-specific SPI routing code needed.
- `lora` needs exactly one `critical-section` provider, selected by the firmware: `esp-idf-hal`'s `critical-section` feature (a recursive FreeRTOS mutex) or `rustyfarian-esp-idf-power`'s `critical-section-impl` (interrupt masking).

## ⚠️ Required `sdkconfig.defaults` for SoftAP provisioning

**If you enable the `provisioning` feature, you must raise the ESP-IDF httpd header limit**, or the captive portal will not work.

OS captive-portal browsers send request headers larger than the ESP-IDF httpd default (512 B).
Without this setting, the provisioning portal's `GET /` is rejected with `431 Request Header Fields Too Large` and the sign-in page never renders (the device connects but no portal appears).
Add to your **workspace-root** `sdkconfig.defaults`:

```text
CONFIG_HTTPD_MAX_REQ_HDR_LEN=2048
```

`2048` matches this workspace's default and the esp-hal portal's request-size cap.
After editing `sdkconfig.defaults`, clean the `esp-idf-sys-*` build directory (or run `cargo clean`) so CMake reconfigures.

## Cargo.toml

Add to your `Cargo.toml`:

```toml
[dependencies]
rustyfarian-esp-idf-network = { version = "0.7", features = ["wifi", "mqtt"] }
```

## Example: Wi-Fi + MQTT (OTA Runtime)

For OTA-enabled firmware using the runtime loop, see [`docs/runbooks/ota-hardware-test.md`](../../docs/runbooks/ota-hardware-test.md) for the full test procedure and [`examples/idf_c3_ota_runtime.rs`](examples/idf_c3_ota_runtime.rs) for example integration (command intake, worker start, health policy, restart callback).

## Example: Wi-Fi + MQTT

Register subscriptions on the builder; publish retained status from `on_connect` using its `client` argument.
Never call `MqttHandle` methods from any callback.
`QoS`, `MqttConfig` and `LwtConfig` come from `juggler` and are shared with the bare-metal tier; the raw `client` passed to `on_connect` is an `EspMqttClient`, so convert with `to_esp_qos`.

```rust
use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig};
use rustyfarian_esp_idf_network::mqtt::{to_esp_qos, MqttBuilder, MqttConfig, QoS};

let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(
    modem,
    sys_loop,
    Some(nvs),
    wifi_config,
    None::<&mut MyLed>
)?;

if let Some(ip) = wifi.get_ip(10000)? {
    println!("Connected: {}", ip);
}

let mqtt_config = MqttConfig::new("mqtt.example.com", 1883, "my-device");
let mqtt = MqttBuilder::new(mqtt_config)
    .subscribe("commands/#", QoS::AtMostOnce)
    .on_connect(|client, _clean_session| {
        client.enqueue("status", to_esp_qos(QoS::AtLeastOnce), true, b"online")?;
        Ok(())
    })
    .on_message(|topic, data| {
        println!("Message on {}: {:?}", topic, data);
    })
    .build()?;

while !mqtt.is_connected() {
    std::thread::sleep(std::time::Duration::from_millis(100));
}
```

## Example: SX1262 LoRa with OTAA Join

```rust
use rustyfarian_esp_idf_network::lora::EspIdfLoraRadio;
use lorawan_device::nb::Device;

let spi = SpiDeviceDriver::new(spi_master, Some(cs_pin))?;
let radio = EspIdfLoraRadio::new(spi, (rst_pin, busy_pin, antenna_pin, dio1_pin))?;

let mut device = Device::new(config, (), EspRng);
let join_result = device.join(join_mode)?;

loop {
    match device.process_event(next_response) {
        Ok(Next::Send(TxConfig { .. })) => { /* send the packet */ }
        Ok(Next::Recv(RxConfig { .. })) => { /* set up RX window */ }
        Ok(Response::RxComplete(_)) => { /* join accepted or downlink rx'd */ }
        Err(_) => { /* handle error */ }
    }
}
```

## Example: ESP-NOW coordinator, peer and scouts

The ESP-IDF ESP-NOW examples talk to each other across two boards.
`idf_c3_espnow_coordinator` (ESP32-C3 Super Mini) and `idf_c6_espnow_coordinator` (ESP32-C6 DevKit, no status LED) are associated stations with ESP-NOW alongside; both run the shared echo app in `examples-shared/espnow_echo_app.rs`, which answers scan probes with the current channel and echoes every other frame.
`idf_c3_espnow_peer` (ESP32-C3 Super Mini) owns its radio as a hidden SoftAP (`EspIdfEspNow::init_with_radio`) and runs the shared peer app in `examples-shared/espnow_peer_app.rs` on a `PeerSession` (`espnow::{PeerSession, PeerLinkConfig, EmbassyClock}`), the twin of `hal_c3_espnow_peer`: the session scans for the coordinator and rescans after 20 consecutive unacknowledged sends, and the app sends an 8-byte frame every 100 ms and logs its counters every 10 s.
`idf_c3_espnow_scout` (SoftAP channel lock), `idf_c3_espnow_scout_promisc` (STA plus promiscuous bracket) and `idf_c3_espnow_sta_scout` are demos of the blocking API (`scan_for_peer_blocking`, `send_and_wait`): they scan for the coordinator and send a frame every second.

```bash
WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example idf_c6_espnow_coordinator
ESPNOW_PEER_MAC="aa:bb:cc:dd:ee:ff" just build-example idf_c3_espnow_peer
COORDINATOR_MAC="aa:bb:cc:dd:ee:ff" just build-example idf_c3_espnow_scout
```

The coordinator prints its station MAC at boot (`Coordinator STA MAC: …`); pass it to the peer as `ESPNOW_PEER_MAC` (it also accepts `COORDINATOR_MAC`) and to the scouts as `COORDINATOR_MAC`.
The coordinator and peer examples are excluded from the published crate because they include a file outside the package.

## Tier capability differences

The tiers share one API ([ADR 022](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/022-tier-parity-one-async-api.md)).
The differences below are capabilities, not API differences.
Design detail: [Wi-Fi suspend, resume and deep sleep](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/wifi-suspend-and-deep-sleep.md).

| Capability             | `rustyfarian-esp-idf-network`                                                          | `rustyfarian-esp-hal-network`                                    |
|:-----------------------|:---------------------------------------------------------------------------------------|:-----------------------------------------------------------------|
| Radio while suspended  | Stopped (`esp_wifi_stop`)                                                              | Powered, station disconnected                                    |
| Broker address         | Hostname or IPv4 (DNS via lwIP)                                                        | IPv4 literal or DNS name (no IPv6)                               |
| Inbound messages       | Callback and/or the `receive()` queue                                                  | `receive()` channel; no callback                                 |
| Inbound retain and QoS | Not reported (retain is always false, QoS always `AtMostOnce`)                         | Reported                                                         |
| Executor               | std threads; `block_on` for the async app                                              | `esp-rtos` plus the embassy executor                             |
| Suspend call           | Blocking; the radio is stopped when it returns, so `wait_suspended()` is ready at once | Non-blocking signal; `wait_suspended()` completes once suspended |
| ESP-NOW sender         | Worker thread `espnow-tx` (6 KiB stack), started by the constructors, joined on drop    | `run_sender()` in a firmware task; without it async sends and scans fail with `NoSenderRunner` |
| ESP-NOW sync send      | `send_and_wait(mac, data, timeout_ms)` queues behind async sends; `send` gives up after `SEND_QUEUE_DEADLINE` (1 s) | `send_and_wait(mac, data)`, no timeout; bypasses the queue and returns `Busy` while a send is in flight |
| ESP-NOW blocking scan  | `scan_for_peer_blocking`, `init_with_radio_scanning`                                   | Async `scan_for_peer` only                                       |
| ESP-NOW receive queue  | 32 frames by default (`init_with_capacity` sets it); a frame at a full queue is dropped and counted in a warning | 10 frames, fixed by `esp-radio`; drops the oldest |
| ESP-NOW `try_recv`     | Draws from the same queue as `receive`; a frame taken there is not seen by `receive`   | Returns `None`                                                   |
| ESP-NOW stop           | `shutdown()` or drop                                                                   | `shutdown()` only; no `Drop`, because futures borrow the handle  |

## Integration with juggler

This crate re-exports all domain modules from `juggler`, so you can import validation logic and types directly:

```rust
use rustyfarian_esp_idf_network::wifi::WiFiConfig;
use rustyfarian_esp_idf_network::mqtt::MqttConnectionState;
```

## Resources

- [Workspace repository](https://github.com/datenkollektiv/rustyfarian-network)
- [ADR 016: Crate Consolidation for Publishing](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/016-crate-consolidation-for-publishing.md)
- [Feature doc: 3-Crate Consolidation v1](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/features/archive/crate-consolidation-3-crates-v1.md)

## License

MIT or Apache-2.0
