# rustyfarian-esp-hal-network

Bare-metal (esp-hal) async drivers for Wi-Fi, LoRa, OTA, and provisioning on ESP32-C3, ESP32-C6, ESP32-S3, and ESP32.

This crate provides `no_std` async implementations for embedded projects targeting bare-metal ESP32 variants.
It uses `embassy-executor` for async task scheduling and `embassy-net` for TCP/IP.

**Tier:** Bare-metal `esp-hal` (no_std, async-only)

## Features

Domain and chip features are **independently opt-in**; `default = []` means you explicitly declare both domains and target chip:

### Domain Features

| Feature        | What it gates                                             | Requires `embassy` | Notes                                              |
|:---------------|:----------------------------------------------------------|:-------------------|:---------------------------------------------------|
| `wifi`         | Async Wi-Fi STA/AP via `esp-radio 0.18`                   | Yes                | Implies `embassy`; auto-enables it.                |
| `lora`         | Synchronous LoRa radio stub (hardware driver in progress) | No                 | Non-async; blocking radio via embedded-hal.        |
| `ota`          | Async over-the-air firmware update                        | Yes                | Requires `embassy` + `provisioning` unsupported.   |
| `provisioning` | Async SoftAP captive-portal provisioning                  | Yes                | Requires `wifi` + `embassy`; NVS storage (RISC-V). |

### Chip Features

Select **exactly one** of the following:

| Feature   | Target            | Compiles                                                                     |
|:----------|:------------------|:-----------------------------------------------------------------------------|
| `esp32c3` | ESP32-C3 (RISC-V) | All domains ✓                                                                |
| `esp32c6` | ESP32-C6 (RISC-V) | All domains ✓                                                                |
| `esp32s3` | ESP32-S3 (Xtensa) | `lora` only (no Wi-Fi, no OTA); compile error if `wifi` or `ota` are enabled |
| `esp32`   | ESP32 (Xtensa)    | `lora` only (no Wi-Fi, no OTA); compile error if `wifi` or `ota` are enabled |

### Support Features

| Feature    | What it gates                                      | Notes                                                   |
|:-----------|:---------------------------------------------------|:--------------------------------------------------------|
| `embassy`  | Async executor, network stack, timers              | Required for `wifi`, `ota`, `provisioning`.             |
| `unstable` | `esp-hal` unstable features (GPIO, SPI access)     | Used internally; rarely needed by consumers.            |
| `rt`       | Runtime startup / reset handler                    | Needed for all examples.                                |
| `ws2812`   | RGB LED support (via `rustyfarian-esp-hal-ws2812`) | Optional; gates the `hal_c6_connect_async_led` example. |

## Cargo.toml

Add to your `Cargo.toml`:

```toml
[dependencies]
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "esp32c6", "embassy", "rt"] }
```

**Chip + domain matrix examples:**

```toml
# Wi-Fi on ESP32-C3
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "esp32c3", "embassy", "rt"] }

# LoRa on ESP32-S3 (no async needed)
rustyfarian-esp-hal-network = { version = "0.4", features = ["lora", "esp32s3", "rt"] }

# Wi-Fi + OTA on ESP32-C6
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "ota", "esp32c6", "embassy", "rt"] }

# SoftAP provisioning on ESP32-C3
rustyfarian-esp-hal-network = { version = "0.4", features = ["provisioning", "esp32c3", "embassy", "rt"] }
```

## Example: Async Wi-Fi Connect

```rust
use rustyfarian_esp_hal_network::wifi::WiFiManager;
use rustyfarian_esp_hal_network::wifi::WiFiConfig;

#[embassy_executor::task]
async fn wifi_task(
    peripherals: esp_hal::peripherals::Peripherals,
) {
    let mut wifi = WiFiManager::init_async(
        peripherals.modem,
        peripherals.radio_clock_control,
        WiFiConfig::new("MyNetwork", "password123"),
    ).await.expect("Wi-Fi init failed");

    loop {
        if wifi.is_connected().await {
            println!("Connected!");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}
```

## Example: Async SoftAP + Provisioning

```rust
use rustyfarian_esp_hal_network::provisioning::ProvisioningPortal;
use rustyfarian_esp_hal_network::provisioning::SchemaProfile;

let portal = ProvisioningPortal::new(
    SchemaProfile::WifiMqttDevice,
).await?;

loop {
    match portal.poll().await {
        Ok(Some(credentials)) => {
            println!("Received: {:?}", credentials);
            break;
        }
        Ok(None) => { /* still polling */ }
        Err(e) => eprintln!("Portal error: {}", e),
    }
}
```

## Integration with juggler

This crate re-exports all domain modules from `juggler`, so you can import validation logic and types directly:

```rust
use rustyfarian_esp_hal_network::wifi::WiFiConfig;
use rustyfarian_esp_hal_network::lora::LoraConfig;
```

## Chip Support Caveats

- **ESP32** (Xtensa LX6): LoRa-only; `esp-radio` (Wi-Fi) does not support this chip.
- **ESP32-S3** (Xtensa LX7): LoRa-only; `esp-storage` does not support S3, so OTA is unavailable.
- **ESP32-C3 and ESP32-C6** (RISC-V): Full support for Wi-Fi, LoRa, OTA, and provisioning.

Attempting to enable `wifi` or `ota` with `esp32` or `esp32s3` will fail at compile time with a `compile_error!` diagnostic.

## docs.rs Build Note

This crate targets bare-metal ESP32 chips and does not build documentation on docs.rs (which uses a POSIX Linux build environment by default).
Documentation is available in the [workspace repository](https://github.com/datenkollektiv/rustyfarian-network/tree/main/crates/rustyfarian-esp-hal-network) and via `cargo doc --open` on your local development machine.

## Resources

- [Workspace repository](https://github.com/datenkollektiv/rustyfarian-network)
- [ADR 016: Crate Consolidation for Publishing](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/016-crate-consolidation-for-publishing.md)
- [Feature doc: 3-Crate Consolidation v1](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/features/archive/crate-consolidation-3-crates-v1.md)

## License

MIT or Apache-2.0
