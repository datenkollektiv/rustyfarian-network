# ADR 008: ESP-NOW Radio-Only Initialisation

## Status

Accepted (amended — see ADR 009 for `WifiInterface` correction)

> **Amendment (2026-03-28):** The original decision recommended `WifiInterface::Ap`
> for `init_with_radio()` peers.
> Hardware testing revealed that `EspWifi::new().start()`
> defaults to STA mode; using the AP interface causes `ESP_ERR_ESPNOW_IF` because no
> AP interface is active.
> `default_interface()` now always returns `WifiInterface::Sta`.
> See [ADR 009](009-espnow-channel-scanning.md) for the full analysis.

## Context

Devices that use ESP-NOW without connecting to a Wi-Fi access point (e.g. LED matrix, nunchuck controller) must manually initialise the Wi-Fi radio before calling `EspIdfEspNow::init()`.
Every consumer writes the same boilerplate:

```rust
let mut wifi = EspWifi::new(peripherals.modem, sys_loop, Some(nvs))?;
wifi.start()?;
let espnow = EspIdfEspNow::init()?;
```

Additionally, ESP-NOW-only devices originally required manual interface decisions in
application code.
This proved error-prone because the radio started by `EspWifi::new().start()` is STA by default,
and forcing AP interface selection caused `ESP_ERR_ESPNOW_IF`.

### Design decisions under evaluation

**Where to store the radio**

`init_with_radio()` must keep the `EspWifi` instance alive — if dropped, the radio shuts down and ESP-NOW stops working.
Two options: a separate wrapper struct, or an `Option<EspWifi<'static>>` field on the existing `EspIdfEspNow`.
A separate struct would duplicate the `EspNowDriver` trait implementation.

**WifiInterface auto-detection**

`init_with_radio()` starts the radio in STA mode.
The driver can expose `default_interface()` so callers do not guess interface mode.

**PeerConfig ergonomics**

`PeerConfig` in `espnow-pure` defaults `interface` to `Sta`.
An AP builder remains useful only for explicit AP/APSTA deployments.

## Decision

Add `init_with_radio()` to `EspIdfEspNow` and a `with_ap_interface()` builder to `PeerConfig`.
Leave the existing `init()` unchanged for devices that manage Wi-Fi themselves.

### Changes to `rustyfarian-esp-idf-espnow`

- Add `_wifi: Option<EspWifi<'static>>` field to `EspIdfEspNow`.
  Existing `init()` sets it to `None`; `init_with_radio()` stores `Some(wifi)`.

- Add `init_with_radio(modem, sys_loop, nvs)` method that:
  1. Creates `EspWifi::new(modem, sys_loop, nvs)` and calls `.start()`.
  2. Stores the `EspWifi` instance to keep the radio alive.
  3. Delegates to the existing `init_with_capacity()` logic for ESP-NOW setup.

- Add `default_interface()` method returning `WifiInterface::Sta`.
  `init_with_radio()` and `init()` both operate with an active STA interface.

- Add `esp-idf-hal` workspace dependency to `Cargo.toml` (for `Modem` type).

### Changes to `espnow-pure`

- Add `PeerConfig::with_ap_interface()` builder method.
  This is optional and intended for explicit AP/APSTA flows.

### What stays unchanged

- `EspNowDriver` trait — radio init is a platform concern, not a driver operation.
- `WifiInterface` enum — no new variants needed.
- Existing `init()` / `init_with_capacity()` signatures — fully backwards compatible.

## Consequences

### Positive

- **Eliminates boilerplate** — ESP-NOW-only firmware drops ~8 lines of manual radio init per crate.
- **Eliminates interface-mode guesswork** — `default_interface()` now points callers to STA, preventing `ESP_ERR_ESPNOW_IF` from AP misconfiguration.
- **Backwards compatible** — existing `init()` callers are unaffected.
- **Radio lifetime is safe** — `EspWifi` stored in the struct prevents accidental drop.

### Negative

- **`EspIdfEspNow` grows an `Option` field** — minor size increase; `None` path has no overhead
- **New dependency** — `esp-idf-hal` added to `rustyfarian-esp-idf-espnow` (already a transitive dep via `esp-idf-svc`)

## References

- [ADR 007 — ESP-NOW Abstraction Layer](007-espnow-abstraction.md)
- [Review queue: init-with-radio-defaults-to-softap](../../review-queue/init-with-radio-defaults-to-softap.md)
