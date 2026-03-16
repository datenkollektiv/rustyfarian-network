# ADR 008: ESP-NOW Radio-Only Initialisation

## Status

Accepted

## Context

Devices that use ESP-NOW without connecting to a Wi-Fi access point (e.g. LED matrix, nunchuck controller) must manually initialise the Wi-Fi radio before calling `EspIdfEspNow::init()`.
Every consumer writes the same boilerplate:

```rust
let mut wifi = EspWifi::new(peripherals.modem, sys_loop, Some(nvs))?;
wifi.start()?;
let espnow = EspIdfEspNow::init()?;
```

Additionally, ESP-NOW-only devices must manually override `PeerConfig::interface` from the default `WifiInterface::Sta` to `WifiInterface::Ap`, because there is no STA connection.
This is a known ESP-NOW quirk that catches every new consumer and wastes debugging time.

Evidence from downstream firmware:
- `firmware-rgb-matrix/src/main.rs` — raw `EspWifi::new()` + `.start()`
- `firmware-rgb-nunchuck/src/main.rs` — same pattern + manual `brain_peer.interface = WifiInterface::Ap`

### Design decisions under evaluation

**Where to store the radio**

`init_with_radio()` must keep the `EspWifi` instance alive — if dropped, the radio shuts down and ESP-NOW stops working.
Two options: a separate wrapper struct, or an `Option<EspWifi<'static>>` field on the existing `EspIdfEspNow`.
A separate struct would duplicate the `EspNowDriver` trait implementation.

**WifiInterface auto-detection**

Consumers who use `init_with_radio()` (no STA connection) always need `WifiInterface::Ap` on their peers.
The driver knows whether it owns the radio, so it can expose a `default_interface()` method to guide callers.

**PeerConfig ergonomics**

`PeerConfig` in `espnow-pure` defaults `interface` to `Sta`.
A builder method for the AP case would eliminate the field-level override that every ESP-NOW-only device needs.

## Decision

Add `init_with_radio()` to `EspIdfEspNow` and a `with_ap_interface()` builder to `PeerConfig`.
Leave the existing `init()` unchanged for devices that manage Wi-Fi themselves.

### Changes to `rustyfarian-esp-idf-espnow`

- Add `_wifi: Option<EspWifi<'static>>` field to `EspIdfEspNow`.
  Existing `init()` sets it to `None`; `init_with_radio()` stores `Some(wifi)`.

- Add `init_with_radio(modem, sys_loop, nvs)` method that:
  1. Creates `EspWifi::new(modem, sys_loop, nvs)` and calls `.start()`
  2. Stores the `EspWifi` instance to keep the radio alive
  3. Delegates to the existing `init_with_capacity()` logic for ESP-NOW setup

- Add `default_interface()` method returning `WifiInterface::Ap` when the driver owns the radio, `WifiInterface::Sta` otherwise.

- Add `esp-idf-hal` workspace dependency to `Cargo.toml` (for `Modem` type).

### Changes to `espnow-pure`

- Add `PeerConfig::with_ap_interface()` builder method — sets `interface` to `WifiInterface::Ap`.

### What stays unchanged

- `EspNowDriver` trait — radio init is a platform concern, not a driver operation.
- `WifiInterface` enum — no new variants needed.
- Existing `init()` / `init_with_capacity()` signatures — fully backwards compatible.

## Consequences

### Positive

- **Eliminates boilerplate** — ESP-NOW-only firmware drops ~8 lines of manual radio init per crate
- **Eliminates the `WifiInterface::Ap` gotcha** — `default_interface()` + `with_ap_interface()` make the correct choice obvious
- **Backwards compatible** — existing `init()` callers are unaffected
- **Radio lifetime is safe** — `EspWifi` stored in the struct prevents accidental drop

### Negative

- **`EspIdfEspNow` grows an `Option` field** — minor size increase; `None` path has no overhead
- **New dependency** — `esp-idf-hal` added to `rustyfarian-esp-idf-espnow` (already a transitive dep via `esp-idf-svc`)

## References

- [ADR 007 — ESP-NOW Abstraction Layer](007-espnow-abstraction.md)
- Feature request: `review-queue/espnow-radio-only-init.md`
