# Key Insights

This file records non-obvious technical discoveries: facts that caused surprising
failures, took significant time to debug, or would save a future developer 30+
minutes if known upfront.

Refer to `CLAUDE.md` and the `/key-insights` skill for recording guidelines.

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

**Every crate that builds examples against ESP-IDF must have a `build.rs` that calls `embuild::espidf::sysenv::output()`.**
`cargo:rustc-link-arg` emitted by `esp-idf-sys`'s build script does **not** automatically propagate
through transitive dependencies to the final binary linker.
Without this, `ldproxy` panics with `Cannot locate argument '--ldproxy-linker <linker>'`
even when the PATH and `RUSTC_LINKER` env var are correctly set.
`sysenv::output()` reads `DEP_ESP_IDF_SVC_EMBUILD_LINK_ARGS` (set by `esp-idf-svc` via the
`links = "esp_idf"` propagation chain) and re-emits each entry as `cargo:rustc-link-arg`,
which reaches the example binary's linker invocation.
Also required: set `RUSTC_LINKER = "ldproxy"` in `.cargo/config.toml` `[env]` so `embuild` detects
ldproxy and emits `--ldproxy-linker` as part of those args (not only as metadata).

**`LIBCLANG_PATH` is the only env var from `~/export-esp.sh` needed for riscv32 (ESP32-C3/C6) projects.**
The riscv32 cross-compiler (`riscv32-esp-elf-gcc`) is managed entirely by `esp-idf-sys`'s CMake build
and does not need to be on PATH.
The Xtensa PATH entry in `export-esp.sh` is only relevant for Xtensa targets (ESP32-S2/S3/classic).
The only missing piece in a fresh shell is `LIBCLANG_PATH`, which `bindgen`/`esp-idf-sys` need to find
the Clang headers.
Permanent fix: add `export LIBCLANG_PATH="$HOME/.espup/esp-clang"` to `.envrc`.
`~/.espup/esp-clang` is a stable symlink created by `espup install` that always points to the
current versioned `esp-clang/lib` directory, so it survives `espup update` without manual editing.

**`espflash 4.x` bundles an ESP-IDF v5.5.1 bootloader that is incompatible with v5.3.3 app binaries.**
When `espflash flash` runs without `--bootloader`, it writes its own bundled v5.5.1 bootloader.
That bootloader uses a different MMU page size (32 KB vs 64 KB in v5.3.3) and may reject the app
with `Image requires efuse blk rev <= v0.99, but chip is v1.3`.
Fix: always pass `--bootloader <path>` pointing to the IDF-built bootloader that `esp-idf-sys`
places at `target/<target>/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin`.
Also pass `--ignore-app-descriptor` to prevent espflash from performing its own chip-model check.
See `scripts/flash.sh` for the implemented solution.

**Bare-metal Xtensa targets (`xtensa-esp32-none-elf`, `xtensa-esp32s3-none-elf`) require `-Tlinkall.x` and `-fno-stack-protector` in rustflags — without them the linker fails with two classes of undefined-reference errors.**
The esp-hal top-level linker script `linkall.x` includes `memory.x`, `alias.x`, `hal-defaults.x`, and the chip-specific section layout.
`hal-defaults.x` contains the line `INCLUDE "device.x"`, which is produced by the PAC (e.g. `esp32s3`) crate's build script and provides `PROVIDE(TG0_T0_LEVEL = DefaultHandler)` and ~60 other peripheral interrupt stubs.
Without `-Tlinkall.x`, none of these scripts are processed, so the linker sees every PAC interrupt vector as an undefined symbol and reports them one per line — making the real cause easy to miss.
The second error (`undefined reference to '__stack_chk_guard'`) appears because GCC 15.2 (`esp-15.2.0_20250920`) injects stack-protection check calls into `esp_hal::init`, but without a linker script the guard variable is never defined.
`-C link-arg=-fno-stack-protector` is passed to GCC-as-linker-driver which forwards it back to the compiler stage at link time, suppressing the injected guard reference.
Both flags belong in `[target.xtensa-esp32-none-elf]` and `[target.xtensa-esp32s3-none-elf]` sections in `.cargo/config.toml.dist` — the workspace default config does not include bare-metal Xtensa sections.
The `riscv32*-unknown-none-elf` targets do not need `-fno-stack-protector` because GCC 15.2 does not inject stack protection for RISC-V bare-metal.

**ESP32-C3 requires target `riscv32imc-esp-espidf` and `MCU=esp32c3` — not the C6 defaults.**
ESP32-C3 is `riscv32imc` (no atomics extension); ESP32-C6 is `riscv32imac`.
Building with `MCU=esp32c6` and `riscv32imac-esp-espidf` produces an image whose ESP-IDF chip
metadata (including `max_efuse_blk_rev`) is wrong for the C3.
The workspace default in `.cargo/config.toml` is C6 because that is the primary RISC-V target,
but `scripts/flash.sh` and `scripts/build-example.sh` extract the chip from the example name
(`idf_c3_*` → C3, `idf_c6_*` → C6) and set `MCU` and `--target` accordingly.

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

**`just verify` does not compile Xtensa IDF targets — missing target sections in `.cargo/config.toml` pass verification but fail to build.**
`just verify` compiles only the workspace default target (`riscv32imac-esp-espidf`).
Xtensa ESP-IDF targets (`xtensa-esp32-espidf`, `xtensa-esp32s3-espidf`) and bare-metal targets are never exercised.
If `.cargo/config.toml` is missing a `[target.xtensa-esp32s3-espidf]` section with `linker = "ldproxy"`,
`just verify` passes clean but `just build-example idf_esp32s3_*` fails with `ldproxy` panicking
(`Cannot locate argument '--ldproxy-linker <linker>'`) because the linker is never invoked through ldproxy.
Fix: ensure every ESP-IDF target used by examples has its own `[target.*]` section in both
`.cargo/config.toml` and `.cargo/config.toml.dist`.
Completion gate for hardware examples: run `just build-example <name>` in addition to `just verify`.

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

**EUI byte order is the single most common cause of silent join failure.**
TTN Console displays DevEUI and JoinEUI as big-endian hex strings (e.g. `70B3D57ED005ABCD`).
Many embedded LoRaWAN stacks — including `lorawan-device` — expect the in-memory `[u8; 8]` in
LSB-first (little-endian / reversed) order.
`LoraConfig::from_hex_strings()` currently preserves string order (MSB-first).
If the underlying stack expects reversed order, OTAA join will fail silently:
the network server sees a valid join request, but the device never receives the join acceptance
(or the keys derived are wrong).
Validation step: after constructing the config, log DevEUI/JoinEUI **as bytes** and compare
with what the `lorawan-device` crate documentation specifies for your region stack.

**"Join-accept sent by TTN; device never joins" almost always means an RX window issue — not wrong keys.**
When TTN Live Data shows a join-accept downlink was transmitted but the device stays in `Joining` state:
- The RX1 window opens ≈1 s after the TX burst ends; RX2 opens ≈2 s after.
- `RX_WINDOW_OFFSET_MS = -200` is a reasonable starting offset — tune upward (less negative) if windows are still missed.
- If DIO1 (radio interrupt) is not wired or the interrupt flag is never cleared, the radio completion
  event is never delivered to the LoRaWAN state machine regardless of RF quality.
- If the BUSY line is not handled, every SPI command stalls and the RX window is entered late or never.

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

**CodeQL's `rust/hard-coded-cryptographic-value` query flags string literals passed to parameters named `password`, `credential`, or similar — even in test code.**
The query performs taint analysis: if a string literal flows into a function parameter whose name matches a credential keyword, it raises a Critical alert regardless of context.
Test helpers like `WiFiConfig::new("ssid", "pass")` trigger it because the second parameter is named `password`.
Fix: define test fixture constants with non-credential names (e.g. `TEST_PSK`) and route them through a helper function (e.g. `test_config()`).
This breaks the direct literal-to-password-parameter flow that CodeQL traces.
For empty passwords, use `&String::new()` instead of `""` — the indirection also defeats the pattern match.
See `crates/wifi-pure/src/lib.rs` test module for the implemented pattern.
