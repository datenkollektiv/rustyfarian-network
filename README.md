# Rustyfarian Network Related Crates

[![CI](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.77%2B-orange.svg)](https://www.rust-lang.org)
[![Last Commit](https://img.shields.io/github/last-commit/datenkollektiv/rustyfarian-network)](https://github.com/datenkollektiv/rustyfarian-network/commits/)

Wi-Fi and MQTT networking libraries for ESP32 projects using ESP-IDF.

> Note: Parts of this library were developed with the assistance of AI tools.
> All generated code has been reviewed and curated by the maintainer.

## Crates

| Crate                                                               | Description                                                   |
|:--------------------------------------------------------------------|:--------------------------------------------------------------|
| [`rustyfarian-esp-idf-wifi`](crates/rustyfarian-esp-idf-wifi) | Wi-Fi connection manager with LED status feedback             |
| [`rustyfarian-esp-idf-mqtt`](crates/rustyfarian-esp-idf-mqtt) | MQTT client with automatic reconnection and graceful shutdown |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
rustyfarian-esp-idf-wifi = { git = "https://github.com/datenkollektiv/rustyfarian-network" }
rustyfarian-esp-idf-mqtt = { git = "https://github.com/datenkollektiv/rustyfarian-network" }
```

## Example

```rust
use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig};
use rustyfarian_esp_idf_mqtt::{MqttManager, MqttConfig};

// Connect to WiFi
let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), wifi_config, None::<&mut MyLed>)?;

// Wait for IP
if let Some(ip) = wifi.get_ip(10000)? {
    println!("Connected with IP: {}", ip);
}

// Connect to MQTT
let mqtt_config = MqttConfig::new("192.168.1.100", 1883, "my-device");
let mqtt = MqttManager::new(mqtt_config, "commands", |data| {
    println!("Received: {:?}", data);
})?;

mqtt.publish("status", b"online")?;
```

## LED Status Feedback

The Wi-Fi manager supports optional LED status feedback during connection.
For boards with a simple on/off LED (not RGB), use `SimpleLed`:

```rust
use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig, SimpleLed};
use esp_idf_hal::gpio::PinDriver;

let pin = PinDriver::output(peripherals.pins.gpio8)?;
let mut led = SimpleLed::new(pin);

let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), wifi_config, Some(&mut led))?;
```

For RGB LEDs, implement the `StatusLed` trait from `led-effects`.

## License

MIT or Apache-2.0
