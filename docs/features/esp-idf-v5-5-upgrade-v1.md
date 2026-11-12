# Feature: Move the ecosystem wave from ESP-IDF v5.3.3 to v5.5.5 v1

Requested by a downstream project (2026-10-11, quarterly maintenance cycle that adopted `rustyfarian-esp-idf-network` 0.6.0 and `rustyfarian-esp-idf-ws2812` 0.7.0).
Every rustyfarian ESP-IDF crate and every known consumer (`rustyfarian-rgb-clock` and a downstream project) pins `ESP_IDF_VERSION = "v5.3.3"`.
Roadmap Research item 1 already tracks "Evaluate ESP-IDF v5.5.2"; this document collects what is known today so the bump can be scheduled.
Target: the next release after 0.7.0, together with `rustyfarian-esp-idf-ws2812`; 0.7.0 stays on v5.3.3.

## Why now

| Fact | Evidence |
|:-----|:---------|
| v5.3.x is two minor releases behind; v5.5.5 is the current 5.x patch (2026-07-17) | `espressif/esp-idf` releases: v5.5.3 (2026-02-18), v5.5.4 (2026-03-27), v5.5.5 (2026-07-17) |
| v5.5.2 fixed "ESP-NOW send failure when coexistence is enabled" | `docs/ROADMAP.md` Research 1; consumers run Wi-Fi STA + ESP-NOW on one radio and carry sdkconfig workarounds (AMPDU off, SW coexist on) |
| `espflash` 4.x bundles a v5.5.1 bootloader that is incompatible with v5.3.3 app binaries | `docs/project-lore.md` "Flashing & Serial"; every consumer must pass `--bootloader` or pin espflash |
| `esp-idf-svc` 0.53.0 (2026-09-25) already carries the v5.5 API surface | esp-idf-svc CHANGELOG 0.53.0 |
| ESP-IDF v6.0 (2026-03-20, now 6.0.3) and v6.1 (2026-08-27) exist | `espressif/esp-idf` releases |

## What esp-idf-svc 0.53 changes on v5.5 (from its CHANGELOG)

- New `WifiEvent` variants that an exhaustive match must handle: `StaBeaconOffsetUnstable`, `DppUriReady`, `DppCfgRecvd`, `DppFailed` (v5.5.0+), plus `ApWrongPassword` (v5.3.3+ / v5.4.1+ / v5.5.0+) and `StaNeighborRep` (v5.3.0+).
- `DppCfgRecvdRef` is no longer `Copy`/`Clone` (flexible-array member in the C struct).
- Unknown event IDs now map to `Other(i32)` with a warning instead of panicking.
- TLS server `ServerConfig` gains `tls_handshake_timeout_ms` on ESP-IDF >= 5.5.0; struct literals must set it.
- MQTT 5 `MqttProtocolVersion::V5` and `Mqtt5ConnectionPropertyConfig`, gated on `CONFIG_MQTT_PROTOCOL_5=y`.
- ESP-IDF v6.0 moved MQTT and the Ethernet PHY drivers to external components; v6 needs `[[package.metadata.esp-idf-sys.extra_components]]` entries.
  That is the reason to target 5.5.x, not 6.x, for this bump.

## Decisions
| Decision | Reason | Rejected Alternative |
|---------:|:-------|:---------------------|
| Target v5.5.5, not v6.x | 6.x needs the external MQTT component and a new sdkconfig audit; 5.5 is a drop-in for esp-idf-svc 0.53 | Jump straight to v6.1 |
| Bump the library first, consumers follow | Consumers pin the version the library validated against (the downstream project's `maintenance-plan.md`, "follow the rustyfarian wave") | Let a consumer go first |
| Keep `esp-idf-svc` 0.53 / `esp-idf-hal` 0.47 | Already carry the 5.5 surface; no second bump needed | Wait for esp-idf-svc 0.54 |
| Ship the bump after 0.7.0, with `rustyfarian-esp-idf-ws2812` | Consumers build both libraries against one ESP-IDF version, and ws2812 v0.9.0 pins v5.3.3 | Bump the network crates alone in 0.7.0 |

## Constraints
- All `sdkconfig.defaults` in `crates/*/` and the workspace root must be re-checked against the v5.5 Kconfig; renamed keys are silently ignored (see the `CONFIG_ESP_WIFI_ESPNOW_MAX_ENCRYPT_NUM` rename in v5.3 recorded in the downstream project's `docs/project-lore.md`).
- The ESP-NOW channel-stability work (ADR 012) and the coexistence workarounds in consumers depend on the v5.5.2 fix, which may let some workarounds go.
- The ESP-IDF tool install shared across projects (`rustyfarian-rgb-clock/.cargo/config.toml` notes ~11 GB) gains a second IDF tree until every consumer has moved.
- `juggler` is unaffected (pure `no_std`); `rustyfarian-esp-hal-network` is unaffected (esp-hal tier).

## Open Questions
- [ ] Does any rustyfarian driver match `WifiEvent` exhaustively? (`grep -rn "WifiEvent::" crates/rustyfarian-esp-idf-network/src`)
- [ ] Is the v5.5.1 bootloader that `espflash` 4.x bundles compatible with v5.5.5 app images, so the `--bootloader` workaround can be retired?
- [ ] Does `just doctor` need a check for the installed IDF version versus `ESP_IDF_VERSION`?

## Validation plan
1. Bump `ESP_IDF_VERSION` in `.cargo/config.toml`, clean the `esp-idf-sys-*` build dirs, build every example for C3 and C6.
2. Hardware: the hardware check, including the ESP-IDF v5.5 coexistence soak on the C6, runs in the release that ships the bump (N/A in 0.7.0).
3. Release as the next minor; consumers bump `ESP_IDF_VERSION` in lockstep and re-run their flash-and-test checklist.

## State
- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-10-11 — Feature doc created from a downstream project's maintenance audit; supersedes the bare roadmap research note.
- 2026-11-10 — Deferred from 0.7.0 to the next release, coordinated with `rustyfarian-esp-idf-ws2812`; the ESP-IDF v5.5 coexistence soak marked N/A for 0.7.0
