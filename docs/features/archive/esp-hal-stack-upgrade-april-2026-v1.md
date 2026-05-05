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

- [x] **Is there already a `rustyfarian-ws2812` branch/commit pinned to the April 2026 wave?** — Resolved 2026-04-29: yes, `rustyfarian-ws2812`'s `Cargo.toml` already pins `esp-hal=1.1.0`, `esp-rtos=0.3.0`, `esp-radio=0.18.0`, etc.  Decision: switch the cross-repo deps from git URLs to **local sibling paths** (`../rustyfarian-ws2812/crates/*`) for the duration of the migration, since the network workspace is co-developed with ws2812 on this machine.  This sidesteps git-rev pinning entirely and keeps the esp-hal feature graph unified.
- [x] **Does `esp-hal 1.1.0` change any GPIO/SPI/RMT API used by `EspHalLoraRadio` or by the SX1262 bring-up in `hal_esp32s3_join`?** — Resolved 2026-04-29: SPI was unaffected; the LoRa example builds clean for `xtensa-esp32s3-none-elf` after the workspace pin update.  RMT TX was reshaped (`configure_tx(pin, config)` → `configure_tx(&config).unwrap().with_pin(pin)`) but only the deleted `hal_c6_connect_nonblocking_rgb` example used RMT in this workspace; the surviving `hal_c6_connect_async_led` was migrated to the new pattern.
- [x] **Does `esp-radio 0.18` change the `wifi` feature so that the `-wifi` crate's feature forwarding still produces a working `WifiController`/`WifiDevice` pair?** — Resolved 2026-04-29: **yes — major breakage.**  The `smoltcp` Cargo feature was removed entirely; the `wifi` feature now pulls `embassy-net-driver` instead of providing a `smoltcp::phy::Device` impl.  `WifiDevice` was renamed to `Interface` (no MODE generic), `ModeConfig` → `Config`, `ClientConfig` → `StationConfig` (in private-but-public `sta` submodule), `WifiError::Disconnected` is now a tuple variant carrying `DisconnectedStationInfo`, sync `connect`/`disconnect`/`start`/`wait_for_event` are gone (only `connect_async`/`disconnect_async`/`wait_for_disconnect_async` remain), and `wifi::new()` lost its radio_ref parameter.  Decision: drop the entire sync surface (`WiFiManager::init`, `init_with_led`, `wait_connected`, `get_ip`, `take_sta_device`, `WifiDriver` trait impl, `S: StatusLed` generic) as a deliberate v0.2.0 breaking change; the async path is now the only public surface and the `embassy` feature is effectively required.
- [x] **Does `esp-rtos 0.3` change `#[esp_rtos::main]` ergonomics?** — Resolved 2026-04-29: no functional change observed in the surviving examples; the macro still accepts `async fn main(spawner: Spawner)` and `esp_rtos::start(timg.timer0, sw_ints.software_interrupt0)` keeps its signature.
- [x] **Does `embassy-net 0.8` change the `Stack` / `Runner` constructor or the `dhcpv4` config struct?** — Resolved 2026-04-29: the `embassy_net::new(device, NetConfig::dhcpv4(DhcpConfig::default()), resources, seed)` call shape compiles unchanged once `WifiDevice` is renamed to `Interface` for the `Runner<'static, Interface<'static>>` generic.
- [x] **Does `esp-alloc 0.10` rename or restructure `heap_allocator!`?** — Resolved 2026-04-29: no — both the plain `heap_allocator!(size: N)` and the `#[esp_hal::ram(reclaimed)] heap_allocator!(size: N)` forms keep working unchanged in the C3 and C6 examples.
- [x] **Does `esp-bootloader-esp-idf 0.5` still produce a descriptor accepted by the IDF v5.3.3 bootloader?** — Deferred: build step passed, but full validation requires hardware flashing, which is not part of this migration task.  The espflash workaround in project-lore (`--bootloader <path>` + `--ignore-app-descriptor`) remains in place.
- [x] **Should the workspace add `embassy-sync = "=0.8.0"` explicitly to force unification?** — Resolved 2026-04-29: yes — added to workspace `Cargo.toml` at `=0.8.0` (matches what `esp-rtos 0.3` pulls transitively and what `rustyfarian-ws2812` pins).
- [x] **Is the `xtensa-esp32s3-none-elf` toolchain installed and ready on the development machine?** — Confirmed 2026-04-29: yes; `just build-example hal_esp32s3_join` builds clean in `release` profile (final binary linked).
- [x] **embassy-executor 0.10 spawn shape** — surfaced 2026-04-29 (was not on the original list): `Spawner::must_spawn` was removed; `#[embassy_executor::task]` macros now return `Result<SpawnToken, SpawnError>`.  All three async examples updated from `spawner.must_spawn(task(arg))` to `spawner.spawn(task(arg).unwrap())`.

## Validation Evidence (2026-04-30)

Build:

- ✅ `just verify` — clean (`fmt-check` + `cargo deny` + `cargo check` + `cargo clippy --all-targets --workspace -- -D warnings`).
- ✅ `just build-example hal_c3_connect_async` — `riscv32imc-unknown-none-elf`, release profile.
- ✅ `just build-example hal_c3_connect_async_led` — `riscv32imc-unknown-none-elf`, release profile.
- ✅ `just build-example hal_c6_connect_async_led` — `riscv32imac-unknown-none-elf`, release profile (also exercises the `-ws2812` local sibling dep at unified `esp-hal 1.1.0`).
- ✅ `just build-example hal_esp32s3_join` — `xtensa-esp32s3-none-elf`, release profile (LoRa stub + lorawan-device 0.12).
- N/A — `hal_c6_connect_nonblocking_rgb` was deleted; the sync RGB demo depended on the removed `WiFiManager::wait_connected` smoltcp DHCP loop.
- ✅ host pure-crate tests untouched (no source changes in `wifi-pure`, `lora-pure`, `espnow-pure`, `rustyfarian-network-pure`).

Hardware:

- ✅ **ESP32-C3-DevKitM-1** — `hal_c3_connect_async_led` joins Wi-Fi, gets a DHCP lease, LED feedback transitions blink → steady on association as expected (2026-04-30).
- ✅ **ESP32-C6-DevKitC-1** — `hal_c6_connect_async_led` joins Wi-Fi, gets a DHCP lease, embassy-net `Stack` runner survives, WS2812 LED transitions blue pulse → steady dim green on association (2026-04-30).
- ⏸ **Heltec WiFi LoRa 32 V3** — `hal_esp32s3_join` builds clean against the new pins; on-device run is gated by Phase 5 TTN v3 EU868 hardware availability and is **not** a prerequisite for this upgrade.  The Phase 5 validation checklist in `docs/ROADMAP.md` covers it.

Tooling fix surfaced during validation (also delivered as part of this feature):

- `scripts/detect-port.sh` — narrows espflash auto-detect to USB serial devices so paired Bluetooth ports stop hijacking flash attempts on macOS.  `flash.sh`, `just run`, `just monitor`, and `just erase-flash` all use it.

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log

- 2026-04-29 — Feature doc created.
  Triggered by the `rustyfarian-ws2812` April 2026 stack upgrade landing on 2026-04-29 (see that repo's `docs/features/esp-hal-stack-upgrade-april-2026-v1.md`).
  Streamlines the rustyfarian family onto a single coordinated April 2026 wave.
  No code changes yet — this doc is the design proposal only; awaiting design approval before implementation begins.
- 2026-04-29 — Implementation complete on branch `esp-hal-stack-upgrade-april-2026`.
  - Workspace `Cargo.toml` exact-pinned to the April 2026 wave; ws2812 cross-repo deps switched from git → local sibling path (`../rustyfarian-ws2812/crates/*`).
  - `rustyfarian-esp-hal-wifi/Cargo.toml` reworked: per-crate version literals → `workspace = true`; the `esp-radio/smoltcp` feature was removed from the chip features and the direct `smoltcp` dep was dropped.
  - `rustyfarian-esp-hal-wifi/src/lib.rs` substantially rewritten: the synchronous `WiFiManager::init`/`init_with_led`/`wait_connected`/`get_ip`/`take_sta_device`/`WifiDriver` trait impl was removed (no backing driver in `esp-radio 0.18` since the `smoltcp::phy::Device` impl was deleted upstream).  `WiFiManager` is now a unit struct exposing only `init_async`; the `S: StatusLed` generic was dropped.  Renames applied: `WifiDevice` → `Interface`, `ModeConfig::Client(ClientConfig)` → `Config::Station(StationConfig)`, `Interfaces.sta` → `.station`, `WifiError::Disconnected` tuple payload.  `set_config` is now idempotent and implicitly starts/connects, so the explicit `start()`/`connect()` chain is gone.
  - Five sync-only examples deleted: `hal_c3_connect`, `hal_c6_connect`, `hal_c3_wifi_raw`, `hal_c6_wifi_raw`, `hal_c6_connect_nonblocking_rgb`.  The three remaining `hal_*_async*` examples were migrated for the renames + the embassy 0.10 spawn shape (`spawner.must_spawn(task(...))` → `spawner.spawn(task(...).unwrap())`) + the esp-hal 1.1 RMT TX builder split (`configure_tx(pin, config)` → `configure_tx(&config).unwrap().with_pin(pin)`).
  - `rustyfarian-esp-hal-lora/Cargo.toml` migrated to `workspace = true` for `esp-hal`, `esp-bootloader-esp-idf`, `esp-println`; no source changes needed for the LoRa stub.
  - Validation: `just fmt && just verify` clean; all four hardware example builds green — `hal_c3_connect_async`, `hal_c3_connect_async_led`, `hal_c6_connect_async_led` (riscv32imc/imac-unknown-none-elf) and `hal_esp32s3_join` (xtensa-esp32s3-none-elf).  Hardware re-test on actual boards is the remaining manual gate (not part of this code-only task).
  - CHANGELOG `[Unreleased]` updated; new "esp-hal April 2026 Stack" section added to `docs/project-lore.md` capturing the rename map, the smoltcp removal, the RMT split, the embassy spawn shape, and the `with_ssid` `&str` inference quirk.
- 2026-04-30 — Hardware validation complete; feature closed.
  Added `scripts/detect-port.sh` to narrow espflash's auto-detect to USB serial devices (Bluetooth ports were hijacking the probe on macOS) and wired it into `flash.sh`, `just run`, `just monitor`, and `just erase-flash`.
  Flashed and ran on real hardware:
  - ESP32-C3-DevKitM-1 with `hal_c3_connect_async_led` — joined Wi-Fi, DHCP lease acquired, LED feedback transitions correctly on association/disassociation.
  - ESP32-C6-DevKitC-1 with `hal_c6_connect_async_led` — joined Wi-Fi, DHCP lease acquired, embassy multitask survival confirmed, WS2812 LED transitions blue pulse → dim green on association.
  ESP32-S3 LoRa hardware run remains gated by Phase 5 TTN v3 EU868 hardware availability per `docs/ROADMAP.md` — not a prerequisite for closing this feature.
  Two project-lore entries added: the auto-detect Bluetooth-port pitfall, and the orphaned `espflash monitor` port-lock pitfall (`lsof` to diagnose).
