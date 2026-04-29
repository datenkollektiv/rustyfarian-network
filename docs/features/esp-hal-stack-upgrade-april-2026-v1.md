# Feature: esp-hal Stack Upgrade — April 2026 Release Wave v1

Upgrade every crate in the bare-metal `esp-hal` stack used by `rustyfarian-esp-hal-wifi` and `rustyfarian-esp-hal-lora` from the `esp-hal 1.0.0` baseline (released October 2025, currently held only by `Cargo.lock`) to the April 2026 release wave that landed on crates.io between 2026-04-16 and 2026-04-24.
This aligns the network workspace with the same wave already adopted in `rustyfarian-ws2812` (`docs/features/esp-hal-stack-upgrade-april-2026-v1.md` in that repo) so the rustyfarian family resolves to a single coordinated stack.

## Version Table

| Crate                    | Current (`Cargo.lock`)  | Target  | Released   |
|:-------------------------|:------------------------|:--------|:-----------|
| `esp-hal`                | 1.0.0                   | 1.1.0   | 2026-04-24 |
| `esp-rtos`               | 0.2.0                   | 0.3.0   | 2026-04-16 |
| `esp-radio`              | 0.17.0                  | 0.18.0  | 2026-04-16 |
| `esp-bootloader-esp-idf` | 0.4.0                   | 0.5.0   | 2026-04-16 |
| `esp-alloc`              | 0.9.0                   | 0.10.0  | 2026-04-16 |
| `esp-println`            | 0.16.1                  | 0.17.0  | 2026-04-16 |
| `esp-backtrace`          | 0.18.1                  | 0.19.0  | 2026-04-16 |

The 2026-04-16 timestamps reflect a coordinated monorepo release wave; `esp-hal 1.1.0` followed eight days later.
The pre-1.0 crates each got a minor bump (0.x → 0.x+1), which in their semver convention typically signals breaking API changes.

## Pinning Note

`Cargo.lock` is gitignored in this workspace (`.gitignore` line 2), exactly as in `rustyfarian-ws2812`.
Today the workspace `Cargo.toml` declares caret-style shorthand (`esp-hal = { version = "1.0", ... }`, `esp-radio = { version = "0.17", ... }`, etc.), so a fresh `cargo build` on a CI runner without a cached `Cargo.lock` is already free to resolve to the April 2026 wave — but local developer machines and the existing committed `Cargo.lock` snapshot still resolve to October 2025.
That mismatch is exactly the failure mode `rustyfarian-ws2812` hit and resolved with exact pins.

This feature exact-pins the coordinated April 2026 stack in workspace `Cargo.toml` so resolution is deterministic across CI, local builds, and downstream consumers:

| Crate                    | Exact pin |
|:-------------------------|:----------|
| `esp-hal`                | `=1.1.0`  |
| `esp-rtos`               | `=0.3.0`  |
| `esp-radio`              | `=0.18.0` |
| `esp-bootloader-esp-idf` | `=0.5.0`  |
| `esp-alloc`              | `=0.10.0` |
| `esp-println`            | `=0.17.0` |
| `esp-backtrace`          | `=0.19.0` |
| `embassy-executor`       | `=0.10.0` |
| `embassy-net`            | `=0.8.0`  |
| `embassy-time`           | `=0.5.1`  |
| `embassy-sync`           | `=0.8.0`  |
| `smoltcp`                | `=0.12.0` |

Exact pins are intentional because these crates use `esp-hal/unstable`, the `esp-radio` Wi-Fi/`smoltcp`/`esp-alloc` extras, and the `esp-rtos`/embassy executor wiring — all of which are explicitly unstable and ship as coordinated waves.
Future bumps land via the `maintenance` skill rather than transparent fresh-resolution drift.

## Scope

In scope:

- `crates/rustyfarian-esp-hal-wifi/` — driver and 8 examples (`hal_c3_connect`, `hal_c3_connect_async`, `hal_c3_connect_async_led`, `hal_c3_wifi_raw`, `hal_c6_connect`, `hal_c6_connect_async_led`, `hal_c6_connect_nonblocking_rgb`, `hal_c6_wifi_raw`).
- `crates/rustyfarian-esp-hal-lora/` — driver and 1 example (`hal_esp32s3_join`).
- Workspace `Cargo.toml` `[workspace.dependencies]` block.
- Cross-repo git dep `rustyfarian-esp-hal-ws2812` — must be re-pinned to a `rustyfarian-ws2812` branch/commit that has already adopted the same April 2026 wave; otherwise feature unification across the two ws2812 git deps will conflict.

Explicitly out of scope (must not be touched):

- `rustyfarian-esp-idf-wifi`, `rustyfarian-esp-idf-mqtt`, `rustyfarian-esp-idf-lora`, `rustyfarian-esp-idf-espnow` — ESP-IDF stack, untouched by this upgrade.
- `wifi-pure`, `lora-pure`, `espnow-pure`, `rustyfarian-network-pure` — host-side pure crates with no esp-hal dependency.
- The MQTT builder API, OTA MVP, ESP-NOW command framework — orthogonal features.

## Decisions

| Decision                                                                                                                          | Reason                                                                                                                                                                                                                                                                  | Rejected Alternative                                                                                                                                                                                              |
|:----------------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Coordinated workspace upgrade of all seven crates in one feature                                                                  | The pre-1.0 crates ship as a monorepo wave; mixing 0.2 + 0.3 across `esp-rtos` / `esp-radio` produces feature-unification conflicts in the Wi-Fi `smoltcp` + `esp-alloc` graph                                                                                          | Bump only `esp-hal` first — rejected because `esp-hal 1.1.0` likely tightens trait bounds that ripple into `esp-rtos` / `esp-radio` / `embassy-net`                                                               |
| Bump in foundation order: `esp-hal` → `esp-rtos` → `esp-radio` → `esp-alloc` → `esp-bootloader-esp-idf` → `esp-println` → `esp-backtrace`, then embassy-* and `smoltcp` last | `esp-hal` is the trait/PAC root; `esp-rtos` and `esp-radio` depend on it; `embassy-net` rides on top of `smoltcp` and `esp-radio`. Fixing breakage from the bottom up keeps the error surface small per step                                                            | `cargo update` everything at once — rejected because cascading errors are harder to attribute, especially across the Wi-Fi feature unification                                                                    |
| Re-validate on real hardware before declaring complete: ESP32-C3 (`hal_c3_connect_async_led`), ESP32-C6 (`hal_c6_connect_async_led`), ESP32-S3 (`hal_esp32s3_join`) | The Wi-Fi connect path exercises `esp-radio`, `smoltcp` DHCP, and `embassy-net`; LoRa exercises `esp-hal` SPI/GPIO + `esp-bootloader-esp-idf`. Host tests cover none of this                                                                                             | Trust `cargo check` + clippy — rejected because runtime behaviour (DHCP, SX1262 IRQ, embassy spawn shape) is not visible at compile time                                                                          |
| Exact-pin the upgraded stack in workspace `Cargo.toml`                                                                            | `Cargo.lock` is gitignored; the HAL surface used here is explicitly unstable; consumers (e.g. `rustyfarian-beekeeper`) need deterministic resolution                                                                                                                    | Keep caret constraints — rejected; same drift problem `rustyfarian-ws2812` already solved with exact pins                                                                                                         |
| Re-pin the `rustyfarian-esp-hal-ws2812` git dep to a commit that has already adopted the April 2026 wave                          | The two crates resolve into a single esp-hal feature graph in `hal_c6_connect_async_led` and `hal_c6_connect_nonblocking_rgb`; mixing 1.0.0 and 1.1.0 across them breaks feature unification                                                                            | Allow git resolution to grab whatever HEAD looks like at build time — rejected; non-deterministic and prone to silent drift                                                                                       |
| Treat both HAL crates (`-wifi` and `-lora`) as one upgrade, even though only one example uses the LoRa crate today                | They share `esp-hal` and `esp-bootloader-esp-idf` workspace pins; bumping one without the other produces inconsistent local builds and forces a second migration cycle later                                                                                            | Upgrade Wi-Fi only and defer LoRa — rejected; coordinated bump is cheaper and avoids a stale-pin trap                                                                                                             |
| Use the same migration playbook as `rustyfarian-ws2812`                                                                           | The ws2812 upgrade landed on 2026-04-29 and is the freshest reference for `esp-hal 1.1.0` API deltas (RMT `configure_tx`, embassy 0.10 spawn shape, embassy-sync 0.8 pin); reusing that playbook reduces unknowns                                                        | Independent investigation — rejected; redundant work                                                                                                                                                              |

## Constraints

- All ESP-IDF crates and their examples must continue to build and run unchanged — the `idf_*` examples use the `esp-idf-svc 0.52` / `esp-idf-hal 0.46` stack, which this upgrade does not touch.
- All three current `hal_*` chip targets must continue to work: ESP32-C6 (`riscv32imac-unknown-none-elf`), ESP32-C3 (`riscv32imc-unknown-none-elf`), and ESP32-S3 (`xtensa-esp32s3-none-elf`).
- `--release` profile remains required for bare-metal builds (project-lore — esp-hal Bare-Metal Driver).
- Continue to flash with the IDF v5.3.3 bootloader workaround (project-lore — espflash) unless `esp-bootloader-esp-idf 0.5` changes that requirement.
- Wi-Fi + ESP-NOW coexistence on ESP-IDF is unaffected by this work; ADR 008 / ADR 009 ESP-NOW radio-only init paths live in `rustyfarian-esp-idf-espnow`.
- The `rustyfarian-esp-hal-ws2812` git dep must resolve to a revision that already runs on the April 2026 wave — verify the pinned commit before merging.
- `WiFiManager::init`, `WiFiManager::init_async`, `WiFiConfig`, and `AsyncWifiHandle` public APIs are unchanged by this work; if a constraint surfaces, it belongs in a separate feature.
- LoRa public surface (`LoraConfig`, `EspIdfLoraRadio`, `EspHalLoraRadio` stub) is unchanged.

## Migration Steps

1. **Branch off `main`** as `esp-hal-stack-upgrade-april-2026` so the upgrade can be reviewed independently of the OTA MVP work currently on `add-ota-mvp`.
2. **Bump constraints in workspace `Cargo.toml`** to the exact pins listed in the Pinning Note table.
3. **Re-pin the `rustyfarian-esp-hal-ws2812` git dep** to a `rustyfarian-ws2812` commit that has already merged the April 2026 wave (use `?rev=...` or `?tag=...`, not branch HEAD).
4. **Run `cargo update`** to refresh `Cargo.lock` locally; commit it temporarily for the duration of the migration so steps 5–7 share a deterministic snapshot, then drop it before merge (it stays gitignored).
5. **Resolve breakage in foundation order**: `esp-hal` → `esp-rtos` → `esp-radio` → `esp-alloc` → `esp-bootloader-esp-idf` → `esp-println` → `esp-backtrace` → embassy-* → `smoltcp`.
   After each, run `just verify` and the per-crate target check (`cargo check -p rustyfarian-esp-hal-wifi --target riscv32imac-unknown-none-elf --no-default-features --features esp32c6,unstable,rt`).
6. **Migrate embassy spawn call sites** in `hal_c3_connect_async.rs`, `hal_c3_connect_async_led.rs`, `hal_c6_connect_async_led.rs` from `spawner.must_spawn(task(...))` to whatever shape `embassy-executor 0.10` requires (the ws2812 reference uses `spawner.spawn(task().unwrap())`; verify whether `must_spawn` survived or was renamed).
7. **Migrate `#[esp_rtos::main]` and `esp_rtos::start()` call sites** in the same three async examples + `hal_c6_wifi_raw.rs` / `hal_c3_wifi_raw.rs` if the macro signature or `start()` parameter list changed.
8. **Migrate `esp_alloc::heap_allocator!` call sites** if the macro signature changed; the ESP32-C6 example uses both the `#[esp_hal::ram(reclaimed)]` form and the plain form.
9. **Validate the `esp-radio` feature flag set** against `0.18.0`: today the `-wifi` crate forwards `wifi`, `esp-alloc`, `smoltcp`, `unstable`, `log-04` — confirm each still exists; rename or drop any that were removed.
10. **Validate the `esp-rtos` feature flag set**: today the `-wifi` crate forwards `esp-radio`, `esp-alloc`, plus `embassy` (under the embassy feature). Confirm survival.
11. **Run `just build-example <name>` for every `hal_*` example**: `hal_c3_connect`, `hal_c3_connect_async`, `hal_c3_connect_async_led`, `hal_c3_wifi_raw`, `hal_c6_connect`, `hal_c6_connect_async_led`, `hal_c6_connect_nonblocking_rgb`, `hal_c6_wifi_raw`, `hal_esp32s3_join`. `just verify` only covers `riscv32imac-esp-espidf`, so the bare-metal builds need explicit per-example runs.
12. **Re-test on hardware** (CLAUDE.md mandates this for hardware-touching changes):
    - `hal_c3_connect_async_led` on ESP32-C3-DevKitM-1 — STA, DHCP, LED feedback
    - `hal_c6_connect_async_led` on ESP32-C6-DevKitC-1 — STA, DHCP, LED feedback, embassy multitask
    - `hal_esp32s3_join` on Heltec WiFi LoRa 32 V3 — SPI bring-up; OTAA join is gated by TTN hardware availability and may be partial
13. **Update `CHANGELOG.md` under `## [Unreleased]`** with one bullet per crate bump and a summary line ("April 2026 esp-hal stack wave — coordinated with rustyfarian-ws2812").
14. **Add a project-lore entry** under "esp-hal Bare-Metal Driver" capturing whichever API deltas actually broke (RMT setup, embassy spawn shape, esp-rtos macro shape, `esp-radio` feature renames). Cross-link from the `## Common Resolution Failures` table in `CLAUDE.md` if any pattern recurred during the migration.
15. **Run `just fmt && just verify`** as the completion gate.
16. **Add a roadmap entry** under "Delivered since v0.1.0" in `docs/ROADMAP.md` once merged.

## Open Questions

- [ ] Is there already a `rustyfarian-ws2812` branch/commit pinned to the April 2026 wave that this workspace can consume, or does it land on `main` first? Recheck the upstream feature doc state — if `main` is up to date, pin to a specific commit hash for determinism; if not, this upgrade waits.
- [ ] Does `esp-hal 1.1.0` change any GPIO/SPI/RMT API used by `EspHalLoraRadio` (currently a stub) or by the SX1262 bring-up in `hal_esp32s3_join`? The ws2812 upgrade only validated RMT TX, not SPI master.
- [ ] Does `esp-radio 0.18` change the `wifi` feature so that the `-wifi` crate's feature forwarding still produces a working `WifiController`/`WifiDevice` pair? The `esp_radio::wifi::WifiError::Disconnected` variant is matched explicitly in `EspHalWifiManager::is_connected()` and may have moved.
- [ ] Does `esp-rtos 0.3` change `#[esp_rtos::main]` ergonomics — specifically whether the async `main` still receives `Spawner` directly, or now wraps it in a different argument struct?
- [ ] Does `embassy-net 0.8` (assumed target; verify the actual published version on 2026-04-16 ±) change the `Stack` / `Runner` constructor or the `dhcpv4` config struct used in the async Wi-Fi examples?
- [ ] Does `esp-alloc 0.10` rename or restructure `heap_allocator!`? The ESP32-C6 example uses both the plain form and the `#[esp_hal::ram(reclaimed)]` attribute form — both must keep working.
- [ ] Does `esp-bootloader-esp-idf 0.5` still produce a descriptor accepted by the IDF v5.3.3 bootloader (per the espflash workaround in project-lore), or does it now require a newer IDF?
- [ ] Should the workspace add `embassy-sync = "=0.8.0"` explicitly to force unification? Today it appears only transitively (`embassy-sync 0.6.2` + `0.7.2` co-resolve in `Cargo.lock`); after the bump, the explicit pin avoids a future split.
- [ ] Is the `xtensa-esp32s3-none-elf` toolchain installed and ready on the development machine running this upgrade? The ws2812 feature flagged Xtensa builds as gated by local toolchain availability; the same gate applies here for `hal_esp32s3_join`.

## Validation Plan

Once implementation lands, validation evidence to record under `## Validation Evidence`:

- `just verify` — must pass clean.
- `just build-example hal_c3_connect_async` — passes for `riscv32imc-unknown-none-elf`.
- `just build-example hal_c3_connect_async_led` — passes.
- `just build-example hal_c6_connect_async_led` — passes for `riscv32imac-unknown-none-elf`.
- `just build-example hal_c6_connect_nonblocking_rgb` — passes (exercises both `-wifi` and the `-ws2812` git dep at the same `esp-hal 1.1.0` resolution).
- `just build-example hal_esp32s3_join` — passes for `xtensa-esp32s3-none-elf` (toolchain-gated).
- `just test-mqtt` / `just test-lora` / pure-crate host tests — must remain green; this upgrade should not touch them, but a green run confirms no accidental cross-crate breakage.
- Hardware:
  - ESP32-C3-DevKitM-1 — `hal_c3_connect_async_led` joins Wi-Fi, gets a DHCP lease, drives LED feedback.
  - ESP32-C6-DevKitC-1 — `hal_c6_connect_async_led` same outcome plus embassy multitask survival.
  - Heltec WiFi LoRa 32 V3 — `hal_esp32s3_join` boots and reaches the SX1262 init log line; OTAA join itself is gated by TTN hardware (Phase 5).

## State

- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-04-29 — Feature doc created.
  Triggered by the `rustyfarian-ws2812` April 2026 stack upgrade landing on 2026-04-29 (see that repo's `docs/features/esp-hal-stack-upgrade-april-2026-v1.md`).
  Streamlines the rustyfarian family onto a single coordinated April 2026 wave.
  No code changes yet — this doc is the design proposal only; awaiting design approval before implementation begins.
