# ADR 004: Extend the LoRa Stack to Support bare-metal (esp-hal) Targets

## Status

Accepted

## Context

The `rustyfarian-network` workspace has expanded from Wi-Fi and MQTT (ESP-IDF only) to include `rustyfarian-esp-idf-lora`, a LoRa/LoRaWAN driver currently targeting `esp-idf-hal` (std + FreeRTOS).

The sister repository `rustyfarian-ws2812` has published a complete bare-metal WS2812 driver stack:

- `rustyfarian-esp-hal-ws2812` v0.3.0 — no_std, bare-metal RMT driver using `esp-hal 1.0.0`
- `led-effects` — a hardware-agnostic `StatusLed` trait and `NoLed` stub (no_std-compatible)

This allows firmware to drive WS2812 RGB LEDs on bare-metal hardware without ESP-IDF overhead.

The current `VISION.md` lists as a non-goal:

> **`esp-hal` / bare-metal targets** — only ESP-IDF (`std`) is supported; no `no_std` / `esp-hal` target.

However, a second ESP32-S3 hardware target — the Heltec V3 wireless development board — requires LoRa + RGB LED status indication without the overhead of full ESP-IDF.
This target drives the decision to extend the workspace to support a bare-metal tier.

## Decision

Extend LoRa support to bare-metal targets via **separate crates**, following the dual-HAL convention established in ADR 005.

The structure will be:

```
lora-pure                        — no_std; shared LoRaRadio trait, TxConfig, RxConfig, config types
rustyfarian-esp-idf-lora         — std; esp-idf-hal; anyhow errors (existing crate, refactored)
rustyfarian-esp-hal-lora         — no_std; esp-hal; custom error enum (new crate)
```

`lora-pure` extracts the hardware-agnostic types currently in `rustyfarian-esp-idf-lora` so both driver crates can depend on it.

`EspHalLoraRadio<S: StatusLed>` accepts a `StatusLed` impl for visual feedback — optional WS2812 status indication via the `led-effects` `StatusLed` trait, or `NoLed` stub for headless deployments.

Both `EspLoraRadio` and `EspHalLoraRadio` implement the same hardware-facing `LoraRadio` trait, allowing higher layers (LorawanDevice, callbacks) to remain hardware-agnostic.

## Consequences

### Positive

- **Bare-metal LoRa becomes possible** — firmware can use LoRa + LED feedback without ESP-IDF overhead
- **Naming honesty** — crate names accurately describe which HAL they target
- **Discoverability** — both HAL variants are independently searchable on crates.io
- **Semver independence** — `esp-hal` breaking changes do not affect `esp-idf` version history; `rustyfarian-esp-idf-lora` users are insulated from `esp-hal` upgrade pressure
- **Code reuse** — `lora-pure` shared types and traits serve both drivers; no duplication of config and protocol logic
- **LED feedback** — `EspHalLoraRadio<S: StatusLed>` makes hardware-independent LED status reporting standard practice
- **No breaking change** — existing `rustyfarian-esp-idf-lora` consumers experience no disruption
- **Ecosystem integration** — separate crates respect Cargo feature additivity and ESP ecosystem naming conventions (ADR 005)

### Negative / Risks

- **Refactoring scope** — extracting `lora-pure` from `rustyfarian-esp-idf-lora` requires careful API design to maximize reuse without over-generalizing
- **Two drivers to maintain** — both `esp-idf` and `esp-hal` paths must be tested on actual hardware (Heltec V3 for `esp-hal`, any ESP32 variant for `esp-idf`)
- **API drift risk** — the two drivers may diverge over time, especially if one targets a different LoRaWAN stack in the future (mitigated by shared `lora-pure` types)

## References

- [ADR 005: Crate Naming Convention for Dual-HAL Drivers](./005-crate-naming-for-dual-hal-drivers.md) — establishes the separate-crates pattern
- [`docs/hal-naming-and-packaging-conventions.md`](../hal-naming-and-packaging-conventions.md) — ecosystem research supporting separate-crates approach
