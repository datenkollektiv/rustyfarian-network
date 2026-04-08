# Feature: hal_c3_connect_async Example v1

First async hardware example for `rustyfarian-esp-hal-wifi`.
Demonstrates embassy-based Wi-Fi connection on ESP32-C3 using the `WiFiManagerAsync` API.
Serves as the hardware validation for both `embassy-feature-flag-v1` and `wifi-manager-async-v1`.

Depends on `embassy-feature-flag-v1` and `wifi-manager-async-v1`.

Source: `docs/embassy-integration-research.md` — example code sketch under Option A.

## Decisions

| Decision                                                                                                                   | Reason                                                                                                                 | Rejected Alternative                                                                        |
|:---------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------|
| Target ESP32-C3 first (not C6 or S3)                                                                                       | Blocking path is validated on C3; research notes a pending C6 bug in `esp-radio`; S3 is Xtensa and adds toolchain cost | C6 first — blocked by known bug; S3 first — Xtensa complexity on top of new async code      |
| `#[esp_rtos::main]` async entry point                                                                                      | Required by embassy-on-esp-rtos; matches the example sketch in the research doc                                        | Manual `Executor::new().run()` — more boilerplate, no benefit                               |
| Two `esp_alloc::heap_allocator!` calls: 64 KiB reclaimed IRAM + 36 KiB DRAM on C6-style chips; single 72 KiB call on C3    | Research doc explains Wi-Fi RX/TX DMA needs reclaimed IRAM on C6; C3 has contiguous SRAM and doesn't need the split    | Single-region heap on C6 — DMA failures; two-region on C3 — unnecessary complexity          |
| Example prints the acquired IP to `esp-println` and then idles in a 10 s loop                                              | Minimal viable demo; matches `hal_c3_connect` blocking example pattern                                                 | Opening a TCP socket and pinging a server — expands scope beyond "did it connect"           |
| Two spawned tasks: `wifi_task` (controller) + `net_task` (runner)                                                          | Canonical embassy-net pattern; keeps the main task free for application logic                                          | Single combined task — couples concerns, harder to extend                                   |
| Credentials via `env!("WIFI_SSID")` / `env!("WIFI_PASS")` at compile time                                                  | Matches pattern already used by `hal_c3_connect` and `idf_esp32s3_join`; no runtime cost, no secrets in repo           | `option_env!` with defaults — risks shipping an example that "works" with empty credentials |
| Example lives in `crates/rustyfarian-esp-hal-wifi/examples/hal_c3_connect_async.rs`                                        | Sibling to existing blocking example; discoverable                                                                     | Separate examples crate — unnecessary indirection                                           |
| Gated behind `#![cfg(feature = "embassy")]` at the example level                                                           | Prevents the example from breaking default-feature builds; matches how feature-gated examples work elsewhere           | Always-on — forces embassy deps into default builds, violates feature flag design           |
| `scripts/build-example.sh` routes `hal_c3_connect_async` the same way as `hal_c3_connect`, with `--features embassy` added | Existing script already handles `hal_*` prefixed examples for bare-metal; only the feature flag differs                | New dedicated script — duplication of toolchain sourcing and target selection logic         |

## Constraints

- Must build via `just build-example hal_c3_connect_async`
- Must flash and run on real ESP32-C3 hardware
- Must acquire a DHCPv4 lease from a real access point and print the IP
- Must not regress `hal_c3_connect` (blocking example)
- `just verify` must remain green — the example does not enter the default verification build because it requires the `embassy` feature
- `just check-embassy` (added in feature 1) should cover the example in `cargo check` form if feasible

## Validation checklist (on hardware)

- [x] `just build-example hal_c3_connect_async` succeeds
- [x] `just flash hal_c3_connect_async` flashes cleanly
- [x] Serial output shows "Wi-Fi connected" and a valid IP address from DHCP
- [x] Example continues running without panic for at least 5 minutes
- [ ] Manually disconnecting the AP triggers the reconnect loop (via `wait_for_event(StaDisconnected)`)
- [ ] Re-connecting the AP brings the example back online without reset
- [ ] Heap headroom remains stable across disconnect/reconnect cycles (no obvious leak)

## Open Questions

- [x] Is the ESP32-C3 heap layout a single `heap_allocator!(size: 72 * 1024)` call, or should it also split into reclaimed + DRAM regions? — Single 72 KiB call; C3 has contiguous SRAM, no need for the C6 two-region split
- [x] Should the example include a visible LED indicator? — Out of scope; see `led-task-embassy-v1` (future feature)
- [x] Do we need a `build.rs` / `sdkconfig.defaults` for bare-metal? — No; pure `esp-hal` + `esp-radio` + `esp-rtos`, no ESP-IDF involvement
- [x] Bootloader situation on C3 bare-metal? — Use espflash's bundled bootloader, same as the existing `hal_c3_connect` blocking example (no custom routing needed)

## State

- [x] Design approved
- [x] `embassy-feature-flag-v1` landed (blocker)
- [x] `wifi-manager-async-v1` landed (blocker)
- [x] Example file created (`crates/rustyfarian-esp-hal-wifi/examples/hal_c3_connect_async.rs`)
- [x] `just build-example hal_c3_connect_async` succeeds (release profile, `riscv32imc-unknown-none-elf`, all deps compile clean)
- [ ] Hardware validation checklist complete — **connect + DHCP verified; AP reconnect loop still open**
- [x] CHANGELOG entry

## Session Log

- 2026-04-08 — Feature doc created from `docs/embassy-integration-research.md`
- 2026-04-08 — Implemented: `examples/hal_c3_connect_async.rs` using `#[esp_rtos::main]` with two spawned tasks (`wifi_task` + `net_task`). Destructures `AsyncWifiHandle` (stack is `Copy`, keeps main's reference while moving controller/runner into tasks). `esp_alloc::heap_allocator!(size: 72 * 1024)` — single-region on C3. `WiFiManager::init_async` internally calls `esp_rtos::start()` via `init_inner`, which works from inside the embassy executor created by `#[esp_rtos::main]` (the macro creates the executor but does not start the RTOS — that is still the user's/library's responsibility). Added `[[example]] required-features = ["esp32c3", "rt", "embassy"]` to the crate Cargo.toml. `scripts/build-example.sh` grew a `*_async*` case that appends the `embassy` feature automatically, mirroring the existing `*_rgb*` pattern. `just fmt`, `just verify`, and `just build-example hal_c3_connect_async` all pass clean.
- 2026-04-10 — Fixed `scripts/flash.sh` missing the `*_async*` → `embassy` feature detection that `build-example.sh` already had. Hardware validation on real ESP32-C3: build, flash, Wi-Fi connect, and DHCP lease all confirmed working. AP reconnect loop test still pending.
