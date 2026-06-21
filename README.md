# Rustyfarian Network

[![CI](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/rust.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-esp--toolchain-orange.svg)](https://github.com/esp-rs/rust)
[![cargo fmt](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/fmt.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/fmt.yml)
[![cargo clippy](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/clippy.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/clippy.yml)
[![cargo audit](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/audit.yml/badge.svg)](https://github.com/datenkollektiv/rustyfarian-network/actions/workflows/audit.yml)

Wi-Fi, MQTT, LoRa, ESP-NOW, and OTA support libraries for ESP32 projects.

> Note: Large parts of this library (and documentation) were developed with the assistance of AI tools.
> All generated code has been reviewed and curated by the maintainer.

## Vision

> Any ESP32-IDF project can add Wi-Fi and MQTT in minutes, with confidence.

**We are building this for:** ESP32-IDF Rust firmware developers — internal projects first, clean enough for anyone to adopt.

**Long-term goals:**
- Reliable, thin hardware wrappers that stay focused on connectivity — nothing more
- A platform-independent layer (`juggler`) with per-domain features, unit-tested on the host — all tiers consolidated and feature-gated (Phases 1–3 complete)
- Minimal friction: a few lines of `Cargo.toml` and no surprises

**Out of scope:** General-purpose application-layer clients (HTTP, CoAP, WebSocket) and BLE provisioning flows.
The OTA feature (in both `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`) carries its own internal HTTP/1.1 GET client for firmware download, but this is an implementation detail and not published as a reusable workspace HTTP API.
SoftAP captive-portal provisioning ships with two `SchemaProfile`s — `LorawanFieldDevice` and `WifiMqttDevice` — under [ADR 013](docs/adr/013-softap-provisioning-acceptance.md) (acceptance, 2026-06-11) and [ADR 014](docs/adr/014-wifi-mqtt-provisioning-profile.md) (Wi-Fi + MQTT generalisation, 2026-06-12); the captive-portal HTTP server follows the same internal-transport pattern as the OTA client.

*Full vision, success signals, and open questions: [VISION.md](./VISION.md)*

## Rustyfarian Philosophy

This library embodies the principle of **extracting testable pure logic from hardware-specific code** —
a pattern common in application development but rare in embedded Rust.

- Pure logic belongs in `juggler` — a consolidated, feature-gated, platform-independent crate with no ESP-IDF or hardware dependency
- Examples: SSID and password validation, connection timeout arithmetic, backoff calculations, MQTT state machines
- If you can unit-test it without hardware, it should be in `juggler` (feature-gated by domain: `wifi`, `mqtt`, `lora`, etc.)
- The ESP-IDF and esp-hal wrappers are thin layers that delegate logic downward and handle the hardware lifecycle

`juggler` can be fully unit-tested on your laptop without an ESP32 or ESP toolchain (via `just test`).

## Crates

**Three publishable crates — one per HAL tier, all with feature-gated domain selection:**

| Crate                                                               | Tier               | Description                                                                  | crates.io                                                                                                                             | Docs                                                                                                               |
|:--------------------------------------------------------------------|:-------------------|:-----------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------------------|
| [`juggler`](crates/juggler)                                         | Pure (no_std)      | Platform-independent types, validation, state machines — fully host-testable | [![crates.io](https://img.shields.io/crates/v/juggler.svg)](https://crates.io/crates/juggler)                                         | [![docs.rs](https://img.shields.io/docsrs/juggler)](https://docs.rs/juggler)                                          |
| [`rustyfarian-esp-idf-network`](crates/rustyfarian-esp-idf-network) | ESP-IDF (std)      | ESP-IDF drivers with blocking APIs and LED status feedback                   | [![crates.io](https://img.shields.io/crates/v/rustyfarian-esp-idf-network.svg)](https://crates.io/crates/rustyfarian-esp-idf-network) | [![readme](https://img.shields.io/badge/docs-readme-blue)](crates/rustyfarian-esp-idf-network/README.md)              |
| [`rustyfarian-esp-hal-network`](crates/rustyfarian-esp-hal-network) | Bare-metal (async) | Bare-metal `esp-hal` drivers with async/await via `embassy`                  | [![crates.io](https://img.shields.io/crates/v/rustyfarian-esp-hal-network.svg)](https://crates.io/crates/rustyfarian-esp-hal-network) | [![docs.rs](https://img.shields.io/docsrs/rustyfarian-esp-hal-network)](https://docs.rs/rustyfarian-esp-hal-network)  |

> Domain features (`wifi`, `mqtt`, `lora`, `espnow`, `ota`, `provisioning`) and chip features (`esp32c3`/`c6`/`s3`/`esp32`) are selected per dependency; `default = []`.
> `rustyfarian-esp-idf-network` links to its crate README rather than docs.rs — `esp-idf-sys` cannot build in the docs.rs sandbox.

**See each crate's README for complete feature tables and usage examples.**
**For migration from the old per-domain crates, see the [Migration Guide](docs/features/archive/crate-consolidation-3-crates-v1.md#migration-guide--old-paths-to-new-paths).**

## Usage

Choose one of the three crates based on your target platform and add it with the domains you need:

**ESP-IDF (std, FreeRTOS):**

```toml
[dependencies]
rustyfarian-esp-idf-network = { version = "0.4", features = ["wifi", "mqtt"] }
```

**Bare-metal (no_std, async with esp-hal):**

```toml
[dependencies]
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "esp32c6", "embassy", "rt"] }
```

**Pure logic only (host-testable):**

```toml
[dependencies]
juggler = { version = "0.4", features = ["wifi", "mqtt"] }
```

## Example

```rust
use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig};
use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig};
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
use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig, LwtConfig};
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
use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig};
use pennant::SimpleLed;
use esp_idf_hal::gpio::PinDriver;

let pin = PinDriver::output(peripherals.pins.gpio8)?;
let mut led = SimpleLed::new(pin);

let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), wifi_config, Some(&mut led))?;
```

For RGB LEDs, implement the `StatusLed` trait from `pennant`.

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

### Build target isolation (optional RAM disk)

Bare-metal (`esp-hal`) and ESP-IDF (`std`) builds use separate target directories —
`target/hal` and `target/idf` — because their artefacts are incompatible and a shared
`target/` forces a full rebuild on every switch between the two.
Host and IDE builds use `target/ide`.
This isolation is always active and needs no setup.

On macOS you can optionally back the embedded target directories with a RAM disk for
faster, SSD-sparing builds:

```sh
just doctor           # show RAM disk status, resolved target dirs, and sccache
just ramdisk attach   # create and mount the RAM disk (idempotent, 6 GB default)
just ramdisk detach   # eject the RAM disk
```

When the RAM disk is detached, builds fall back to `target/hal` / `target/idf` on disk —
isolation is preserved, builds are just slower.

## License

MIT or Apache-2.0
