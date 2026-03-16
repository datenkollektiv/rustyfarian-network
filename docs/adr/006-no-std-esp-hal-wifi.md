# ADR 006: Extend Wi-Fi Support to bare-metal (esp-hal) Targets

## Status

Accepted

## Context

The `rustyfarian-network` workspace provides Wi-Fi connectivity exclusively through `rustyfarian-esp-idf-wifi`, which depends on `esp-idf-svc` (std, FreeRTOS).
The dual-HAL pattern established in ADR 005 and proven by the LoRa tier (ADR 004) demonstrates that separate crates per HAL — with a shared `*-pure` crate for platform-independent logic — is the correct approach for this workspace.

Several factors motivate extending the pattern to Wi-Fi:

- **`esp-wifi 0.14.0`** (Dec 2024) provides production-ready bare-metal Wi-Fi for ESP32-C3 and ESP32-C6, compatible with `esp-hal 1.0.0` (already in the workspace).
  It bundles `smoltcp 0.11.0` for the TCP/IP stack.
- **Extractable pure logic exists** — `WiFiConfig`, `ConnectMode`, `DEFAULT_TIMEOUT_SECS`, `POLL_INTERVAL_MS`, and `wifi_disconnect_reason_name` are platform-independent and currently locked inside the ESP-IDF crate.
  SSID/password validation previously lived in `rustyfarian-network-pure::wifi` and has been moved to `wifi-pure`.
- **The LoRa path is blocked on hardware** — the LoRa radio for TTN v3 validation is unavailable, making the Wi-Fi dual-HAL tier the highest-value near-term work.

The LoRa tier's three-crate split (`lora-pure`, `rustyfarian-esp-idf-lora`, `rustyfarian-esp-hal-lora`) serves as a direct template.

## Decision

Extend Wi-Fi support to bare-metal targets via **separate crates**, following the dual-HAL convention (ADR 005).

The structure will be:

```
wifi-pure                        — no_std; WiFiConfig, ConnectMode, WifiDriver trait, disconnect reason map
rustyfarian-esp-idf-wifi         — std; esp-idf-svc (existing, refactored to depend on wifi-pure)
rustyfarian-esp-hal-wifi         — no_std; esp-hal + esp-wifi 0.14.0; ESP32-C3/C6 bare-metal
```

### `wifi-pure` — shared platform-independent crate

`wifi-pure` is unconditionally `#![no_std]` and contains:

| Symbol                                                                   | Currently in                         | Move to                     |
|:-------------------------------------------------------------------------|:-------------------------------------|:----------------------------|
| `WiFiConfig<'a>`                                                         | `rustyfarian-esp-idf-wifi`           | `wifi-pure`                 |
| `ConnectMode`                                                            | `rustyfarian-esp-idf-wifi`           | `wifi-pure`                 |
| `DEFAULT_TIMEOUT_SECS`                                                   | `rustyfarian-esp-idf-wifi`           | `wifi-pure`                 |
| `POLL_INTERVAL_MS`                                                       | `rustyfarian-esp-idf-wifi`           | `wifi-pure`                 |
| `wifi_disconnect_reason_name`                                            | `rustyfarian-esp-idf-wifi` (private) | `wifi-pure` (pub, testable) |
| `SSID_MAX_LEN`, `PASSWORD_MAX_LEN`, `validate_ssid`, `validate_password` | `rustyfarian-network-pure::wifi` (removed) | `wifi-pure`                 |

A `mock` feature provides a test double, following the `lora-pure` convention.

### `WifiDriver` trait

The initial trait surface in `wifi-pure`:

```rust
pub trait WifiDriver {
    type Error: core::fmt::Debug;
    fn configure(&mut self, ssid: &str, password: &str) -> Result<(), Self::Error>;
    fn start(&mut self) -> Result<(), Self::Error>;
    fn connect(&mut self) -> Result<(), Self::Error>;
    fn is_connected(&self) -> Result<bool, Self::Error>;
    fn wait_netif_up(&mut self) -> Result<(), Self::Error>;
}
```

`get_ip` is intentionally excluded — IP address retrieval is ESP-IDF-specific (`sta_netif()`).
The trait surface is initial and expected to evolve during implementation.

### `rustyfarian-esp-hal-wifi` — bare-metal Wi-Fi crate

Chip features select the target:

| Feature   | Cargo target                   | MCU      |
|:----------|:-------------------------------|:---------|
| `esp32c3` | `riscv32imc-unknown-none-elf`  | ESP32-C3 |
| `esp32c6` | `riscv32imac-unknown-none-elf` | ESP32-C6 |

No default features — the stub compiles on the host without `esp-hal`.

Key dependencies:
- `esp-wifi 0.14.0` — bare-metal Wi-Fi driver
- `smoltcp 0.11.0` (transitive via `esp-wifi`) — `no_std` TCP/IP stack, `0BSD` licence

### `rustyfarian-esp-idf-wifi` — refactored existing crate

The existing crate gains a dependency on `wifi-pure` and re-exports its types via `pub use`.
`WiFiManager` and all `esp-idf-svc` types remain in this crate.
No breaking change for existing consumers.

### Phased implementation

1. This ADR (Phase 1)
2. Create `wifi-pure` skeleton with `WifiDriver` trait and moved types; update `rustyfarian-esp-idf-wifi` with `pub use` re-exports
3. Create `rustyfarian-esp-hal-wifi` stub (compile-only, `EspHalWifiManager` returns errors)
4. Add `check-wifi-pure`, `check-wifi-hal`, `test-wifi` to `justfile`; add bare-metal target blocks to `.cargo/config.toml.dist`
5. Implement full `EspHalWifiManager` using `esp-wifi 0.14.0` + `smoltcp`
6. Add `hal_c3_connect` and `hal_c6_connect` examples

## Consequences

### Positive

- **Bare-metal Wi-Fi becomes possible** — firmware can associate to an access point without ESP-IDF overhead
- **Testable Wi-Fi logic** — `wifi-pure` types and the disconnect reason map can be unit-tested on the host
- **Naming honesty** — crate names accurately describe which HAL they target (ADR 005)
- **Semver independence** — `esp-hal` breaking changes do not affect `esp-idf` version history
- **Code reuse** — `WiFiConfig`, `ConnectMode`, and validation logic serve both drivers without duplication
- **No breaking change** — existing `rustyfarian-esp-idf-wifi` consumers experience no disruption

### Negative / Risks

- **New licence** — `smoltcp 0.11.0` uses `0BSD`, which must be added to the `deny.toml` allow list
- **Refactoring scope** — extracting `wifi-pure` from `rustyfarian-esp-idf-wifi` and relocating validators from `rustyfarian-network-pure` required coordinated changes across three crates (completed; the `wifi` shim module has been removed)
- **Two drivers to maintain** — both `esp-idf` and `esp-hal` paths must be tested on actual hardware (mitigated by keeping drivers thin and delegating shared logic to `wifi-pure`)
- **Trait evolution** — the `WifiDriver` trait is initial; `esp-hal` implementation may reveal methods that need adding or signatures that need changing before the trait stabilises

## References

- [ADR 004: Extend LoRa to bare-metal targets](./004-no-std-esp-hal-lora.md) — the LoRa dual-HAL decision this ADR mirrors
- [ADR 005: Crate Naming Convention for Dual-HAL Drivers](./005-crate-naming-for-dual-hal-drivers.md) — establishes the separate-crates pattern
- [`docs/hal-naming-and-packaging-conventions.md`](../hal-naming-and-packaging-conventions.md) — ecosystem research supporting separate-crates approach
- [`docs/ROADMAP.md`](../ROADMAP.md) — dependency research and phased implementation plan
