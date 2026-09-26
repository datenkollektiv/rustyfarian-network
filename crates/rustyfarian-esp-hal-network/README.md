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
| `wifi`         | Async Wi-Fi STA/AP via `esp-radio 1.0.0-beta.1`           | Yes                | Implies `embassy`; auto-enables it.                |
| `lora`         | Synchronous LoRa radio stub (hardware driver in progress) | No                 | Non-async; blocking radio via embedded-hal.        |
| `ota`          | Async over-the-air firmware update                        | Yes                | Requires `embassy` + `provisioning` unsupported.   |
| `provisioning` | Async SoftAP captive-portal provisioning                  | Yes                | Requires `wifi` + `embassy`; NVS storage (RISC-V). |

### Dependency pins and MSRV

Exact pins are public API: `esp-hal =1.2.2`, `esp-rtos =0.4.0`, `esp-radio =1.0.0-beta.1` (pre-release; re-pins to `1.0.0` on release), `esp-alloc =0.11.0`, `esp-bootloader-esp-idf =0.6.0`, `esp-storage =0.10.0`, `esp-println =0.18.0`, `esp-backtrace =0.20.0`, Embassy `0.10`/`0.8.0`/`0.8.0`/`0.5.1`.
Minimum Rust is 1.95.

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

Condensed from `examples/hal_c3_connect_async.rs`.
`init_async` is synchronous: it starts the `esp-rtos` scheduler, configures the radio, and builds the `embassy-net` stack, but does not associate.
The caller spawns one task that owns the `WifiController` (association and reconnects) and one that drives the network `Runner`; `stack` is `Copy` and is used for sockets.

```rust
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_network::wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let config = WiFiConfig::new("MyNetwork", "password123").with_peripherals(
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
        peripherals.WIFI,
    );
    let AsyncWifiHandle { controller, stack, runner } =
        WiFiManager::init_async(config).expect("Wi-Fi init failed");

    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    stack.wait_config_up().await;
    let v4 = stack.config_v4().expect("no IPv4 config");
    esp_println::println!("connected, IP {}", v4.address);
}

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                let _ = controller.wait_for_disconnect_async().await;
                Timer::after(Duration::from_millis(500)).await;
            }
            Err(_) => Timer::after(Duration::from_secs(5)).await,
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
```

The ESP32-C6 needs two heap regions (a 64 KiB `#[esp_hal::ram(reclaimed)]` region for the radio plus 36 KiB of DRAM); see `examples/hal_c6_connect_async_led.rs`.

## Example: Async SoftAP + Provisioning

Condensed from `examples/hal_c3_provision_mqtt.rs`.
The SoftAP is brought up through `init_softap_async`, the credential store lives in a dedicated flash partition, and `ProvisioningBuilder::start` spawns the DHCP, DNS and HTTP tasks.
The library never reboots or erases on its own; the host acts on the `ProvisioningOutcome`.

```rust
use embassy_executor::Spawner;
use rustyfarian_esp_hal_network::provisioning::{
    PortalConfig, PortalDefaults, ProvisioningBuilder, ProvisioningOutcome, ProvisioningStore,
    SchemaProfile,
};
use rustyfarian_esp_hal_network::wifi::{ApConfig, ApConfigExt, WiFiManager};

const FLASH_PARTITION_OFFSET: u32 = 0x300000;
const FLASH_PARTITION_SIZE: u32 = 8192;

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let store = ProvisioningStore::open(flash, FLASH_PARTITION_OFFSET, FLASH_PARTITION_SIZE)
        .expect("provisioning store open");

    let ap_config = ApConfig::open("rustyfarian").with_channel(1).with_ap_peripherals(
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
        peripherals.WIFI,
    );
    let softap = WiFiManager::init_softap_async(ap_config).expect("SoftAP init");
    let rng = esp_hal::rng::Rng::new();

    let portal_config = PortalConfig {
        ssid_prefix: "rustyfarian",
        ssid_override: None,
        ap_password: None,
        channel: 1,
        device_name: "field-device",
        firmware_version: env!("CARGO_PKG_VERSION"),
        profile: SchemaProfile::WifiMqttDevice,
        defaults: PortalDefaults::default(),
    };

    let session = ProvisioningBuilder::new(portal_config)
        .start(spawner, softap, store, rng)
        .expect("provisioning start");

    match session.wait_outcome().await {
        ProvisioningOutcome::Committed(cfg) => {
            esp_println::println!("committed for SSID of {} bytes", cfg.wifi_ssid().len());
        }
        ProvisioningOutcome::FactoryResetRequested => {}
        ProvisioningOutcome::HostAborted => {}
    }
}
```

The already-provisioned boot path must not `.await` before Wi-Fi is up: `esp_rtos::start` runs inside `init_async` / `init_softap_async`, and the executor panics on its first park without it.
The full example shows the `is_provisioned()` branch, the `on_event` hook, and the build-time-seeded `PortalDefaults`.

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
