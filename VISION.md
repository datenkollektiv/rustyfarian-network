# Project Vision

## North Star

Any ESP32-IDF project can add Wi-Fi and MQTT in minutes, with confidence.

## Long-Term Goals

- **Reliable, thin hardware wrappers** — `rustyfarian-esp-idf-wifi` and `rustyfarian-esp-idf-mqtt`
  remain focused connective tissue: starting connections, managing subscriptions, and surfacing
  errors — nothing more.
- **A growing platform-independent layer** — `rustyfarian-network-pure` accumulates the logic that
  can be verified without hardware: validation, timing math, backoff calculations, and similar pure
  functions; all unit-tested on the host.
- **Minimal friction for adopters** — adding networking to a new ESP32-IDF firmware project should
  require a few lines of `Cargo.toml` and no surprises; builder APIs, sensible defaults, and clear
  error messages make that possible.
- **Driven by real firmware, not speculation** — new features are added when a concrete downstream
  project surfaces a gap; the library stays lean and avoids untested abstractions.
- **An `esp-hal` bare-metal tier** — dedicated `rustyfarian-esp-hal-*` crates provide bare-metal alternatives alongside the existing ESP-IDF path.
  This generalises the earlier LoRa-only `esp-hal` goal into a workspace-wide pattern: separate crates per HAL tier with shared `*-pure` crates for platform-independent types and traits (see [ADR 005](docs/adr/005-crate-naming-for-dual-hal-drivers.md)).
  Active: `rustyfarian-esp-hal-lora` (LoRa radio driver) and `rustyfarian-esp-hal-wifi` (Wi-Fi via `esp-wifi 0.14.0`, in progress).

## Target Beneficiaries

ESP32-IDF Rust firmware developers — primarily the maintainer's own downstream projects,
but with an API clean enough that any ESP32-IDF project can adopt it with confidence.

## Non-Goals

- **Application-layer protocols** — HTTP, CoAP, WebSocket, and similar are out of scope;
  this library stops at Wi-Fi association and MQTT pub/sub.
- **Provisioning / SoftAP mode** — no captive portal, BLE provisioning, or Wi-Fi setup flows.
- **Full `no_std` / `esp-hal` MQTT** — MQTT over bare-metal Wi-Fi (`rustyfarian-esp-hal-mqtt`) is a long-term goal but not an active workstream; Wi-Fi association is the current `esp-hal` frontier.

## Success Signals

- A new firmware project can integrate Wi-Fi + MQTT in under 30 minutes with no surprises.
- `rustyfarian-network-pure` has enough coverage that most connection-logic bugs surface in
  host tests before touching hardware.
- The CI pipeline is green on every push; `just verify` passes locally without friction.
- Downstream firmware projects never need to fork or patch this library to meet a new requirement.

## Open Questions

_(none at this time)_

## Vision History

- 2026-03-01 — Initial `VISION.md` created; north star, goals, non-goals, and success signals
  established from first review session.
- 2026-03-03 — no-std / `esp-hal` LoRa tier added as an active goal; `esp-hal` Wi-Fi/MQTT remains a non-goal.
  Decision recorded in ADR 004.
- 2026-03-12 — `esp-hal` Wi-Fi promoted from non-goal to active goal; LoRa path blocked on hardware.
  `rustyfarian-esp-hal-wifi` added to long-term goals; non-goal narrowed to `esp-hal` MQTT only.
  The `esp-hal` goal was generalised from LoRa-only to a workspace-wide dual-HAL pattern (ADR 005).
