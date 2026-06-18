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
- [x] Manually disconnecting the AP triggers the reconnect loop (via `wait_for_disconnect_async`) — validated 2026-06-18 by kicking the C3 from the router
- [x] Re-connecting the AP brings the example back online without reset — fresh `link up` with no ESP-ROM boot banner between cycles
- [x] Heap headroom remains stable across disconnect/reconnect cycles (no obvious leak) — disconnect-time free heap constant at 26988 B across 3 cycles; link-up variance (24764–26024 B) is transient association/DHCP buffers, non-monotonic

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
- [x] Hardware validation checklist complete — validated 2026-06-18 on ESP32-C3 Super Mini (connect + DHCP + reconnect loop + heap stability; see Session Log)
- [x] CHANGELOG entry

## Session Log

- 2026-04-08 — Feature doc created from `docs/embassy-integration-research.md`
- 2026-04-08 — Implemented: `examples/hal_c3_connect_async.rs` using `#[esp_rtos::main]` with two spawned tasks (`wifi_task` + `net_task`). Destructures `AsyncWifiHandle` (stack is `Copy`, keeps main's reference while moving controller/runner into tasks). `esp_alloc::heap_allocator!(size: 72 * 1024)` — single-region on C3. `WiFiManager::init_async` internally calls `esp_rtos::start()` via `init_inner`, which works from inside the embassy executor created by `#[esp_rtos::main]` (the macro creates the executor but does not start the RTOS — that is still the user's/library's responsibility). Added `[[example]] required-features = ["esp32c3", "rt", "embassy"]` to the crate Cargo.toml. `scripts/build-example.sh` grew a `*_async*` case that appends the `embassy` feature automatically, mirroring the existing `*_rgb*` pattern. `just fmt`, `just verify`, and `just build-example hal_c3_connect_async` all pass clean.
- 2026-04-10 — Fixed `scripts/flash.sh` missing the `*_async*` → `embassy` feature detection that `build-example.sh` already had. Hardware validation on real ESP32-C3: build, flash, Wi-Fi connect, and DHCP lease all confirmed working. AP reconnect loop test still pending.
- 2026-06-18 — **Reconnect-loop hardware validation COMPLETE; build regression found + fixed first.** Resuming the trailing validation, `just build-example hal_c3_connect_async` no longer compiled (8 errors): `embassy-executor` / `embassy-time` were unresolved, cascading into misleading `no method unwrap found for impl Future` errors because the unresolved `#[embassy_executor::task]` attribute left the task fns untransformed. Root cause: `rustyfarian-esp-hal-wifi` never declared `embassy-executor` / `embassy-time` — the library does not use them, only the three async examples do. Fixed by adding both as `[dev-dependencies]` (workspace-pinned, already vetted via the provisioning crate); this also unbroke `hal_c3_connect_async_led` / `hal_c6_connect_async_led`. Added free-heap logging to `wifi_task` on link-up and disconnect so reconnect success and leak-detection are observable on serial. On-hardware result (ESP32-C3 Super Mini, kicked from the router 3×): reconnect loop fires, re-associates with no reset, and the disconnect-time free heap is constant at 26988 B across all cycles (no leak; link-up readings 24764–26024 B are transient buffers). All validation boxes ticked.

---

## Debugging Session: AuthenticationExpired on WPA2 AP (2026-05-15)

### Environment

- Hardware: ESP32-C3 Super Mini
- Network: WPA2 AP with two virtual SSIDs on the same physical radio
- Crate stack: `rustyfarian-esp-hal-wifi` v0.2.1, `esp-radio 0.18.0`, `esp-rtos 0.2.0`
- Reference: `rustyfarian-esp-idf-wifi` using `esp-idf-svc` connects without issue on the same C3 board

### Symptom

`connect_async()` always fails with:

```
connect failed: Disconnected(DisconnectedStationInfo {
    ssid: "<ssid>",
    reason: AuthenticationExpired,
    rssi: -33
})
```

`WIFI_REASON_AUTH_EXPIRE` = reason code 2.
The AP sends a Deauthentication frame before the WPA2 4-way handshake completes.
Signal strength is excellent (-33 to -40 dBm) — not a range issue.

### Key facts established

- Two virtual SSIDs on the same physical AP; same channel 11.
- Secondary SSID appeared in `scan_async` results; primary target SSID did NOT appear in the scan despite excellent signal.
- The ESP-IDF C stack is used by both IDF and esp-radio; differences are in how they call it.
- esp-radio 0.18.0 `wifi_init_config_t` has `nvs_enable: 0` — NVS (PMKSA cache) is disabled.
- `apply_sta_config` in esp-radio sets `pmf_cfg: { capable: true, required: false }` (hardcoded, not configurable via `StationConfig`).
- `StationConfig::default()` fields: `auth_method: Wpa2Personal`, `failure_retry_cnt: 1`, `beacon_timeout: 6`, `scan_method: Fast`.
- These map to identical `wifi_sta_config_t` C fields as `esp-idf-svc`'s `ClientConfiguration::default()` — no difference found at the C config level.

### Failed attempt 1 — set_config before every connect_async

**Hypothesis:** `scan_async` clears the station config stored in the Wi-Fi driver. After the scan, `connect_async` runs with empty SSID/password and the AP rejects the association.

**Change:** Added `controller.set_config(&Config::Station(station))` inside the `wifi_task` loop, immediately before every `connect_async()` call. Also applied to both LED examples and `lib.rs`.

**Result (hardware log):**
```
Scanning...
  AccessPointInfo { ssid: "<secondary-ssid>", channel: 11,
    signal_strength: -73, auth_method: Some(Wpa2Personal), ... }
Waiting for DHCPv4 lease...
connect failed: Disconnected(..., reason: AuthenticationExpired, rssi: -40)
```

Scan produced output (confirmed set_config was applied). Primary SSID still not in scan results. Auth still fails. **Hypothesis disproved** — the station config was not the cause.

### Failed attempt 2 — remove scan_async before connecting

**Hypothesis:** The full channel scan (active, 10–20 ms per channel) puts the radio in a post-scan state that adds latency to the subsequent auth exchange. The target SSID is not in the scan's BSSID→channel cache, so `connect_async` must probe for it internally, adding further latency. Combined, the ESP32's own auth timer expires before the AP's response arrives. Supporting reasoning: the IDF variant does not scan at all and succeeds.

**Change:** Removed `scan_async` and its import from `hal_c3_connect_async.rs`. Cleaned up stale "scan clears config" comments in LED examples and `lib.rs`.

**Result (hardware log):**
```
Initializing Wi-Fi (async)...
INFO - Wi-Fi configured, power save: None
Waiting for DHCPv4 lease...
connect failed: Disconnected(..., reason: AuthenticationExpired, rssi: -33)
connect failed: Disconnected(..., reason: AuthenticationExpired, rssi: -33)
```

Scan removal made no difference. **Hypothesis disproved.** The scan was not interfering with auth timing.

### Resolution — TX power (2026-05-18)

**Root cause confirmed:** ESP32-C3 Super Mini PCB antenna reflects RF back into the chip at full TX power (~20 dBm), corrupting WPA2 auth frames.
Reproduced on a phone hotspot (isolated from AP-specific configuration); fixed by calling `esp_wifi_set_max_tx_power(34)` (8.5 dBm) after `set_config` triggers `esp_wifi_start`.
See `docs/project-lore.md` "esp-hal April 2026 Stack" for the full entry; fix lives in `WiFiManager::init_async` and `hal_c3_connect_async_upstream.rs`.

### Current state of the code (as of 2026-05-18)

- No `scan_async` in `hal_c3_connect_async.rs` (removed; the IDF variant never scanned either).
- `wifi_task` calls `set_config` before every `connect_async` — retained as defensive practice.
- `lib.rs` `init_async` calls `esp_wifi_set_max_tx_power(34)` immediately after `set_config`.
- `StationConfig`: `Wpa2Personal`, no explicit BSSID, no explicit channel.
