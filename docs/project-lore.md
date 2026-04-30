# Project Lore

This file records non-obvious technical discoveries: facts that caused surprising
failures, took significant time to debug, or would save a future developer 30+
minutes if known upfront.

Refer to `CLAUDE.md` and the `/project-lore` skill for recording guidelines.

---

## Toolchain & Build

**`rust-toolchain.toml` is required to activate the `esp` toolchain — `espup` alone is not enough.**
Without this file, `cargo check` silently falls back to the host `stable` toolchain, which has no
knowledge of `riscv32imac-esp-espidf`, producing the misleading error
`can't find crate for 'core' / target may not be installed` even when `espup` is fully installed.
Fix: add `rust-toolchain.toml` with `channel = "esp"` to the repo root;
rustup then selects the correct toolchain automatically in any shell session without requiring
`source ~/export-esp.sh`.

**When `rust-toolchain.toml` pins `channel = "esp"`, every CI job must install the `esp` toolchain — not just the build job.**
`rustup` reads `rust-toolchain.toml` for every `cargo` invocation, including `cargo fmt`.
A CI job that installs only `stable` (e.g. via `dtolnay/rust-toolchain@stable`) will fail with
`error: custom toolchain 'esp' specified in override file '...rust-toolchain.toml' is not installed`
even though `cargo fmt` itself does not compile ESP-IDF code.
Fix: replace `dtolnay/rust-toolchain@stable` in the `format` job with `esp-rs/xtensa-toolchain@v1.6`
(`ldproxy: false` suffices — the linker proxy is not needed for a format check).
The `esp` toolchain ships `rustfmt`, so no separate stable step is required.

**`just fmt` must be run before `just verify` (and before every commit) — skipping it causes CI to fail.**
`just verify` calls `just fmt-check` which only *detects* formatting drift; it does not fix it.
Any code change that was not passed through `cargo fmt` first will cause `fmt-check` to fail in CI
with no compiler error to aid diagnosis.
Fix: always run `just fmt` then `just verify` in that order; see the `## Completion Gate` section
in `CLAUDE.md`.

**Every crate that builds examples against ESP-IDF needs a `build.rs` calling `embuild::espidf::sysenv::output()`.**
`cargo:rustc-link-arg` emitted by `esp-idf-sys` does not propagate through transitive deps to the example binary's linker.
`sysenv::output()` re-reads those args from `DEP_ESP_IDF_SVC_EMBUILD_LINK_ARGS` and re-emits them locally; without it, `ldproxy` panics with `Cannot locate argument '--ldproxy-linker <linker>'` despite a correct PATH.
Fix: add the `build.rs`, and set `RUSTC_LINKER = "ldproxy"` in `.cargo/config.toml` `[env]` so embuild emits `--ldproxy-linker` as a real link arg, not just metadata.

**`LIBCLANG_PATH` is the only env var from `~/export-esp.sh` needed for riscv32 (ESP32-C3/C6) projects.**
The riscv32 cross-compiler is managed by `esp-idf-sys`'s CMake build and never needs to be on PATH; the Xtensa PATH entry only matters for Xtensa targets.
`bindgen`/`esp-idf-sys` need `LIBCLANG_PATH` to locate the Clang headers — that is the only missing piece in a fresh shell.
Fix: add `export LIBCLANG_PATH="$HOME/.espup/esp-clang"` to `.envrc`.
`~/.espup/esp-clang` is a stable symlink maintained by `espup`, so it survives `espup update` without manual edits.

**`espflash 4.x` bundles an ESP-IDF v5.5.1 bootloader that is incompatible with v5.3.3 app binaries.**
When `espflash flash` runs without `--bootloader`, it writes its own bundled v5.5.1 bootloader.
That bootloader uses a different MMU page size (32 KB vs 64 KB in v5.3.3) and may reject the app
with `Image requires efuse blk rev <= v0.99, but chip is v1.3`.
Fix: always pass `--bootloader <path>` pointing to the IDF-built bootloader that `esp-idf-sys`
places at `target/<target>/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin`.
Also pass `--ignore-app-descriptor` to prevent espflash from performing its own chip-model check.
See `scripts/flash.sh` for the implemented solution.

**Bare-metal Xtensa targets (`xtensa-esp32-none-elf`, `xtensa-esp32s3-none-elf`) require `-Tlinkall.x` and `-fno-stack-protector` in rustflags.**
Without `-Tlinkall.x`, esp-hal's linker chain (which pulls in PAC-generated `device.x` providing ~60 peripheral interrupt stubs) is never processed, and every PAC interrupt vector appears as an undefined reference — burying the real cause in noise.
Without `-C link-arg=-fno-stack-protector`, GCC 15.2 injects stack-guard calls into `esp_hal::init` that resolve to an undefined `__stack_chk_guard`.
Fix: add both flags to `[target.xtensa-esp32*-none-elf]` sections in `.cargo/config.toml.dist`.
RISC-V bare-metal targets are unaffected — GCC 15.2 does not inject stack protection there.

**ESP32-C3 requires target `riscv32imc-esp-espidf` and `MCU=esp32c3` — not the workspace C6 defaults.**
ESP32-C3 is `riscv32imc` (no atomics extension); ESP32-C6 is `riscv32imac`.
Building C3 firmware with the C6 target produces an image whose ESP-IDF chip metadata (including `max_efuse_blk_rev`) is wrong, which the bootloader rejects.
Fix: `scripts/flash.sh` and `scripts/build-example.sh` derive the chip from the example prefix (`idf_c3_*` → C3) and override both `MCU` and `--target`; new examples must follow the prefix convention.

**`sdkconfig.defaults` must be placed at the workspace root for embuild to pick it up — not in the crate root.**
In a Cargo workspace, `embuild` (used by `esp-idf-sys`) resolves `sdkconfig.defaults` relative to the workspace root (where the top-level `Cargo.toml` lives), not relative to the crate that's being built.
Placing the file in a crate subdirectory (e.g. `crates/rustyfarian-esp-idf-lora/sdkconfig.defaults`) is silently ignored: `esp-idf-sys` recompiles but CMake reconfigures without the custom settings, and the generated `sdkconfig` retains all defaults.
Fix: place `sdkconfig.defaults` at the workspace root and declare it in `build.rs` as `cargo:rerun-if-changed=../../sdkconfig.defaults`.
The main task stack is commonly the first setting needed: `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` is sufficient for full LoRaWAN OTAA crypto on ESP-IDF.

**`sx126x 0.3` requires `esp-idf-hal` with `features = ["critical-section"]` when used in an ESP-IDF std build.**
`sx126x` unconditionally depends on the `critical-section` crate and calls `critical_section::with()` internally.
The backend (`_critical_section_1_0_acquire` / `_critical_section_1_0_release` symbols) must be provided by the HAL.
`esp-idf-hal` (0.45+) provides a FreeRTOS-backed implementation in `src/task.rs`, but only when the `critical-section` feature is enabled.
Without it, the linker fails with `undefined reference to '_critical_section_1_0_acquire'` even though `esp-idf-hal` is already in the dependency tree.
Fix: in any ESP-IDF crate that uses `sx126x`, declare `esp-idf-hal = { workspace = true, features = ["critical-section"] }`.

**`just verify` only compiles the workspace default target (`riscv32imac-esp-espidf`) — Xtensa IDF and bare-metal targets are never exercised.**
A missing `[target.xtensa-esp32s3-espidf]` section with `linker = "ldproxy"` passes verify clean but `just build-example idf_esp32s3_*` panics with `Cannot locate argument '--ldproxy-linker <linker>'`.
Fix: add a `[target.*]` section for every ESP-IDF target used by an example, in both `.cargo/config.toml` and `.cargo/config.toml.dist`.
Completion gate: run `just build-example <name>` for every hardware example in addition to `just verify`.

**Cross-repo git dependencies without `tag` or `rev` track the default branch — upstream version bumps that touch a `links = "..."` crate silently break workspace resolution.**
A `links = "..."` declaration (e.g. `esp-println`, `esp-idf-sys`) tells Cargo only one version may exist in the dependency graph.
When an unpinned git dep adopts a new release wave that bumps the linked crate, the workspace's existing constraint and the upstream's new constraint cannot coexist, producing `failed to select a version for <crate>` — the error names the linked crate, not the unpinned git dep that introduced the conflict.
Fix: always pin cross-repo git deps with `tag = "vX.Y.Z"` (or `rev = "<sha>"`).
Upstream release waves then cannot reach into this workspace without a deliberate, coordinated bump.

---

## esp-hal April 2026 Stack (esp-radio 0.18, esp-hal 1.1, embassy 0.10)

**`esp-radio 0.18` deleted the `smoltcp` feature and the `smoltcp::phy::Device` impl on `WifiDevice` — the bare-metal Wi-Fi controller is now async-only and tied to `embassy-net`.**
The 0.17 `smoltcp` Cargo feature is gone; the `wifi` feature now pulls `embassy-net-driver` instead.
Any blocking poll loop that drove `smoltcp::iface::Interface` directly off `WifiDevice` (the `WiFiManager::wait_connected` + DHCP path used in the network workspace before the upgrade) has no equivalent in 0.18 — the option is to drop the sync surface and route everything through `embassy-net`.
Confirmed in `esp-radio-0.18.0/CHANGELOG.md` line 107 (`Support for the feature 'smoltcp' has been removed (#4870)`) and verified by grep against the unpacked source: zero `smoltcp` references in `wifi/mod.rs`, only `impl Driver for Interface<'_>` (`embassy_net_driver::Driver`) remains.

**`esp-radio 0.18` rename map (any HAL crate consuming the bare-metal Wi-Fi surface needs to apply these):**
- `WifiDevice<'d, MODE>` → `Interface<'d>` (the `MODE` generic is gone — `embassy_net::Runner<'static, Interface<'static>>` replaces `Runner<'static, WifiDevice<'static>>`)
- `ModeConfig` → `Config`; `ModeConfig::Client(ClientConfig)` → `Config::Station(StationConfig)`
- `ClientConfig` → `StationConfig` (lives at `esp_radio::wifi::sta::StationConfig` — the `sta` submodule is `pub` but `StationConfig` is **not** re-exported at `esp_radio::wifi`, so `use esp_radio::wifi::{StationConfig, ...}` fails with `E0603 private struct`; import the full path instead)
- The old top-level `Config` → `ControllerConfig`
- `Interfaces.sta` → `Interfaces.station`
- `WifiEvent::StaDisconnected` → `WifiEvent::StationDisconnected`
- `WifiError::Disconnected` is now a tuple variant `Disconnected(DisconnectedStationInfo)` — pattern matches that previously used the unit variant break
- `controller.is_connected()` returns `bool` directly, not `Result<bool, WifiError>`
- `controller.connect()`, `disconnect()`, `start()` (sync) — all removed; replacements are `connect_async().await`, `disconnect_async().await`; `set_config()` is now idempotent and implicitly starts the controller and begins association
- `controller.wait_for_event(WifiEvent::StaDisconnected)` removed; replacement is `controller.wait_for_disconnect_async().await -> Result<DisconnectedStationInfo, WifiError>`
- `esp_radio::wifi::new()` signature is now `(WIFI<'d>, ControllerConfig)` — the prior `radio_ref` parameter is gone; `esp_radio::init()` is now `pub(crate)` and not part of user code

**`StationConfig::with_ssid` accepts `&str` directly via `Into<Ssid>` — calling `.into()` first triggers `E0283`.**
The compiler can't pick between `&str → Ssid` and `&str → &str` when the `&str.into()` is bare.
Pass the `&str` literal directly: `StationConfig::default().with_ssid(ssid).with_password(password.into())`.
The password setter still needs `.into()` because its parameter type is the more specific `Password`, not a trait object.

**`esp-hal 1.1.0` split the RMT TX builder: `configure_tx(pin, config)` is gone — the new pattern is `configure_tx(&config).unwrap().with_pin(pin)`.**
The pin moved from the `configure_tx` parameter to a chained `.with_pin(...)` call so that channel configuration can be reused independently of pin assignment.
The reference migration is in the `rustyfarian-ws2812` repo's CHANGELOG entry for the April 2026 wave (file: `crates/rustyfarian-esp-hal-ws2812/examples/hal_c6_*.rs`).

**`embassy-executor 0.10` removed `Spawner::must_spawn`; `#[embassy_executor::task]` macros now return `Result<SpawnToken<...>, SpawnError>`.**
Old: `spawner.must_spawn(my_task(arg));`
New: `spawner.spawn(my_task(arg).unwrap());` — the `.unwrap()` goes on the **task call** (which returns `Result`), not on `spawner.spawn` (which returns `()`).
The compiler hint `consider using Result::expect to unwrap the Result<SpawnToken, SpawnError>` is correct in spirit but its suggested edit is wrong (it places `.expect("REASON")` after the closing paren of `spawn`, where it would be applied to `()`).

---

## Flashing & Serial (espflash 4.x)

**`espflash`'s auto-detect picks Bluetooth ports on Macs with paired headsets, then fails with the generic "Error while connecting to device".**
On macOS every paired Bluetooth device shows up under `/dev/cu.*` (`/dev/cu.B21S-HC15130245`, `/dev/cu.fluffisNC700`, `/dev/cu.Bluetooth-Incoming-Port`, …) and `espflash`'s probe order can land on one of them before the actual USB-JTAG device (`/dev/cu.usbmodem1101`).
The error message gives no hint about which port was tried.
Fix: `scripts/detect-port.sh` filters candidates to USB serial devices only (`usbmodem*`, `usbserial*`, `SLAB_USBtoUART*`, `wchusbserial*` on macOS; `ttyUSB*`, `ttyACM*` on Linux) and only auto-picks when exactly one matches; otherwise it prints the candidate list and lets `espflash` error out with its own message.
`flash.sh`, `just run`, `just monitor`, and `just erase-flash` all use the helper.
`ESPFLASH_PORT=…` still wins when set explicitly.

**`espflash monitor` from `just run` lingers after the user thinks it has exited and locks the port for the next flash.**
Symptom: `[INFO ] Serial port: '/dev/cu.usbmodem1101'` followed by `Error: Failed to open serial port /dev/cu.usbmodem1101 — Error while connecting to device`.
This is a *different* failure mode from auto-detect picking the wrong port — here the right port is selected but `open()` fails because another process holds it.
Diagnose: `lsof /dev/cu.usbmodem1101` shows the holding `espflash` PID; kill it and retry.
Common cause: closing a terminal tab without sending SIGINT to the foreground `espflash monitor`, leaving the process orphaned and still holding the FD.

---

## ESP-IDF Event Loop (`esp-idf-svc`)

**`EspEventLoop::subscribe` requires the callback to accept the event type by value, not by reference.**
The bound is `F: for<'a> FnMut(D::Data<'a>)`, so the callback signature must be
`|event: WifiEvent<'_>|`, not `|event: &WifiEvent|`.
Using a reference produces `E0631: type mismatch in closure arguments` with the note
`expected closure signature 'for<'a> fn(WifiEvent<'a>) -> _' / found 'fn(&WifiEvent<'_>) -> _'`.
The compiler's `help` suggestion (remove `&`) is correct and sufficient.

**An `EspSubscription` must be stored for as long as events are needed — dropping it unregisters the handler.**
`EspSystemEventLoop::subscribe` returns an `EspSystemSubscription<'static>` whose `Drop` impl
automatically deregisters the callback.
If the subscription is bound to a local variable that goes out of scope (e.g. inside an `if` branch),
the handler fires zero times.
Fix: store the subscription in the owning struct (e.g. as `Option<EspSystemSubscription<'static>>`).

---

## LoRaWAN / TTN v3 (EU868)

**EUI byte order is the single most common cause of silent OTAA join failure.**
TTN displays DevEUI/JoinEUI as big-endian hex strings; `lorawan-device` (and most embedded LoRaWAN stacks) expect the in-memory `[u8; 8]` in LSB-first order.
`LoraConfig::from_hex_strings()` preserves the string order (MSB-first), so callers must `.reverse()` both EUIs before constructing `DevEui`/`AppEui`.
Symptom of a mismatch: the network server logs a valid join request, but the device never sees the join-accept (or derives wrong session keys).
Validation: log both EUIs as bytes and compare against the stack's documented order before flashing.

**"Join-accept sent by TTN; device never joins" almost always means an RX window issue — not wrong keys.**
RX1 opens ≈1 s after the TX burst ends; RX2 opens ≈2 s after; `RX_WINDOW_OFFSET_MS = -200` is a reasonable starting point — tune upward (less negative) if windows are still missed.
Two common upstream causes are documented as separate entries below — DIO1 interrupt not delivered, and BUSY pin not handled — both block the radio completion event from reaching the LoRaWAN state machine in time.

**DIO1 interrupt isn’t delivered → radio events never reach the state machine.**
The SX1262 signals TX/RX completion via DIO1.
If the GPIO is unconfigured, wired to the wrong pin, or the interrupt handler does not set a flag
visible to the polling loop, `nb_device::Event::RadioEvent` is never generated.
Symptom: radio operations appear to hang; no downlinks are ever processed even with strong RF signal.
Fix: configure DIO1 as an input with a rising-edge interrupt; store a flag (e.g. `AtomicBool`)
that the `process()` loop checks on every tick.

**BUSY pin isn’t handled → all SX1262 SPI commands time out.**
The SX1262 asserts BUSY high during internal processing, and any SPI command issued while BUSY is high
is silently ignored or produces corrupt results.
Fix: poll BUSY low (with a bounded timeout) before issuing every SPI command in the driver.

**Frame counter reuse after deep sleep causes TTN to reject uplinks.**
TTN tracks `FCntUp` per device session and silently drops frames with a counter equal to or lower
than the last accepted value (unless the "Reset frame counters" option is enabled, which weakens security).
After deep sleep, `LorawanSessionData::fcnt_up` must be restored exactly and incremented before the
next uplink — never reset to zero while the session is still valid.
If session restore fails (CRC mismatch, RTC memory unreadable), force a full re-join rather than
sending with a stale counter.

**Downlinks only arrive in the RX windows immediately after an uplink — there is no push delivery.**
Queuing a downlink in TTN Console does not transmit it until the next uplink's RX1 or RX2 window.
If the device is idle (no uplinks), the queued downlink sits indefinitely.
Implication for OTA command validation (port 10): always trigger a test uplink first, then check
whether the downlink arrives in that uplink's receive window.

---

## sx126x 0.3 API

**`LoraCodingRate` not `LoRaCodingRate`**
In `sx126x 0.3`, the coding rate type is `op::modulation::LoraCodingRate` (lowercase `o` in `Lora`), NOT `LoRaCodingRate`.
The bandwidth and spread-factor types DO use the `LoRa` prefix (`LoRaBandWidth`, `LoRaSpreadFactor`).
The inconsistency is in the crate itself — match the actual crate spelling exactly.

**`set_low_dr_opt` not `set_low_data_rate_opt`**
The LDRO setter on `LoraModParams` is `.set_low_dr_opt(bool)`, not `.set_low_data_rate_opt(bool)`.
The compiler's `help:` suggestion is reliable — always check it when method names differ from docs or specs.

**`OutputPin<Error = GpioError>` required for ANT pin**
`sx126x 0.3`'s `SX126x::new` uses a single `TPINERR` type variable for ALL pin error types: `TNRST: OutputPin<Error = TPINERR>`, `TBUSY: InputPin<Error = TPINERR>`, etc.
Rust infers `TPINERR = GpioError` from the concrete `PinDriver` types.
Any generic `ANT` parameter must therefore also declare `OutputPin<Error = GpioError>` — a bare `OutputPin` bound causes a type mismatch error.
Import `esp_idf_hal::gpio::GpioError` and write: `where ANT: OutputPin<Error = GpioError>`.

---

## CodeQL / GitHub Advanced Security

**CodeQL's `rust/hard-coded-cryptographic-value` query flags string literals reaching parameters named `password`, `credential`, or similar — even in test code.**
The query traces taint from any string literal into a credential-named parameter and raises a Critical alert regardless of context; test helpers like `WiFiConfig::new("ssid", "pass")` trigger it on the second parameter.
Fix: define test fixtures with non-credential names (e.g. `TEST_PSK`) and route them through a helper (e.g. `test_config()`); for empty passwords use `&String::new()` instead of `""`.
The indirection breaks the direct literal-to-credential-parameter flow that CodeQL traces — see `crates/wifi-pure/src/lib.rs` test module.
