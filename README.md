# Rustyfarian Network Related Crates

Wi-Fi and MQTT networking libraries for ESP32 projects using ESP-IDF.

> Note: Parts of this library were developed with the assistance of AI tools.
> All generated code has been reviewed and curated by the maintainer.

## Crates

| Crate                                             | Description                                                   |
|:--------------------------------------------------|:--------------------------------------------------------------|
| [`esp32-wifi-manager`](crates/esp32-wifi-manager) | Wi-Fi connection manager with LED status feedback             |
| [`esp32-mqtt-manager`](crates/esp32-mqtt-manager) | MQTT client with automatic reconnection and graceful shutdown |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
esp32-wifi-manager = { git = "https://github.com/datenkollektiv/esp32-network" }
esp32-mqtt-manager = { git = "https://github.com/datenkollektiv/esp32-network" }
```

## Example

```rust
use esp32_wifi_manager::{WiFiManager, WiFiConfig};
use esp32_mqtt_manager::{MqttManager, MqttConfig};

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
use esp32_wifi_manager::{WiFiManager, WiFiConfig, SimpleLed};
use esp_idf_hal::gpio::PinDriver;

let pin = PinDriver::output(peripherals.pins.gpio8)?;
let mut led = SimpleLed::new(pin);

let wifi_config = WiFiConfig::new("MyNetwork", "password123");
let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), wifi_config, Some(&mut led))?;
```

For RGB LEDs, implement the `StatusLed` trait from `led-effects`.

## License

MIT or Apache-2.0
