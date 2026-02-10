# ADR 001: Rename `esp32-*` Crates to `rustyfarian-esp-idf-*`

## Status

Accepted

## Context

The workspace contains two crates originally named `esp32-wifi-manager` and `esp32-mqtt-manager`.
Both crates depend on `esp-idf-svc` and use ESP-IDF APIs that work across all ESP32 variants (ESP32, ESP32-S2, ESP32-S3, ESP32-C3, ESP32-C6, etc.).

Two naming problems exist:

**The `esp32-` prefix is inaccurate.**
It implies these crates are specific to the original ESP32 chip.
In practice, any chip supported by ESP-IDF can use them.

**The `esp-idf-*` prefix risks namespace confusion.**
The esp-rs organization maintains official crates like `esp-idf-hal`, `esp-idf-svc`, and `esp-idf-sys`.
Using the same `esp-idf-` prefix for third-party crates could confuse users into thinking these are official, or cause namespace conflicts if esp-rs later publishes crates with similar names.
This mirrors unwritten community norms seen with `tokio-*`, `serde-*`, and `embassy-*` prefixes.

The sister repository [rustyfarian-ws2812](https://github.com/datenkollektiv/rustyfarian-ws2812) solved both problems by adopting a `rustyfarian-{framework}-{feature}` convention (see [ADR 005: Dual-HAL Strategy](https://github.com/datenkollektiv/rustyfarian-ws2812/blob/main/docs/adr/005-dual-hal-strategy.md)):

| Old                  | New                          |
|:---------------------|:-----------------------------|
| `esp-idf-ws2812-rmt` | `rustyfarian-esp-idf-ws2812` |
| `esp-hal-ws2812-rmt` | `rustyfarian-esp-hal-ws2812` |

The `-rmt` suffix was dropped as an implementation detail (kept in keywords).

## Decision

Rename both crates using the `rustyfarian-esp-idf-*` convention, dropping the `-manager` suffix (implementation detail, kept in keywords):

| Before               | After                      | Snake-case                 |
|:---------------------|:---------------------------|:---------------------------|
| `esp32-wifi-manager` | `rustyfarian-esp-idf-wifi` | `rustyfarian_esp_idf_wifi` |
| `esp32-mqtt-manager` | `rustyfarian-esp-idf-mqtt` | `rustyfarian_esp_idf_mqtt` |

## Rationale

### Arguments for `rustyfarian-esp-idf-*`

| Factor                     | Analysis                                                                                        |
|:---------------------------|:------------------------------------------------------------------------------------------------|
| **Clear ownership**        | The `rustyfarian-` prefix signals this is a community/project crate, not an official esp-rs one |
| **Accuracy**               | The `esp-idf` infix correctly identifies the framework dependency                               |
| **Chip compatibility**     | No chip-specific prefix — ESP-IDF supports ESP32, C3, C6, S2, S3 and future variants            |
| **Cross-repo consistency** | Aligns with `rustyfarian-ws2812` which uses `rustyfarian-esp-idf-ws2812`                        |
| **Ecosystem respect**      | Avoids squatting on the `esp-idf-*` namespace used by official esp-rs crates                    |

### Arguments against

| Factor              | Analysis                                                                  |
|:--------------------|:--------------------------------------------------------------------------|
| **Longer names**    | `rustyfarian-esp-idf-wifi` is longer than `esp32-wifi-manager`            |
| **Breaking change** | Requires downstream updates, but project is young with minimal dependents |

## Consequences

### Positive

- **No namespace confusion**: Users and tools clearly distinguish these from official esp-rs crates
- **Brand identity**: The `rustyfarian-` prefix builds project identity across repositories
- **Future-proof**: New ESP32 variants are automatically implied as supported
- **Discoverability**: Keywords in `Cargo.toml` still include `esp-idf` for search

### Negative

- **Breaking change**: Downstream projects must update their `Cargo.toml` and `use` statements
- **Verbose imports**: `use rustyfarian_esp_idf_wifi::...` is longer than `use esp32_wifi_manager::...`

### Migration

Downstream projects update:

```toml
[dependencies]
rustyfarian-esp-idf-wifi = { git = "https://github.com/datenkollektiv/rustyfarian-network" }
rustyfarian-esp-idf-mqtt = { git = "https://github.com/datenkollektiv/rustyfarian-network" }
```

```rust
use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig};
use rustyfarian_esp_idf_mqtt::{MqttManager, MqttConfig};
```
