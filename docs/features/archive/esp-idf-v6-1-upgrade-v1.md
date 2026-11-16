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
- ESP-IDF v6.1 defaults to picolibc: the generated sdkconfig has `CONFIG_LIBC_PICOLIBC=y` (also in this repo's own v6.1 builds), and newlib-only keys such as `CONFIG_NEWLIB_NANO_FORMAT` are silently ignored.
  The workspace `sdkconfig.defaults` files contain no newlib keys, so this repo is unaffected, and the 2026-11-15 hardware runs already ran on picolibc.
  Consumers that set newlib keys keep newlib with `CONFIG_LIBC_NEWLIB=y` or accept picolibc.
- The ESP-NOW channel-stability work (ADR 012) and the coexistence workarounds in consumers depended on the v5.5.2 fix; v6.1 is later, so some workarounds may be removable, but this is not yet verified on hardware.
- The ESP-IDF tool install shared across projects gains a second IDF tree until every consumer has moved.
- `juggler` is unaffected (pure `no_std`); `rustyfarian-esp-hal-network` is unaffected (esp-hal tier).

## Validation plan
1. Per-chip example builds for C3, C6, S3 and classic ESP32 (`just build-example <name>`).
2. Hardware: run the hardware gate on each chip the release claims.
3. ESP-NOW coexistence soak on the C6 with the v6.1 build.
4. Release as the next minor together with `rustyfarian-esp-idf-ws2812`; consumers bump `ESP_IDF_VERSION` in lockstep and re-run their flash-and-test checklist.

## Open items
- [x] Per-chip example builds complete for C3, C6, S3 and ESP32.
  `just build-all-examples idf_` 2026-11-15: 23 of 23 built (16 C3, 3 C6, 3 S3, 1 ESP32).
- [ ] Hardware runs on each claimed chip.
  ESP32-C3 and ESP32-S3 have v6.1 evidence (see the details block below).
  Still owed on v6.1: ESP32-C6 (`c6/connect`, `c6/mqtt`), classic ESP32 (`esp32/mqtt`), and the attended router steps on C3 and S3.
  Deferred 2026-11-15 to the full hardware test before the next release (maintainer decision).
- [ ] ESP-NOW coexistence soak on the C6 with v6.1.
  Deferred 2026-11-15 to the full hardware test before the next release (maintainer decision).
- [x] Re-check that esp-mqtt in `espressif/mqtt` 1.1.0 still holds `MQTT_API_LOCK` during event dispatch; ADR 017 and the MQTT callback contract depend on it.
  Source-checked 2026-11-12: `esp_mqtt_task` locks at `mqtt_client.c` line 1982 and unlocks at 2164/2175, with both `esp_mqtt_dispatch_event_with_msgid` calls inside, the same shape as v5.3.3 (1580–1734).
- [x] Hardware re-run of `idf_c3_mqtt_callback_guard` on v6.1 (the bug 001 gate).
  Passed 2026-11-15 on the ESP32-C3: 298 s, sent = received = 146, guard_hits = 292, no panic, reboot or disconnect.
- [x] Confirm whether the espflash-bundled bootloader accepts v6.1 app images; ws2812 verified only the IDF-built v6.1 bootloader, and only on ESP32-C3.
  Verified 2026-11-15 on ESP32-C3 only: espflash 4.6.0 bundles v6.1-beta1 bootloaders for 14 chips, and the C3 boots the v6.1 app with the espflash-bundled bootloader.
  Other chips are not covered by this check.

<details><summary>Hardware evidence on v6.1 (2026-11-15 and 2026-11-16)</summary>

ESP32-C3 and ESP32-S3 runs from this repo on ESP-IDF v6.1 builds, 2026-11-15:

- ESP32-C3: `idf_c3_mqtt_callback_guard` passed (see the open item above) and the espflash-bundled bootloader boots the v6.1 app.
- ESP32-S3 (CrowPanel) IDF tier: `mqtt-lwt` 5/5, `mqtt-scenarios` 5/5 (local broker), `suspend` 13/13 (both tiers), and `soak` 3/3 with a flat heap (about 210.7 KB free, minimum 204 512 B).

v6.1 behaviour seen on the bench:

- v6.1 posts `IP_EVENT_NETIF_UP` and `IP_EVENT_NETIF_DOWN` (IDs 9 and 10), which esp-idf-svc 0.53 logs as `IpEvent: unknown event ID 9, ignoring`; the warning is harmless.
- On v6.1 the IDF `[mqtt] disconnected` line lands before `wait_suspended` returns, so `bench/s3/suspend.toml` was relaxed for it.

Downstream evidence, 2026-11-16, from a downstream firmware on a Heltec WiFi LoRa 32 V3 (ESP32-S3).
It uses the published 0.7.0 crates with features `wifi` and `lora`.
The same firmware was flashed on v5.3.3 and v6.1 minutes apart, and `main` changes no Rust sources that the downstream firmware uses, so the result carries over to the next release.

- Both versions behave identically: the IDF-built bootloader boots the app (the v6.1 bootloader for v6.1), and `juggler::lora::sx1262::init` logs `SX1262 initialized`.
- OTAA join TX and RX1/RX2 windows keep the same cadence (868.x MHz SF7 / 869.525 MHz SF12, re-join backoff 5 s).
- `WiFiManager` defaults link up on channel 11 at −24 dBm, with IP at 5.9 s on v5.3.3 and 6.1 s on v6.1.
- TX power is Low (34 quarter-dBm), country is `01`, and channels 1–11 are used on both.
- The `esp_http_client` OTA version check fails cleanly with `ESP_ERR_HTTP_CONNECT` against an offline server; there is no panic or watchdog in 60 s, and `just check-matrix` passes on both.
- Not covered downstream: OTAA join accept (gateway offline), MQTT, ESP-NOW, provisioning, and OTA download.
- The downstream app grew by 67 KB on v6.1 (1,622,208 → 1,691,152 bytes).

</details>

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing (host tests and `just verify` green, `just check-idf-all` passes on v6.1)
- [x] Documentation updated (CHANGELOG Unreleased has the three v6.1 entries, `docs/project-lore.md` has the v6.x entries)

## Session Log
- 2026-10-11 — Feature doc created from a downstream project's maintenance audit; supersedes the bare roadmap research note.
- 2026-11-10 — Deferred from 0.7.0 to the next release, coordinated with `rustyfarian-esp-idf-ws2812`; the ESP-IDF coexistence soak marked N/A for 0.7.0.
- 2026-11-12 — Target changed from v5.5.5 to v6.1 by maintainer decision; `.cargo/config.toml` committed, `extra_components` added, `just check-idf-all` passes on v6.1 with no Rust source changes.
- 2026-11-15 — Closed without hardware runs by maintainer decision; the chip checks move to the full hardware test before the next release; archived.
- 2026-11-15 (after archiving) — The ESP32-C3 bug 001 gate and the ESP32-S3 IDF tier checks passed on v6.1 the same day; the open items above are updated, and the feature stays archived.
- 2026-11-16 — Downstream ESP32-S3 evidence from a downstream firmware matched on v5.3.3 and v6.1; picolibc finding recorded: ESP-IDF v6.1 defaults to picolibc, newlib-only sdkconfig keys are ignored, and this repo's workspace defaults contain none.
