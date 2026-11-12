# Feature: Move the ecosystem wave to ESP-IDF v6.1 v1

Requested by a downstream project (2026-10-11, quarterly maintenance cycle that adopted `rustyfarian-esp-idf-network` 0.6.0 and `rustyfarian-esp-idf-ws2812` 0.7.0).
Every rustyfarian ESP-IDF crate and every known consumer pinned `ESP_IDF_VERSION = "v5.3.3"`.
This document replaces the earlier v5.5.5 plan (`esp-idf-v5-5-upgrade-v1.md`, deleted, see git history), which rejected v6.x because of the external MQTT component.
That objection is now solved, and the maintainer decided on 2026-11-12 to move the whole ecosystem straight to v6.1.

## Why now

| Fact | Evidence |
|:-----|:---------|
| `rustyfarian-esp-idf-ws2812` already moved to v6.1 | Commit 4a863f8 in `../rustyfarian-ws2812` |
| `rustyfarian-power` is moving to v6.1 in lockstep | Maintainer decision 2026-11-12 |
| v6.1 released 2026-08-27; v6.0 moved esp-mqtt to the managed component `espressif/mqtt` | `espressif/esp-idf` releases; ESP-IDF v6.0 migration guide, "ESP-MQTT" |
| v6.1 esp-clang `esp-21.1.3_20260408` has no libxml2 dependency | Verified by the ws2812 build on ubuntu:26.04 |
| `esp-idf-svc` 0.53, `esp-idf-hal` 0.47 and `esp-idf-sys` 0.38.1 stay unchanged | `just check-idf-all` passes on v6.1 with no Rust source changes |

## What esp-idf-svc 0.53 exposes on newer IDFs (from its CHANGELOG)

- New `WifiEvent` variants that an exhaustive match must handle: `StaBeaconOffsetUnstable`, `DppUriReady`, `DppCfgRecvd`, `DppFailed` (v5.5.0+), plus `ApWrongPassword` (v5.3.3+ / v5.4.1+ / v5.5.0+) and `StaNeighborRep` (v5.3.0+).
- `DppCfgRecvdRef` is no longer `Copy`/`Clone` (flexible-array member in the C struct).
- Unknown event IDs now map to `Other(i32)` with a warning instead of panicking.
- TLS server `ServerConfig` gains `tls_handshake_timeout_ms` on ESP-IDF >= 5.5.0; struct literals must set it.
- MQTT 5 `MqttProtocolVersion::V5` and `Mqtt5ConnectionPropertyConfig`, gated on `CONFIG_MQTT_PROTOCOL_5=y`.

## Decisions

| Decision | Reason | Rejected Alternative |
|---------:|:-------|:---------------------|
| Target ESP-IDF v6.1 | Ecosystem lockstep with `rustyfarian-esp-idf-ws2812` and `rustyfarian-power`; the MQTT objection is solved | v5.5.5 (superseded 2026-11-12) |
| Commit `.cargo/config.toml` with `ESP_IDF_VERSION = "v6.1"` | One pinned source of truth; cargo never reads a dependency's `.cargo/config.toml` and the workspace-root `.cargo/` is outside every published crate, so consumers are unaffected | `.cargo/config.toml.dist` template plus `just setup-cargo-config` copy step (both removed) |
| Declare `espressif/mqtt` as `remote_component` in `crates/rustyfarian-esp-idf-network/Cargo.toml` under `[[package.metadata.esp-idf-sys.extra_components]]` | esp-idf-sys 0.38.1 honours `extra_components` of the root crate and of its direct dependencies | Per-consumer entries (needed only for a firmware that reaches the crate transitively) |
| Set `ESP_IDF_SYS_ROOT_CRATE = "rustyfarian-esp-idf-network"` and `CARGO_WORKSPACE_DIR = { value = "", relative = true }` in `[env]` | The virtual workspace has no root package; without these, esp-idf-sys's `cargo metadata` ran outside the workspace on the RAM disk | Adding a root package to the workspace |
| Return CI to `runs-on: ubuntu-latest` | The v5.3.3 esp-clang needed `libxml2.so.2`; v6.1 does not | Keeping `ubuntu-24.04` |
| Keep `esp-idf-svc` 0.53 / `esp-idf-hal` 0.47 / `esp-idf-sys` 0.38.1 | Already carry the v6.1 surface; no second bump needed | Wait for esp-idf-svc 0.54 |
| `scripts/ensure-bootloader.sh` records `idf_version=` in the cache `.meta` and rebuilds a cached bootloader from another IDF version | A bootloader built for v5.3.3 must not be reused for v6.1 apps | Keeping one bootloader cache across versions |

## Constraints
- Generated sdkconfig shows `CONFIG_IDF_INIT_VERSION="6.1.0"`, managed component `espressif__mqtt` 1.1.0, and the workspace `sdkconfig.defaults` applied.
- All seven keys in `sdkconfig.defaults` and `sdkconfig.sta-only.defaults` exist in the v6.1 Kconfig; a renamed key would be silently ignored, so re-check after any sdkconfig change.
- The ESP-NOW channel-stability work (ADR 012) and the coexistence workarounds in consumers depended on the v5.5.2 fix; v6.1 is later, so some workarounds may be removable, but this is not yet verified on hardware.
- The ESP-IDF tool install shared across projects gains a second IDF tree until every consumer has moved.
- `juggler` is unaffected (pure `no_std`); `rustyfarian-esp-hal-network` is unaffected (esp-hal tier).

## Validation plan
1. Per-chip example builds for C3, C6, S3 and classic ESP32 (`just build-example <name>`), in progress 2026-11-12.
2. Hardware: run the hardware gate on each chip the release claims.
3. ESP-NOW coexistence soak on the C6 with the v6.1 build.
4. Release as the next minor together with `rustyfarian-esp-idf-ws2812`; consumers bump `ESP_IDF_VERSION` in lockstep and re-run their flash-and-test checklist.

## Open items
- [ ] Per-chip example builds complete for C3, C6, S3 and ESP32.
- [ ] Hardware runs on each claimed chip (none run on v6.1 yet).
- [ ] ESP-NOW coexistence soak on the C6 with v6.1.
- [x] Re-check that esp-mqtt in `espressif/mqtt` 1.1.0 still holds `MQTT_API_LOCK` during event dispatch; ADR 017 and the MQTT callback contract depend on it.
  Source-checked 2026-11-12: `esp_mqtt_task` locks at `mqtt_client.c` line 1982 and unlocks at 2164/2175, with both `esp_mqtt_dispatch_event_with_msgid` calls inside, the same shape as v5.3.3 (1580–1734).
- [ ] Hardware re-run of `idf_c3_mqtt_callback_guard` on v6.1 (the bug 001 gate).
- [ ] Confirm whether the espflash-bundled bootloader accepts v6.1 app images; ws2812 verified only the IDF-built v6.1 bootloader, and only on ESP32-C3.

## State
- [x] Design approved
- [x] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-10-11 — Feature doc created from a downstream project's maintenance audit; supersedes the bare roadmap research note.
- 2026-11-10 — Deferred from 0.7.0 to the next release, coordinated with `rustyfarian-esp-idf-ws2812`; the ESP-IDF coexistence soak marked N/A for 0.7.0.
- 2026-11-12 — Target changed from v5.5.5 to v6.1 by maintainer decision; `.cargo/config.toml` committed, `extra_components` added, `just check-idf-all` passes on v6.1 with no Rust source changes.
