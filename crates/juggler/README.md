# juggler

Platform-agnostic types, validation, and state machines for Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning on ESP32.

`juggler` is a pure `no_std` crate that holds the domain logic for all wireless protocols in the rustyfarian-network family.
It compiles on any platform (macOS, Linux, Windows) and is fully unit-testable without ESP hardware — enabling rapid prototyping and confident refactoring.

**Tier:** Pure (no hardware dependencies)

## Features

Domain features are opt-in; `default = []` means you explicitly declare which domains you need:

| Feature        | What it gates                                                                  | Dependencies                       | Notes                                                            |
|:---------------|:-------------------------------------------------------------------------------|:-----------------------------------|:-----------------------------------------------------------------|
| `wifi`         | Wi-Fi configuration, STA/AP state machines                                     | (none)                             | Core validation and connection logic.                            |
| `mqtt`         | MQTT state machine, connection state, QoS handling                             | (none)                             | `std` feature adds `spawn_subscriber_thread`, `SubscribeClient`. |
| `lora`         | LoRa/LoRaWAN types, coding rates, spreading factors, device state machine      | `heapless`, `nb`, `lorawan-device` | Radio-agnostic LoRaWAN join and TX/RX state.                     |
| `espnow`       | ESP-NOW frame types, MAC address validation                                    | (none)                             | Peer-to-peer frame abstraction.                                  |
| `ota`          | OTA manifest parsing, firmware update state machine                            | `heapless`, `sha2`                 | Partition-agnostic update orchestration.                         |
| `provisioning` | Provisioning schema profiles, field validators, credential storage abstraction | `heapless`                         | Enables: `wifi`, `mqtt`, `lora`                                  |
| `mock`         | Test doubles for radio and MQTT drivers                                        | (alloc)                            | Host-testing feature only; never shipped.                        |
| `std`          | MQTT helper traits: `spawn_subscriber_thread`, `SubscribeClient`, `QoS`        | `anyhow`                           | Host-only; requires `std::thread` and `std::sync`.               |

**Special features:**

- **`provisioning`:** a meta-feature that enables `wifi`, `mqtt`, and `lora` (they are prerequisites for captive-portal profiles).
- **`std`:** optional support for the standard library, used to implement blocking MQTT subscriber threads. Gated on `#[cfg(feature = "std")]` inside the `mqtt` module; does NOT depend on any HAL.
- **`mock`:** test-double implementations for host-side unit tests. Never included in release builds; filtered by `cargo publish --dry-run`.

## Cargo.toml

Add to your `Cargo.toml`:

```toml
[dependencies]
juggler = { version = "0.4", features = ["wifi", "mqtt"] }
```

To compile for your target (ESP32, bare-metal, or host), no additional configuration is needed — the pure crate runs everywhere.

## Example: Validating Wi-Fi Configuration

```rust
use juggler::wifi::{WiFiConfig, SecurityMode};

let config = WiFiConfig::new("MyNetwork", "password123")
    .with_security(SecurityMode::Wpa2);

match config.validate() {
    Ok(()) => println!("Config is valid"),
    Err(e) => eprintln!("Config error: {}", e),
}
```

## Example: MQTT Connection State Machine

```rust
use juggler::mqtt::MqttConnectionState;

let mut state = MqttConnectionState::Disconnected;
state = state.transition_connecting();
assert_eq!(state, MqttConnectionState::Connecting);

state = state.transition_connected();
assert_eq!(state, MqttConnectionState::Connected);
```

## Testing

Compile and run all pure-crate unit tests on your host (no ESP toolchain required):

```sh
cargo test -p juggler --all-features
```

To test a specific domain:

```sh
cargo test -p juggler --features wifi,mqtt
```

## Integration with HAL Crates

`juggler` is re-exported by the two hardware tiers:

- [`rustyfarian-esp-idf-network`](../rustyfarian-esp-idf-network) — ESP-IDF drivers (std)
- [`rustyfarian-esp-hal-network`](../rustyfarian-esp-hal-network) — Bare-metal `esp-hal` drivers (async)

Downstream consumers typically depend on one of the HAL crates, which pull `juggler` as a transitive dependency.
If you need the pure types directly (e.g., for cross-platform validation logic), depend on `juggler` explicitly.

## Resources

- [Workspace repository](https://github.com/datenkollektiv/rustyfarian-network)
- [ADR 016: Crate Consolidation for Publishing](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/016-crate-consolidation-for-publishing.md)
- [Feature doc: 3-Crate Consolidation v1](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/features/archive/crate-consolidation-3-crates-v1.md)

## License

MIT or Apache-2.0
