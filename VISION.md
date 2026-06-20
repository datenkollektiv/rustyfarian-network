# Project Vision

## North Star

Any ESP32-IDF project can add Wi-Fi and MQTT in minutes, with confidence.

## Long-Term Goals

- **Reliable, thin hardware wrappers** — the `wifi` and `mqtt` features of `rustyfarian-esp-idf-network`
  remain focused connective tissue: starting connections, managing subscriptions, and surfacing
  errors — nothing more.
- **A growing platform-independent layer** — `juggler` accumulates the logic that
  can be verified without hardware: validation, timing math, backoff calculations, and similar pure
  functions; all unit-tested on the host.
- **Minimal friction for adopters** — adding networking to a new ESP32-IDF firmware project should
  require a few lines of `Cargo.toml` and no surprises; builder APIs, sensible defaults, and clear
  error messages make that possible.
- **Driven by real firmware, not speculation** — new features are added when a concrete downstream
  project surfaces a gap; the library stays lean and avoids untested abstractions.
- **An `esp-hal` bare-metal tier** — `rustyfarian-esp-hal-network` provides bare-metal (`no_std`) drivers alongside the ESP-IDF path, feature-gated by domain (`wifi`, `lora`, `ota`, `provisioning`) and chip.
  The three HAL tiers stay separate crates because mutually-exclusive backends cannot be feature-toggled, while platform-independent types and traits are shared via `juggler` (see [ADR 005](docs/adr/005-crate-naming-for-dual-hal-drivers.md)); the per-domain crates within each tier were later consolidated into one crate per tier (see [ADR 016](docs/adr/016-crate-consolidation-for-publishing.md)).
- **OTA as firmware-update plumbing** — OTA support may live in this workspace when it reuses the same Wi-Fi, bootloader, partition-table, and dual-HAL foundations as the networking crates.
- **SoftAP captive-portal provisioning** — provisioning support may live in this workspace when it reuses the same Wi-Fi lifecycle and NVS foundations as the networking crates.
  The HTTP server backing the captive portal is a private transport, not a reusable workspace API (see [ADR 013](docs/adr/013-softap-provisioning-acceptance.md)).

## Target Beneficiaries

ESP32-IDF Rust firmware developers — primarily the maintainer's own downstream projects,
but with an API clean enough that any ESP32-IDF project can adopt it with confidence.

## Non-Goals

- **General-purpose application-layer clients** — HTTP, CoAP, WebSocket, and similar reusable clients are out of scope.
  Feature-specific private transports may exist behind crate APIs, such as OTA fetching, but are not exported as protocol libraries.
- **BLE provisioning** — no BLE-based Wi-Fi setup flows; SoftAP captive-portal provisioning is in scope (see [ADR 013](docs/adr/013-softap-provisioning-acceptance.md)).
- **Full `no_std` / `esp-hal` MQTT** — MQTT over bare-metal Wi-Fi (a future `mqtt` feature on `rustyfarian-esp-hal-network`) is a long-term goal but not an active workstream.

## Success Signals

- A new firmware project can integrate Wi-Fi + MQTT in under 30 minutes with no surprises.
- `juggler` has enough coverage that most connection-logic bugs surface in
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
- 2026-04-29 — OTA accepted as firmware-update plumbing in this workspace, with private transports only.
  General-purpose HTTP clients remain a non-goal (ADR 011).
- 2026-06-11 — SoftAP captive-portal provisioning accepted as a workspace concern, deferred to Long-term.
  The captive-portal HTTP server is a private transport, not a reusable workspace API.
  BLE provisioning remains a non-goal (ADR 013).
- 2026-06-12 — Wi-Fi + MQTT provisioning profile accepted, generalising the four-field v1 schema into a closed set of named `SchemaProfile`s (`LorawanFieldDevice`, `WifiMqttDevice`).
  `rustyfarian-network-pure` gains a `no_std`-safe surface so `provisioning-pure` can delegate MQTT validation without dragging in `std` (ADR 014).
- 2026-06-20 — The 16 per-domain crates consolidated into three publishable crates — `juggler` (pure), `rustyfarian-esp-idf-network` (ESP-IDF), and `rustyfarian-esp-hal-network` (bare-metal `esp-hal`) — each feature-gated by domain, in preparation for the first crates.io release (0.4.0).
  Supersedes the per-domain granularity of ADR 005, keeping its HAL-split core (ADR 016).
