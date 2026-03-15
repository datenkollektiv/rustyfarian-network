# ADR 007: ESP-NOW Abstraction Layer

## Status

Accepted

## Context

Three firmware crates in `rustbox-backstage` (`firmware-rgb-puzzle-brain`, `firmware-rgb-matrix`, `firmware-rgb-nunchuck`) each contain ~155 lines of nearly identical ESP-NOW wrapper code:

- A static `Mutex<Option<SyncSender<EspNowEvent>>>` for bridging the C receive callback into Rust
- An `unsafe extern "C" fn recv_callback(...)` that copies frames into the channel
- `esp_now_init` / `esp_now_send` / `esp_now_deinit` FFI calls
- Peer management via zeroed `esp_now_peer_info_t` structs
- An `EspNowEvent` struct carrying sender MAC + payload

Each copy has drifted slightly (one uses `drain()`, another auto-adds peers on receive), but the core pattern is identical.
This violates DRY and makes bug fixes or API improvements require synchronized edits across three repositories.

The dual-HAL pattern established in ADR 005 and proven by Wi-Fi (ADR 006) and LoRa (ADR 004) provides a clear template for extraction.

### Design decisions under evaluation

**Raw `esp_idf_sys` FFI vs. `esp-idf-svc::espnow`**

`esp-idf-svc` (0.52) exposes an `EspNow` wrapper, but the downstream code is battle-tested on raw FFI.
The receive-callback bridge — a `sync_channel` stored in a static `Mutex`, written from an unsafe C callback and read from Rust — is the critical piece that has been debugged across three products.
Migrating to `esp-idf-svc::espnow` would introduce a new integration surface with no functional benefit today.

**Bare-metal (`esp-hal`) stub**

Unlike Wi-Fi and LoRa, there is no bare-metal ESP-NOW use case in any current or planned project.
Creating a stub crate now would be speculative and add maintenance burden for zero users.

**`&self` vs. `&mut self` on the trait**

The real ESP-NOW driver uses global FFI state: `esp_now_send` and `esp_now_register_recv_cb` operate on process-wide singletons.
The receive side is a `sync_channel` behind a `Mutex`.
Requiring `&mut self` would force callers to hold exclusive ownership of the driver, preventing shared access from multiple tasks — a common pattern in the downstream firmware (e.g., a send task and a receive task sharing the same driver handle).

## Decision

Extract duplicated ESP-NOW code into two crates following the `*-pure` + `rustyfarian-esp-idf-*` convention:

```
espnow-pure                      — #![no_std]; trait, types, constants, mock
rustyfarian-esp-idf-espnow       — std; wraps esp_idf_sys FFI, implements trait
```

### Key design choices

- **Use raw `esp_idf_sys` FFI** in `rustyfarian-esp-idf-espnow`, not `esp-idf-svc::espnow`.
  The trait boundary means a future migration to `esp-idf-svc` internals is non-breaking.

- **No `rustyfarian-esp-hal-espnow` stub.**
  The naming convention is established by ADR 005; the crate can be added when a bare-metal use case materialises, without any breaking change to existing code.

- **`EspNowDriver` trait methods take `&self`** (not `&mut self`).
  Interior mutability matches the reality of the FFI layer and allows shared ownership patterns.

### Crate responsibilities

**`espnow-pure` (implemented)**

| Item | Purpose |
|:-----|:--------|
| `MacAddress` | `[u8; 6]` type alias |
| `MAX_DATA_LEN` | 250-byte ESP-NOW payload limit |
| `BROADCAST_MAC` | `[0xFF; 6]` |
| `validate_payload()` | Payload length check |
| `EspNowEvent` | Fixed-size received frame (no heap) |
| `PeerConfig` | Peer registration config with defaults |
| `EspNowDriver` | Trait: `add_peer`, `remove_peer`, `send`, `try_recv` |
| `MockEspNowDriver` | Test double (behind `mock` feature) |

**`rustyfarian-esp-idf-espnow` (planned)**

| Component | What it replaces |
|:----------|:-----------------|
| Static `Mutex<Option<SyncSender>>` + receive callback | ~30 lines per downstream crate |
| `EspIdfEspNow::init()` / `init_with_capacity()` | ~20 lines per downstream crate |
| `impl EspNowDriver` | ~40 lines per downstream crate |
| `impl Drop` (deinit + cleanup) | ~10 lines per downstream crate |

### What stays downstream

The `rgb-puzzle-protocol` crate in `rustbox-backstage` encodes and decodes puzzle-specific messages.
It has no dependency on the transport layer and is unchanged by this extraction.

## Consequences

### Positive

- **Single source of truth** for ESP-NOW initialisation, callback bridging, and peer management
- **Testable on the host** — `MockEspNowDriver` enables unit tests for protocol logic without hardware
- **Consistent with workspace conventions** — follows the `*-pure` + `rustyfarian-esp-idf-*` pattern from ADR 005
- **Non-breaking future migration** — switching from raw FFI to `esp-idf-svc::espnow` only affects the ESP-IDF crate internals; downstream code programs against the trait

### Negative

- **One more crate pair to maintain** (mitigated by thin driver code and shared trait)
- **Raw FFI is unsafe** — the receive callback and static sender require `unsafe` blocks that must be audited carefully (mitigated by extracting the pattern once rather than maintaining three copies)

## Implementation Notes

### `esp-idf-svc` used over raw FFI (2026-03-15)

The original decision recommended raw `esp_idf_sys` FFI.
During implementation, we chose `esp-idf-svc::espnow::EspNow` instead:

- Handles version-conditional callback signatures (`esp_idf_version_major = "4"` vs v5) — getting these wrong causes silent UB
- Singleton enforcement via internal `TAKEN` mutex
- `Drop` calls `esp_now_deinit` and clears callbacks automatically
- `register_recv_cb` accepts a closure — the `sync_channel` bridge fits naturally
- Already a workspace dependency — zero new deps

The trait boundary (`EspNowDriver`) means this is a non-breaking internal choice, as anticipated in the original decision.

## References

- [ADR 005 — Crate Naming Convention for Dual-HAL Drivers](005-crate-naming-for-dual-hal-drivers.md)
- [ADR 006 — Extend Wi-Fi Support to bare-metal Targets](006-no-std-esp-hal-wifi.md)
- [ADR 004 — no-std esp-hal LoRa](004-no-std-esp-hal-lora.md)
