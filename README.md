# Rustyfarian Network Related Crates

[![CI](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-esp--toolchain-orange.svg)](https://github.com/esp-rs/rust)
[![Last Commit](https://img.shields.io/github/last-commit/datenkollektiv/rustyfarian-network)](https://github.com/datenkollektiv/rustyfarian-network/commits/)

Wi-Fi and MQTT networking libraries for ESP32 projects using ESP-IDF.

> Note: Large parts of this library (and documentation) were developed with the assistance of AI tools.
> All generated code has been reviewed and curated by the maintainer.

## Vision

> Any ESP32-IDF project can add Wi-Fi and MQTT in minutes, with confidence.

**We are building this for:** ESP32-IDF Rust firmware developers — internal projects first, clean enough for anyone to adopt.

**Long-term goals:**
- Reliable, thin hardware wrappers that stay focused on connectivity — nothing more
- A growing platform-independent layer (`rustyfarian-network-pure`) that can be unit-tested on the host
- Minimal friction: a few lines of `Cargo.toml` and no surprises

**Out of scope:** Application-layer protocols (HTTP, CoAP, WebSocket) and provisioning/SoftAP flows.

*Full vision, success signals, and open questions: [VISION.md](./VISION.md)*

## Rustyfarian Philosophy

This library embodies the principle of **extracting testable pure logic from hardware-specific code** —
a pattern common in application development but rare in embedded Rust.

- Pure functions belong in `rustyfarian-network-pure` — a platform-independent crate with no ESP-IDF dependency
- Examples: SSID and password validation, connection timeout arithmetic, backoff calculations
- If you can unit-test it without hardware, it should be in `rustyfarian-network-pure`
- The ESP-IDF wrappers (`rustyfarian-esp-idf-wifi`, `rustyfarian-esp-idf-mqtt`) are thin layers that delegate logic downward and handle the hardware lifecycle

`rustyfarian-network-pure` can be fully unit-tested on your laptop without an ESP32 or ESP toolchain.

## Crates

| Crate                                                         | Description                                                                               |
|:--------------------------------------------------------------|:------------------------------------------------------------------------------------------|
| [`rustyfarian-network-pure`](crates/rustyfarian-network-pure) | Platform-independent primitives — validation, timing math; unit-testable on the host      |
| [`wifi-pure`](crates/wifi-pure)                               | Platform-independent Wi-Fi types, traits, and validation; `no_std`; unit-testable on host |
| [`rustyfarian-esp-idf-wifi`](crates/rustyfarian-esp-idf-wifi) | Wi-Fi connection manager with LED status feedback                                         |
| [`rustyfarian-esp-hal-wifi`](crates/rustyfarian-esp-hal-wifi) | Wi-Fi driver stub for bare-metal `esp-hal` targets; full implementation in progress       |
| [`rustyfarian-esp-idf-mqtt`](crates/rustyfarian-esp-idf-mqtt) | MQTT client with automatic reconnection and graceful shutdown                             |
| [`lora-pure`](crates/lora-pure)                               | Platform-independent LoRa/LoRaWAN types and traits; `no_std`; unit-testable on host       |
| [`rustyfarian-esp-idf-lora`](crates/rustyfarian-esp-idf-lora) | LoRa radio driver (SX1262) and LoRaWAN adapter for ESP-IDF targets                        |
| [`rustyfarian-esp-hal-lora`](crates/rustyfarian-esp-hal-lora) | LoRa radio stub for bare-metal `esp-hal` targets; hardware driver in progress             |

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
use rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttConfig};
use esp_idf_svc::mqtt::client::QoS;

let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), wifi_config, None::<&mut MyLed>)?;

if let Some(ip) = wifi.get_ip(10000)? {
    println!("Connected with IP: {}", ip);
}

let mqtt_config = MqttConfig::new("192.168.1.100", 1883, "my-device");
let mqtt = MqttBuilder::new(mqtt_config)
    .on_connect(|client, _clean_session| {
        client.subscribe("commands", QoS::AtMostOnce)?;
        Ok(())
    })
    .on_message(|topic, data| {
        println!("Received on {}: {:?}", topic, data);
    })
    .build()?;

mqtt.publish_with("status", b"online", QoS::AtMostOnce, false)?;
```

### LWT and Retained Messages

```rust
use rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttConfig, LwtConfig};
use esp_idf_svc::mqtt::client::QoS;

let lwt = LwtConfig::new("device/status", b"offline", QoS::AtLeastOnce, true);
let mqtt_config = MqttConfig::new("192.168.1.100", 1883, "my-device")
    .with_lwt(lwt);

let mqtt = MqttBuilder::new(mqtt_config)
    .on_connect(|client, _clean_session| {
        client.subscribe("commands", QoS::AtMostOnce)?;
        Ok(())
    })
    .on_message(|topic, data| {
        println!("Received on {}: {:?}", topic, data);
    })
    .build()?;

mqtt.publish_with("device/status", b"online", QoS::AtLeastOnce, true)?;
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

## Hardware Examples

Each crate includes runnable examples for specific ESP32 targets.
List all examples with `just` and build one with:

```sh
just build-example idf_c3_connect
```

To flash to a connected board:

```sh
just flash idf_c3_connect
```

See `crates/*/examples/` for the full set, including Wi-Fi, MQTT, and LoRaWAN OTAA join demos.

## Development Setup

After cloning, run the one-time setup before building or running examples:

```sh
just setup-toolchain
just setup-cargo-config
```

`setup-cargo-config` copies `.cargo/config.toml.dist` to `.cargo/config.toml`, which
configures linker settings and target-specific flags for ESP32, ESP32-S3, and bare-metal
Xtensa targets.
Without this step, builds for those targets will fail with linker or toolchain errors.

## License

MIT or Apache-2.0
