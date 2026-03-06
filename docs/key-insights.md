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

**ESP32-C3 requires target `riscv32imc-esp-espidf` and `MCU=esp32c3` — not the C6 defaults.**
ESP32-C3 is `riscv32imc` (no atomics extension); ESP32-C6 is `riscv32imac`.
Building with `MCU=esp32c6` and `riscv32imac-esp-espidf` produces an image whose ESP-IDF chip
metadata (including `max_efuse_blk_rev`) is wrong for the C3.
The workspace default in `.cargo/config.toml` is C6 because that is the primary RISC-V target,
but `scripts/flash.sh` and `scripts/build-example.sh` extract the chip from the example name
(`idf_c3_*` → C3, `idf_c6_*` → C6) and set `MCU` and `--target` accordingly.

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
