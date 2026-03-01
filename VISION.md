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

## Target Beneficiaries

ESP32-IDF Rust firmware developers — primarily the maintainer's own downstream projects,
but with an API clean enough that any ESP32-IDF project can adopt it with confidence.

## Non-Goals

- **Application-layer protocols** — HTTP, CoAP, WebSocket, and similar are out of scope;
  this library stops at Wi-Fi association and MQTT pub/sub.
- **Provisioning / SoftAP mode** — no captive portal, BLE provisioning, or Wi-Fi setup flows.
- **`esp-hal` / bare-metal targets** — only ESP-IDF (`std`) is supported; no `no_std` / `esp-hal` target.

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
