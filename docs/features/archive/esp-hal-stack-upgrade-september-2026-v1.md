# Feature: esp-hal Stack Upgrade — September 2026 Release Wave v1

Upgrade bare-metal stack: `esp-hal` 1.1.0→1.2.2, `esp-radio` 0.18.0→1.0.0-beta.1 (pre-release, exact-pin for deterministic resolution, will re-pin to 1.0.0 stable).
Coordinated with `rustyfarian-esp-hal-ws2812` 0.7.0 (published 2026-09-25); maintains unified `esp-hal 1.2.2` feature graph across Wi-Fi, LoRa, OTA, and provisioning.

## Version Table

| Crate                    | Current (`Cargo.lock`) | Target       | Released   |
|:-------------------------|:-----------------------|:-------------|:-----------|
| `esp-hal`                | 1.1.0                  | 1.2.2        | 2026-09-18 |
| `esp-rtos`               | 0.3.0                  | 0.4.0        | 2026-08-26 |
| `esp-radio`              | 0.18.0                 | 1.0.0-beta.1 | 2026-09-15 |
| `esp-alloc`              | 0.10.0                 | 0.11.0       | 2026-08-26 |
| `esp-bootloader-esp-idf` | 0.5.0                  | 0.6.0        | 2026-08-26 |
| `esp-storage`            | 0.9.0                  | 0.10.0       | 2026-08-26 |
| `esp-println`            | 0.17.0                 | 0.18.0       | 2026-08-26 |
| `esp-backtrace`          | 0.19.0                 | 0.20.0       | 2026-08-26 |

- Ecosystem primary wave 2026-08-26; esp-hal saw maintenance releases (1.2.0, 1.2.1, 1.2.2); `esp-radio` 1.0.0-beta.1 (2026-09-15) pre-release required for esp-hal 1.2 compatibility.
- Embassy/registry unchanged: `embassy-executor =0.10.0`, `embassy-net =0.8.0`, `embassy-sync =0.8.0`, `embassy-time =0.5.1`.
- Transitive: `esp-sync 0.3.0`, `esp-radio-rtos-driver 0.4.2`, `esp-phy 0.3.0`, `esp-rom-sys 0.1.5`, `esp-config 0.8.0`, `esp-metadata-generated 0.5.3`; `embassy-usb-driver` 0.2.1→0.2.2.

## Scope

**In scope:** `rustyfarian-esp-hal-network` driver + examples (`hal_c3/c6` connect/provision, `hal_esp32s3` join), workspace `Cargo.toml` dependencies, `rustyfarian-esp-hal-ws2812` 0.7, `pennant` 0.7, `Cargo.lock`.

**Out of scope:** `rustyfarian-esp-idf-network` (separate bump), `juggler` (no esp-hal dep).

## API Changes Migrated

- `esp-rtos` 0.4: `SoftwareInterruptControl` removed; `WiFiConfigExt::with_peripherals` now takes `FROM_CPU_INTR0`; field `sw_interrupt` → `from_cpu_intr0`; five examples updated.
- `esp-radio`: `WifiController::new(WIFI, cfg)` replaces `esp_radio::wifi::new`; `Interface::station()` / `::access_point()` singletons; `Runner<'static, Interface>` type; `net_task` signatures updated.
- `esp-radio` config: `Ssid`/`Password` via `try_from(&str)` map to `WifiError::Driver`; STA always `Wpa2Personal(pw)` (empty allowed); AP `Some(pw)`→`Wpa2Personal`, `None`→`Open`; `connect_async` returns `Result<sta::ConnectedInfo, ConnectionError>`.
- `esp-storage` 0.10: `embedded-storage` trait impls now opt-in feature; OTA manager uses inherent `FlashRegion::capacity()`/`write()` methods.
- `esp-alloc` 0.11 / `esp-backtrace` 0.20 / `esp-println` 0.18: chip features mandatory; `heap_allocator!` macro unchanged; no code changes needed.

## Decisions

| Decision                                       | Reason                                                              | Rejected Alternative              |
|------------------------------------------------|---------------------------------------------------------------------|-----------------------------------|
| Take `esp-radio` 1.0.0-beta.1 pre-release      | esp-hal 1.2 has no stable equivalent; unifies with ws2812 0.7.0     | Defer until 1.0.0 stable          |
| Exact-pin as `"=1.0.0-beta.1"`                 | Deterministic resolution; caret would drift                         | Caret constraint                  |
| STA TX power: inherit 5 dBm default            | esp-radio 1.0.0-beta.1 default 5 dBm; forcing 8.5 would raise power | Clamp to 8.5 dBm                  |
| OTA uses inherent `FlashRegion` methods        | Upstream direction; avoids feature gate                             | Enable `embedded-storage` feature |
| Declare `rust-version = "1.95"` on hal-network | Only hal tier needs MSRV                                            | Workspace-wide                    |

## Constraints

- `--release` profile required for bare-metal builds; hardware validation mandatory before "complete" declaration.
- Public API change: `with_peripherals` / `with_ap_peripherals` parameters and `Runner<'static, Interface>` type only.
- Coordinated `esp-hal 1.2.2` feature graph across Wi-Fi / LoRa / OTA / provisioning drivers.

## Open Questions

- [ ] Runtime defaults changed in beta.1: TX 5 dBm (vs 8.5), `sta_disconnected_pm`, ESP-IDF 6.1 blobs; TX policy validated C3 2026-09-26 (no `AuthenticationExpired` on join).
- [ ] High-power TX (`TxPowerLevel::High/Max`) on ESP32-C3 Super Mini untested; 5 dBm default validated 2026-09-26.
- [x] `esp-bootloader-esp-idf` 0.6 app-descriptor on Xtensa — YES, validated Heltec V3 2026-09-26.
- [ ] When `esp-radio` 1.0.0 stable ships: re-pin immediately, mandatory CHANGELOG entry.

## Hardware Validation Checklist

Hardware validation complete (2026-09-26): all five rows signed off (rows 1–2, 5 PASS; rows 3–4 PASS* with reboot-persistence caveat accepted).

| # | Check                                                                  | Board                  | Result | Date          |
|:--|:-----------------------------------------------------------------------|:-----------------------|:-------|:--------------|
| 1 | `just run hal_c3_connect_async_led` — STA join, DHCP, LED blink→steady | DevKitM-1, Super Mini  | PASS   | 2026-09-25/26 |
| 2 | `just run hal_c6_connect_async_led` — STA, DHCP, embassy, ws2812 0.7   | ESP32-C6-DevKitC-1     | PASS   | 2026-09-26    |
| 3 | `just run hal_c6_provision_mqtt` — SoftAP portal E2E, DHCP, reboot STA | ESP32-C6-DevKitC-1     | PASS*  | 2026-09-26    |
| 4 | `just run hal_c3_provision_mqtt` — same on C3                          | ESP32-C3-DevKitM-1     | PASS*  | 2026-09-26    |
| 5 | `just run hal_esp32s3_join` — SX1262 bring-up, SPI/GPIO, bootloader    | Heltec WiFi LoRa 32 V3 | PASS   | 2026-09-26    |

- Row 3–4 PASS* = E2E validated to credential commit; reboot-persistence not exercised (library no-reboot contract; can close via manual power-cycle).
- Row 5 PASS = SX1262 bring-up only (reset, BUSY, TCXO, RF-switch, register read); bare-metal OTAA join unvalidated, out of scope.

## Compile Verification

All pass (2026-09-26, re-verified after source changes):

- ✅ `just check-hal` (all HAL domains, RISC-V targets)
- ✅ `just test-provisioning-hal`, `just test-ota-hal`, `just test` (1669 passed)
- ✅ `just fmt`, `just verify`, `just audit`, `just deny`
- ✅ All 6 HAL examples build (release, riscv32/xtensa targets)

Note: `just verify` checks IDF target only (pre-commit speed); `clippy-hal` (all HAL × C3/C6 with `-D warnings`) runs in `build-examples` CI job.

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing (compile + host)
- [x] Documentation updated
- [x] Hardware validated
