# rustyfarian-esp-idf-network

ESP-IDF (std) drivers for Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning on ESP32.

This crate provides tested, production-ready implementations for connectivity on ESP-IDF targets (ESP32, ESP32-C3, ESP32-C6, ESP32-S3).
All features are **opt-in** and fully compatible with each other.

**Tier:** ESP-IDF (std, FreeRTOS-based)

## Features

Domain features are opt-in; `default = []` means you explicitly declare which domains you need:

| Feature        | What it gates                                                  | Enables juggler feature | Chip support | Notes                                            |
|:---------------|:---------------------------------------------------------------|:------------------------|:-------------|:-------------------------------------------------|
| `wifi`         | Wi-Fi STA/AP manager, LED status feedback                      | `wifi`                  | All          | Blocking API; includes softAP lifecycle.         |
| `mqtt`         | MQTT client with builder and automatic reconnection            | `mqtt`, `std`           | All          | Requires `wifi` for bootstrap (compile-checked). |
| `lora`         | SX1262 radio driver, LoRaWAN OTAA/ADR support                  | `lora`                  | All          | Requires `sx126x` and `lorawan-device` crates.   |
| `espnow`       | ESP-NOW peer-to-peer messaging, broadcast mode                 | `espnow`                | All          | No Wi-Fi required; works in AP or isolation.     |
| `ota`          | Over-the-air firmware update (streaming, SHA-256 verified)     | `ota`                   | All          | Blocking API; downloads firmware from HTTPS.     |
| `provisioning` | SoftAP captive-portal Wi-Fi and device credential provisioning | `provisioning`          | All          | Requires `wifi` feature; NVS credential store.   |

**Special notes:**

- All features use **blocking APIs** — no async/await.
- `mqtt` and `provisioning` implicitly depend on `wifi` for bootstrap (compile error if `wifi` is omitted).
- Dual SPI support via `esp-idf-hal` abstractions; no board-specific SPI routing code needed.

## Cargo.toml

Add to your `Cargo.toml`:

```toml
[dependencies]
rustyfarian-esp-idf-network = { version = "0.4", features = ["wifi", "mqtt"] }
```

## Example: Wi-Fi + MQTT

```rust
use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig};
use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig};
use esp_idf_svc::mqtt::client::QoS;

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
    .on_connect(|client, _clean_session| {
        client.subscribe("commands/#", QoS::AtMostOnce)?;
        Ok(())
    })
    .on_message(|topic, data| {
        println!("Message on {}: {:?}", topic, data);
    })
    .build()?;

mqtt.publish_with("status", b"online", QoS::AtMostOnce, false)?;
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

## Integration with juggler

This crate re-exports all domain modules from `juggler`, so you can import validation logic and types directly:

```rust
use rustyfarian_esp_idf_network::wifi::WiFiConfig;
use rustyfarian_esp_idf_network::mqtt::MqttConnectionState;
```

## Resources

- [Workspace repository](https://github.com/datenkollektiv/rustyfarian-network)
- [ADR 016: Crate Consolidation for Publishing](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/016-crate-consolidation-for-publishing.md)
- [Feature doc: 3-Crate Consolidation v1](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/features/crate-consolidation-3-crates-v1.md)

## License

MIT or Apache-2.0
