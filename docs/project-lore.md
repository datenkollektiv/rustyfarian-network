# Project Lore

Non-obvious technical discoveries: pitfall reminders that cost real debugging time.
Where an ADR, rustdoc or runbook already holds the full story, the entry is one line with a pointer.
Refer to `CLAUDE.md` and the `/project-lore` skill for recording guidelines.

---

## Toolchain & Build

**`rust-toolchain.toml` with `channel = "esp"` is required — `espup` alone is not enough.**
Without it `cargo` falls back to host `stable` and reports `can't find crate for 'core' / target may not be installed` for `riscv32imac-esp-espidf`.
With it, rustup picks the toolchain in any shell without `source ~/export-esp.sh`.

**Every CI job must install the `esp` toolchain, including `cargo fmt`.**
rustup reads `rust-toolchain.toml` for every `cargo` call, so `dtolnay/rust-toolchain@stable` fails with `custom toolchain 'esp' ... is not installed`.
Fix: `esp-rs/xtensa-toolchain@v1.7.0` (`ldproxy: false` suffices for a format check; the `esp` toolchain ships `rustfmt`).

**`just verify` calls `fmt-check`, which only detects drift — run `just fmt` first, or CI fails with no compiler error.**

**Every crate building ESP-IDF examples needs a `build.rs` calling `embuild::espidf::sysenv::output()`.**
`esp-idf-sys`'s `cargo:rustc-link-arg` does not propagate through transitive deps; `sysenv::output()` re-emits them from `DEP_ESP_IDF_SVC_EMBUILD_LINK_ARGS`.
Without it `ldproxy` panics `Cannot locate argument '--ldproxy-linker <linker>'` despite a correct PATH.
Also set `RUSTC_LINKER = "ldproxy"` in `.cargo/config.toml` `[env]` so embuild emits `--ldproxy-linker` as a real link arg.

**`LIBCLANG_PATH` is the only `~/export-esp.sh` variable riscv32 (C3/C6) builds need.**
The riscv32 cross-compiler is managed by `esp-idf-sys`'s CMake build; the Xtensa PATH entry matters only for Xtensa.
Fix: `export LIBCLANG_PATH="$HOME/.espup/esp-clang"` in `.envrc` (a stable symlink that survives `espup update`).

**`espflash 4.x` bundles an ESP-IDF v5.5.1 bootloader incompatible with v5.3.3 apps.**
It uses another MMU page size (32 KB vs 64 KB) and may reject the app with `Image requires efuse blk rev <= v0.99, but chip is v1.3`.
Fix: pass `--bootloader` with the IDF-built `target/<target>/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin`, plus `--ignore-app-descriptor`; see `scripts/flash.sh`.

**Bare-metal Xtensa targets need `-Tlinkall.x` and `-C link-arg=-fno-stack-protector`.**
Without `-Tlinkall.x` the PAC `device.x` (~60 interrupt stubs) is never processed and every vector is an undefined reference.
Without `-fno-stack-protector`, GCC 15.2 injects stack-guard calls into `esp_hal::init` that need an undefined `__stack_chk_guard`.
Fix: both flags in the `[target.xtensa-esp32*-none-elf]` sections of `.cargo/config.toml.dist`; RISC-V is unaffected.

**ESP32-C3 needs target `riscv32imc-esp-espidf` and `MCU=esp32c3`, not the workspace C6 defaults.**
A C6-target build carries wrong chip metadata (`max_efuse_blk_rev`) and the bootloader rejects it.
`scripts/flash.sh` and `scripts/build-example.sh` derive chip and target from the example prefix (`idf_c3_*`); new examples must follow it.

**`sdkconfig.defaults` must sit at the workspace root; embuild silently ignores it in a crate directory.**
Declare it in `build.rs` as `cargo:rerun-if-changed=../../sdkconfig.defaults`.
`CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` suffices for LoRaWAN OTAA crypto on ESP-IDF.

**`sx126x 0.3` needs exactly one `critical-section` provider, selected only by the final binary.**
`esp-idf-hal` (0.45+) provides one behind its `critical-section` feature as a recursive FreeRTOS mutex (`xQueueTakeMutexRecursive`, `src/task.rs`): it masks no interrupts and suspends no scheduler.
`rustyfarian-esp-idf-power`'s `critical-section-impl` masks interrupts; both together fail with `multiple definition of '_critical_section_1_0_acquire'`, none with `undefined reference`.
Libraries never select one (ADR 021; the `lora` feature did until bug 007); `EspIdfLoraRadio` pulses NRST itself instead of `sx126x::reset()` — see rustdoc of `pulse_nrst`.

**`just verify` compiles only `riscv32imac-esp-espidf`, never Xtensa IDF or bare-metal targets.**
A missing `[target.xtensa-esp32s3-espidf]` section with `linker = "ldproxy"` passes verify, then `just build-example idf_esp32s3_*` panics `Cannot locate argument '--ldproxy-linker <linker>'`.
Fix: one `[target.*]` section per ESP-IDF target in both `.cargo/config.toml` and `.cargo/config.toml.dist`; run `just build-example <name>` for hardware examples.

**Unpinned cross-repo git deps break resolution when upstream bumps a `links = "..."` crate.**
Only one version of a `links` crate (`esp-println`, `esp-idf-sys`) may exist, and the `failed to select a version for <crate>` error names the linked crate, not the unpinned dep.
Fix: pin with `tag = "vX.Y.Z"` or `rev = "<sha>"`.

**A crate-level `default-features = false` on a `workspace = true` dep is ignored unless the workspace declaration also says `default-features = false`.**
Cargo only warns (`'default-features' is ignored for <crate>`), so a `no_std` consumer silently inherits `std` (this broke the ADR 014 §2 `no_std` contract of a provisioning consumer, now in `juggler`).
Fix: workspace declaration `default-features = false`, `std` consumers add `features = ["std"]`, re-run `just verify`; treat the warning as `-D warnings`-class.

**`ESP_IDF_SDKCONFIG_DEFAULTS` needs ABSOLUTE paths; a relative list silently applies neither base nor overlay.**
Generated `.../esp-idf-sys-*/out/sdkconfig` then shows IDF defaults (`CONFIG_ESP_MAIN_TASK_STACK_SIZE=3584`, not `32768`); this gave a false STA-only pass with SoftAP still `y`.
Fix: `{{justfile_directory()}}/...` paths (see `check-sta-only`); verify by grepping `out/sdkconfig` (disabled = `# CONFIG_X is not set`, not `CONFIG_X=n`), never by compile success.

**`just clean-idf` removes `release/build/esp-idf-sys-*`, but `cargo check` builds under `debug/`, so a variant switch reuses a stale esp-idf-sys.**
Fix: `cargo clean -p esp-idf-sys --target-dir {{ idf_dir }}` (profile-agnostic) before and after.
A per-variant `--target-dir` would carry another 1-2 GB esp-idf tree and overflow the 8 GB RAM disk (`/Volumes/RustBuilds`).
Context: `docs/features/archive/wifi-softap-cfg-gate-v1.md`.

**A doctest in a `#[cfg(any(feature = "X", test))]` module never runs under `--no-default-features`: rustdoc does not set `cfg(test)`.**
Symptom: `Doc-tests <crate> … running 0 tests`.
Fix: a `#[cfg(test)]` unit test, e.g. `reexport_parity_guard` in `crates/rustyfarian-esp-hal-network/src/ota/mod.rs`; a bare `pub use` also misses an omitted re-export, only a consumer-style `use` fails.

**A shell export (direnv `.envrc`) silently overrides the same key in `.env`: `set dotenv-load` never overwrites.**
The `Config — ... mqtt_host=... mqtt_port=...` line from `examples/common/env.rs` shows what was baked in.
Fix: build-time keys only in `.env`, `direnv reload`, rebuild (`build.rs` has `rerun-if-env-changed` per key).

**`ubuntu-latest` = Ubuntu 26.04 ships `libxml2.so.16`; ESP-IDF v5.3.3's prebuilt `esp-clang` 16 needs `libxml2.so.2`, so every `esp-idf-sys` CI build dies after 2-3 min.**
The failing step is `idf_tools.py install`; `fmt`, `cargo audit` and `lint-docs` stay green, and the `idf5.3_py3.14_env` venv name is the tell.
Fix: `runs-on: ubuntu-24.04` (rust.yml, clippy.yml); `just act` maps it to `catthehacker/ubuntu:act-24.04` with `--pull=false` (GHCR pull is denied without login).
Only an ESP-IDF upgrade whose esp-clang links the new soname leads back to `ubuntu-latest` (`docs/features/esp-idf-v5-5-upgrade-v1.md`).

**The Xtensa LLVM backend crashes rustc (`Cannot scavenge register without an emergency spill slot!`, SIGSEGV) on a function with many arguments.**
Seen 2026-11-04 for `xtensa-esp32-espidf` / `xtensa-esp32s3-espidf` at opt-level 3 (eight-argument IDF MQTT `connect_work`); RISC-V built the same code, and a full clean did not help.
The error names no function; an LLVM machine-IR dump found it.
Fix: bundle the arguments into a struct; run `just build-xtensa-idf-mqtt` after widening a tier signature.

---

## esp-hal Bare-Metal Stack (esp-radio / esp-hal waves)

**`esp-radio` gates bare-metal waves: `esp-radio 0.18` needs `esp-hal ~1.1`; only the pre-release `1.0.0-beta.1` targets `~1.2.0`.**
The workspace took the pre-release deliberately to stay in lockstep with `rustyfarian-ws2812 0.7.0`; re-pin to `1.0.0` when it ships.
Before planning a wave, read `curl -s https://index.crates.io/es/p-/esp-radio` for a non-prerelease matching the target `esp-hal` minor.

**"`esp32s3` is LoRa-only by design" (merge `45f2d67`) was false.**
Every pinned crate (`esp-radio`, `esp-storage`, `esp-bootloader-esp-idf`, `esp-rtos`, `esp-alloc`, `esp-backtrace`) has an `esp32s3` feature the crate never forwarded, and the promised `compile_error!` never existed.
Before trusting a chip restriction, grep `~/.cargo/registry/src/*/<crate>-<ver>/Cargo.toml` for the chip feature.

**`minimq 0.13` defaults to `defmt`: declare `default-features = false`, or the link fails on missing `_defmt_*` symbols.**
The default drags in `heapless/defmt`, `embedded-io-async/defmt`, `embassy-time/defmt`; without it the log macros are no-ops; `cargo tree -e features -p minimq` shows the pull.

**Hand `minimq::Session::connect` a `&mut TcpSocket`, never the socket by value.**
`connect(io)` drops `io` on failure, leaking `StaticCell` buffers for the boot; `embedded-io-async` blanket impls for `&mut T` satisfy minimq's `Io` bound.
`socket.abort()` then `flush().await` returns it to CLOSED; that `flush()` waits for the RST, so skip it while the network is down and bound it while up — see `mqtt/runner.rs`.

**`riscv32imc` (C3) has no atomic read-modify-write: `fetch_add` is `E0599` on the C3 only, the C6 (`imac`) passes.**
Use load + store for advisory counters, `critical_section` for exact ones, or `portable-atomic`; always run the C3 line of `just clippy-hal`.

**Bare-metal MQTT costs ~150 B heap on the S3: 25 928 B free with Wi-Fi + MQTT vs 26 076 B with Wi-Fi alone, flat over 10 min with a broker restart (2026-10-14).**
All client memory is `StaticCell` `.bss` (~8 KiB: minimq 1 KiB RX + 2 KiB TX, TCP 2 × 1536 B, two 4-deep queues); procedure in `docs/runbooks/esp-hal-mqtt-hardware-test.md`.

**esp-hal MQTT stays `Connected` through a Wi-Fi outage (`flush()` waits for a peer ACK) — see ADR 019 amendment 2026-10-16.**

**ESP32-S3 bare-metal Wi-Fi fits the same 72 KiB internal heap as the C3; PSRAM cannot serve Wi-Fi buffers (`malloc_internal` only, ADR 019 references).**
`hal_esp32s3_connect_async` got a DHCP lease with ~26 KB free on the CrowPanel (2026-10-14).

**`esp-storage 0.10` made the `embedded-storage` impls opt-in: `FlashStorage` stops implementing `NorFlash`.**
Fix: `features = ["embedded-storage"]` for the provisioning store; inherent `FlashRegion::{capacity, write}` in the OTA manager.

**`esp-radio 1.0.0-beta.1`: controller-only return, STA/AP singletons (`Interface::station()` / `access_point()`), typed `Ssid` / `Password` / `AuthenticationMethodConfig`.**
`try_from` rejects oversized input (0.18 truncated); an empty password stays `Wpa2Personal("")`; a second `station()` panics, `try_station()` probes.

**Two semver-incompatible `pennant` versions make `StatusLed` two traits** (`E0599 no method named set_color`); keep matching versions, diagnose with `cargo tree -i pennant`.

**`esp-radio 0.18` removed the `smoltcp` feature and `WifiDevice`'s `smoltcp::phy::Device` impl; the Wi-Fi controller is async-only via `embassy-net-driver`** (CHANGELOG #4870).

**`embassy-net`'s `udp`, `tcp`, `proto-ipv4`, `dhcpv4`, `medium-ethernet` features are independent and off by default.**
Missing ones show as `E0432: unresolved import 'embassy_net::udp'` or a `smoltcp` `compile_error!` deep in the graph.
Fix: forward them on the consuming crate's `embassy-net` dep (`embassy` feature in `crates/rustyfarian-esp-hal-network/Cargo.toml`).

**`embassy-net::TcpSocket` server loop: `accept(port)` is listen and wait in one; `close()` only sends FIN; `flush()` must follow before reuse.**
Without `flush()` the old connection lingers in FIN-WAIT and races the next accept; `abort()` (RST) also needs `flush()`.
Pattern: `loop { accept; serve; close; flush.await }` — see `provisioning/portal.rs::run`.

**Bind workspace invariants at construction, not per call: host tests miss the call-site indirection.**
`LeaseTable::allocate` once took `pool_start` per call and one of three call sites passed `server_ip`, so the phone got `192.168.4.1` and stalled on "IP configuration failure" — only on hardware.
Fix: `LeaseTable::new(pool_start, lease_secs)`; likewise `build_response` hard-codes `Connection: close`.

**`StaticCell<[u8; N]>` needs a compile-time `N`, so a runtime `buf_size` field cannot size it.**
Spike code uses `const` buffer sizes and keeps the config fields as markers; production code should lift sizes to const generics.

**`StationConfig::with_ssid(ssid.into())` is `E0283`; pass the `&str` directly** (the password setter still needs `.into()` for `Password`).

**`esp-hal 1.1.0` RMT TX: `configure_tx(pin, config)` became `configure_tx(&config).unwrap().with_pin(pin)`** (see the `rustyfarian-ws2812` April 2026 CHANGELOG).

**C3/C6 Super Mini PCB antennas corrupt WPA2 auth frames at full TX power — see rustdoc of `juggler::wifi::STATION_TX_POWER_DEFAULT`.**

**`embassy-executor 0.10` removed `Spawner::must_spawn`: write `spawner.spawn(my_task(arg).unwrap())`.**
The `.unwrap()` belongs on the task call (`Result<SpawnToken, SpawnError>`); the compiler's suggested `.expect` placement after `spawn(...)` is wrong.

**`esp-rtos` panics `unwrap of self.time_driver.as_mut() failed: NoneError` on the first `.await` of any kind if `esp_rtos::start` never ran.**
`WiFiManager::init_async` / `init_softap_async` call it, so a path without Wi-Fi (e.g. an already-provisioned branch, `hal_c3_provision_mqtt` 2026-06-18) has no scheduler.
Fix: spin non-async on that path, or call `esp_rtos::start` with `TIMG0` + `FROM_CPU_INTR0`; since 2026-10-31 the `*_scheduler_running` variants and `ProvisioningBuilder::start_scheduler_running` let the firmware own it (ADR 021), and both `init_*` start the scheduler before validating.

**A missing `embassy-executor` dep surfaces as `no method named 'unwrap' found for ... impl Future` at every `spawn` site.**
The `#[embassy_executor::task]` attribute cannot expand; the first error, `E0432: unresolved import embassy_executor`, is the cause.
Fix: `embassy-executor` + `embassy-time` as workspace-pinned `[dev-dependencies]` when only examples use them.

**esp-hal joined the first AP of the SSID by channel, not the strongest (a -89 dBm mesh node next to a -20 dBm AP) — see rustdoc of `juggler::wifi::ApSelection`.**
esp-radio's `WIFI_CONNECT_AP_BY_SIGNAL` only applies to all-channel scans; the `[wifi] link up` line shows bssid and rssi.

**esp-radio 1.0.0-beta.1 hard-codes country CN, channels 1-13 — see ADR 026.**

---

## Flashing & Serial (espflash 4.x)

**`espflash` auto-detect picks Bluetooth `/dev/cu.*` ports on Macs and fails with "Error while connecting to device".**
`scripts/detect-port.sh` keeps only USB serial devices (`usbmodem*`, `usbserial*`, `SLAB_USBtoUART*`, `wchusbserial*`; `ttyUSB*`, `ttyACM*` on Linux) and auto-picks only a single match; `ESPFLASH_PORT` wins.

**A lingering `espflash monitor` holds the port: `Failed to open serial port ... Error while connecting to device`.**
`lsof /dev/cu.usbmodem1101` shows the PID; usually a closed terminal tab without SIGINT.

**A helper that prefixes `$PWD/` onto a caller-supplied dir breaks when the justfile passes an absolute RAM-disk path.**
`find_idf_bootloader` in `scripts/lib.sh` globbed `/Users/…/rustyfarian-network//Volumes/…`, so every bare-metal flash rebuilt an IDF example and fell back to the bundled bootloader.
Fix: normalise before globbing and re-check after the populating build (fatal on `xtensa-*`, warning on RISC-V).

**`cargo clean -p esp-idf-sys` does not rebuild the bootloader: `out/` survives with the old `bootloader.bin`.**
A variant-switching harvest must delete `release/build/esp-idf-sys-*` (`just clean-idf`) and grep `out/sdkconfig` for the rollback line.

**Bare-metal OTA rollback depends on the bootloader, built from the sdkconfig overlay, not the example's features.**
Only `sdkconfig.ota.defaults` sets `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`; cache files are `<chip>.bin` and `<chip>-ota.bin`.
`flash.sh` picks `-ota` when the features include `ota`; `ensure-bootloader.sh --ota` builds it (c3 via `idf_c3_ota_runtime`, esp32s3 via `idf_esp32s3_mqtt_lwt` + `ota`).

**The IDF-built v5.3.3 bootloader boots bare-metal esp-hal images on RISC-V (C3) and Xtensa (Heltec V3 S3), verified 2026-09-26** with `--bootloader <path> --ignore-app-descriptor`.

**The bootloader cache `<idf_dir>/bootloader/<chip>[-ota].bin` lives on the RAM disk and dies with it; the first flash afterwards rebuilds a full IDF example (~1m26s).**
A copy outside the RAM disk keyed by chip and IDF version is unimplemented; keep `just ramdisk` attached.

**An ESP-IDF program's last console line can stay in the USB Serial/JTAG TX FIFO: a partial 64-byte packet goes out only on a flush.**
Looks like a hang with no panic or reboot; the line appears at the start of the next boot.
Fix: `esp_idf_svc::sys::fsync(1)` after the last line, then a heartbeat.

---

## ESP-IDF Event Loop (`esp-idf-svc`)

**`EspEventLoop::subscribe` takes the event by value: `|event: WifiEvent<'_>|`, not `&WifiEvent`** (else `E0631`; the compiler's remove-`&` hint is right).

**Dropping an `EspSubscription` unregisters the handler; store it in the owning struct.**

**`IP_EVENT_STA_LOST_IP` (esp-idf-svc 0.53 `IpEvent::DhcpIpDeassigned`) fires only after `CONFIG_ESP_NETIF_IP_LOST_TIMER_INTERVAL`, 120 s by default.**
`esp_netif_down` arms `esp_netif_ip_lost_timer`; `IP_EVENT_STA_DHCP_DEASSIGNED` comes only from the DHCP client's deinit.
Fix: gate on `WiFiManager::subscribe_ready` / `is_ready` (association + IP, milliseconds), which is why the MQTT gate uses `LinkSupervisor::subscribe_ready`.

---

## ESP-IDF FreeRTOS Tasks & Threads

**`xTaskGetHandle` asserts (`strlen(pcNameToQuery) < 16`, `tasks.c:2855`) on a name over 15 characters.**
Names are stored cut to 15: esp-idf-svc's `MQTT Sending task` is `MQTT Sending ta`, and querying the full name rebooted the C3 every minute; query the cut name (`MAX_TASK_NAME_LEN`).

**`std::thread::Builder::name` never reaches FreeRTOS: every thread's task is `pthread`.**
Fix: `esp_pthread_set_cfg` with `thread_name` before the spawn — see rustdoc of `spawn_named` (`mqtt/task_name.rs`).
`esp_pthread_get_cfg` zeroes its out-parameter on `ESP_ERR_NOT_FOUND`, so take `esp_pthread_get_default_config()` for the fallback, or `set_cfg` rejects the zero `stack_size` with `ESP_ERR_INVALID_ARG`.

---

## MQTT Event Loop (`esp-mqtt` / `esp-idf-svc`)

**Any `esp_mqtt_client_*` call from a callback on the event-loop thread deadlocks on esp-mqtt's recursive `api_lock` — see ADR 017 and rustdoc of the IDF `mqtt` module.**
`subscribe` does not wait for a SUBACK; the block is the mqtt task parked while holding `api_lock` until our `next()`.

**"Release the event before `on_message`" is impossible: esp-idf-svc 0.53 calls `receiver.done()` only inside `next()` — see ADR 017.**

**`MqttClientConfiguration::reconnect_timeout: None` disables esp-mqtt auto-reconnect; the runner now sets it on purpose and paces reconnects itself — see rustdoc of `mqtt/runner.rs`; reproduce with `just mosquitto down`/`up`.**

**A free-heap trend falling in doubling steps at doubling intervals is a growable collection gaining one entry per event, not a slow leak.**
`PendingAcks::early` kept every fire-and-forget QoS 1 outcome (116, 288, 576, 1152 B at ~1.5, 2.5, 5, 9.5 min); the flat esp-hal twin placed it on the std side — see rustdoc of `EARLY_ACK_CAPACITY`.

**The `mqtt-net` runner cannot deadlock on `api_lock` (ADR 023, dated 2026-11-11), and `esp_mqtt_client_reconnect` / `_disconnect` take no lock at all.**
`api_lock` is taken by `start`, `subscribe`, `enqueue` and `stop`; reconnect and disconnect only set event-group bits.
The event-loop thread never waits on the runner (only unbounded `Session::send`, inbound pushes, `PendingAcks`), so a blocked step ends within one `mqtt_task` iteration.
Before the MQTT thread memory change an empty connect plan skipped the runtime catch-up and later `MqttHandle::subscribe` calls were stored, not sent — see `an_empty_plan_still_finishes_before_reporting_done`.

---

## LoRaWAN / TTN v3 (EU868)

**EUI byte order is the most common cause of silent OTAA join failure.**
TTN shows DevEUI/JoinEUI MSB-first; `lorawan-device` expects LSB-first `[u8; 8]`, and `LoraConfig::from_hex_strings()` keeps string order, so `.reverse()` both EUIs.
Symptom: the server logs a valid join request, the device never sees the accept (or derives wrong keys).

**"Join-accept sent by TTN, device never joins" almost always means an RX window issue, not wrong keys.**
RX1 opens ≈1 s, RX2 ≈2 s after TX end; start with `RX_WINDOW_OFFSET_MS = -200` and tune less negative if windows are missed.
Common upstream causes: DIO1 interrupt not delivered, BUSY pin not handled (below).

**DIO1 interrupt not delivered → radio events never reach the state machine.**
Without a working DIO1 path `nb_device::Event::RadioEvent` never fires and downlinks are never processed; on ESP-IDF poll `get_irq_status()` instead — see rustdoc of `EspIdfLoraRadio::irq_pending`.

**BUSY pin not handled → SX1262 SPI commands issued while BUSY is high are ignored or corrupt; wait for BUSY low (bounded) before every command.**

**Frame counter reuse after deep sleep makes TTN drop uplinks.**
TTN silently drops `FCntUp` ≤ the last accepted one (unless "Reset frame counters", which weakens security).
Restore `LorawanSessionData::fcnt_up` exactly and increment it; if restore fails (CRC, RTC memory), re-join instead of sending a stale counter.

**Downlinks arrive only in the RX windows after an uplink** — see `docs/runbooks/ttn-otaa-validation.md` Step 6.

---

## sx126x 0.3 API

**`LoraCodingRate`, not `LoRaCodingRate`** (but `LoRaBandWidth`, `LoRaSpreadFactor`).

**`set_low_dr_opt`, not `set_low_data_rate_opt`**, on `LoraModParams`.

**`ANT` needs `OutputPin<Error = GpioError>`: `SX126x::new` uses one `TPINERR` for all pins, inferred as `esp_idf_hal::gpio::GpioError`.**

**`check_trans_valid(1052): rx length > tx length in full duplex mode` on ESP32-S3 — see rustdoc of `FullDuplexDevice`.**
Half-duplex is no fix (`SOC_SPI_HD_BOTH_INOUT_SUPPORTED` undefined on S3 breaks `TransferInPlace`); `hal_esp32s3_join` never hit it because it issues equal-length `Transfer`s.

---

## CodeQL / GitHub Advanced Security

**`rust/hard-coded-cryptographic-value` flags string literals reaching `password`/`credential`-named parameters, also in test code.**

**Renaming the constant or routing it through a helper does NOT clear it (PR #91, 2026-08-18).**
The query is interprocedural taint tracking: the source is the literal, flow follows `const` bindings and calls.
Breaking dataflow (e.g. `[u8; N]` via `core::str::from_utf8`) is obfuscation that regresses when the query pack learns the flow; do not use it.

**Correct handling: generate the fixture at runtime from real entropy.**
`OnceLock<String>` from `RandomState`: `test_psk()` in `crates/juggler/src/test_support.rs`, `test_nonce()` in the provisioning portals.
A test comparing against a request body builds it at runtime too (`alloc::format!`) and derives the mismatching value from the fixture, so it cannot flake; it checks match versus mismatch, not a value.
Dismissal ("Used in tests") stays valid only for externally fixed values; path exclusion would also drop production code in inline `#[cfg(test)]` files.

**Examples are scanned too: an empty `option_env!` fallback (`env_or!("WIFI_PASS", "")`) trips the query.**
The PR check reports only new findings, so older identical shapes stay invisible there.
Fix: `fn password() -> &'static str { option_env!("WIFI_PASS").unwrap_or_default() }`; LoRa examples use `unwrap_or_default()` for EUIs and AppKey so `from_hex_strings` fails fast (the old all-zero AppKey literal was a real hard-coded key).
Rule: never write a literal for a password, PSK, key, nonce or token, and never give one a non-empty default.
Alerts on `/security/code-scanning` reflect the default branch (close only after merge to `main`) and need repo write access plus `security-events` to read; read the real locations before editing.

**A `const` holding an `option_env!` credential is flagged; the same value returned from a `fn` is not** (`const AP_PSK` vs `fn ap_psk()`, PR #112); the bot's `= None` suggestion would have removed the WPA2 portal path.

**`rust/cleartext-logging` taints every field read through a struct that carries a credential (e.g. `config.ap.channel`).**
Log only values that never passed through such a struct, or a fixed message.
A library log line can turn tainted the day an example first calls `with_auth` / `with_username_only` (`MqttBuilder::build`, 2026-11-01); grep library `log::` calls for reads from credential-bearing config types.

---

## OTA MVP — Three-Crate Triad

**`#![no_std]` test modules cannot use `format!` / `.to_string()` even on the host**; use `core::fmt::Write` into `heapless::String`, or `use alloc::string::ToString`.

**`esp-idf-svc 0.52.x` needs `embedded-svc 0.29`; pinning `0.28` gives a silent trait-version mismatch on `Headers::content_len`.**

**`esp-storage 0.9.0` `FlashStorage::new` takes `FLASH<'d>`; a second call per boot panics** — hence `EspHalOtaManager::new(config, FLASH<'d>)`.

**Hand-rolled HTTP/1.1 parsers must reject `Content-Length: +N` and whitespace before the colon** (request smuggling) — see rustdoc of `parse_header` and `HeaderState::feed` in `ota/http.rs`.

**A same-second `[ota] update failed … Update server unreachable` with nothing in the server log means no TCP connect happened; a blocked route times out after `timeout_secs`.**
The manager wrote the `GET` onto an unconnected socket; it also accepted only `HTTP/1.1 ` while `just ota-serve` answers `HTTP/1.0` (`BadStatusLine`); a hal OTA run needs a router rule for the image port.

**Logging a URL with RFC 3986 userinfo leaks credentials** — see rustdoc of `url_for_log`.

---

## OTA Consumer Runtime

**NVS `set` of a key stored with another type replaces it in one commit — see rustdoc of `OtaKv`** (ESP-IDF v5.3.3 `Storage::writeItem` looks the key up with `ItemType::ANY`).

**serde ignores `deny_unknown_fields` on unit variants of an internally tagged enum — see rustdoc in `ota/wire/status.rs`.**

**`EspOta::new()` returns `ESP_ERR_INVALID_STATE` (0x103) while another `EspOta` lives — see rustdoc of `ota/boot_facts.rs`; re-check on every `esp-idf-svc` bump.**

**After `att_ctr` loss a reissued attempt id swallowed a delivered report's `rb_id` — see rustdoc of `next_attempt_id`.**
Only a pending report (`rb == 1`) is adopted; regression tests in `crates/juggler/src/ota/persist/tests/counter_loss.rs`.

**Any OTA status can arrive twice (QoS 1 resend after 1 s, triggered by the ~6.5 s partition erase) — see `docs/runbooks/ota-hardware-test.md`.**

---

## ESP-NOW

The channel-stability analysis lives in [ADR 012](adr/012-espnow-unassociated-sta-channel-stability.md); update the ADR and the `EspIdfEspNow` rustdoc, not this section.

**An ESP32-C3 Super Mini at 20 dBm breaks ESP-NOW both ways — see rustdoc of `DEFAULT_TX_POWER_QUARTER_DBM`.**
A MAC ACK is the receiver's transmission, so a C3 TX-power fault looks like the peer's sends failing; bisect with the scout's `ESPNOW_TX_POWER`, one variable per run.

**Unassociated STA drifts off `esp_wifi_set_channel` within ms (coordinator gets probes, zero data; `FAIL` ~30 ms after TX) — use `init_with_radio`, see ADR 012.**

**Promiscuous-bracket re-pinning races (`ESP_ERR_ESPNOW_CHAN` 0x3069, ACKs missed off-channel) — prefer SoftAP, see ADR 012.**

**A peer mid-AP-roam hardware-ACKs unicast probes on every channel it visits, so first-ACK-wins latches the wrong channel — keep `ScanConfig::probe_confirmations` ≥ 1 with `confirmation_gap` longer than a scan dwell (defaults 1 / 150 ms), see `crates/juggler/src/espnow/mod.rs`.**

**Adjacent 2.4 GHz channels overlap, so a MAC ACK does not prove the channel — see ADR 025.**

**A failed `scan_for_peer` once left the radio on the last-probed channel with the peer removed; the `Err` branch restores both from `pinned_channel`** (`crates/rustyfarian-esp-idf-network/src/espnow/mod.rs`).

**esp-hal ESP-NOW: never hold a critical section across a send (`SendWaiter` spins on an interrupt-set flag) — see ADR 020.**

**Host tests need the test-only `host_critical_section`, not `critical-section/std` (`restore-state-bool` clashes with esp-idf-hal's `restore-state-none` in workspace clippy).**
It exists only in the lib unit-test binary, so integration tests and doctests touching the ESP-NOW cells do not link.

**C3 Super Mini logs over USB need `esp-println/auto`, not `uart`: there is no UART bridge, and `auto` checks the USB SOF flag at runtime — firmware selects the console, see ADR 021.**

**A scan interrupted by `shutdown()` ends `Err(Shutdown)` or `Err(Scan(Aborted))`; a harness must accept both.**
The worker publishes `Aborted`, but a caller polling its `ScanTicket` may see the shutdown flag first.

---

## SoftAP Provisioning

**ESP-IDF keeps the Wi-Fi country in flash; set it with `Country` / `set_country`, not raw `esp_wifi_set_country_code` — see ADR 026.**

**A SoftAP channel outside the country silently becomes channel 1 on ESP-IDF — see ADR 026.**

**The ESP-IDF AP netif can keep a stale subnet (`192.168.71.1` instead of `192.168.4.1`) across firmware — see rustdoc of `pin_ap_netif_ip`.**
`CONFIG_ESP_WIFI_NVS_ENABLED=n` does not prevent it; `espflash erase-flash` clears it only until recurrence.

**ESP-IDF httpd's 512 B header buffer returns `431 Request Header Fields Too Large` to browsers — see the `idf_c3_provision_mqtt` example docs (`CONFIG_HTTPD_MAX_REQ_HDR_LEN=2048`).**

**`EspHttpServer` sets no `Cache-Control`; captive-portal sheets cache the form across reflashes.**
Fix: `request.into_response(200, Some("OK"), &headers)` with `Cache-Control: no-store`, not `into_ok_response()`.

**`juggler::wifi::validate_password` only checks the maximum; a 1–7 byte password was accepted although WPA2-Personal needs ≥ 8.**
`validate_wifi_password` returns `TooShort { min: 8 }` for non-empty short values, and the form sets `minlength="8"` (empty still means open network).

**`ProvisioningStore::open`'s `total_bytes` is the region size at `base_offset` — see its rustdoc.**
Host tests at offsets 0/4096 hid the old check until `0x300000` panicked `OffsetOutOfBounds { end: 3153920, limit: 8192 }` on hardware; guard: `store_open_accepts_region_at_high_offset_within_capacity`.

**esp-storage 0.10 `read` rejects lengths not a multiple of `READ_SIZE` (4 B), so a committed record never read back on hardware.**
Blank flash passed (aligned 1280 B read) and host mocks used `READ_SIZE = 1`; `MockFlashWordRead` now enforces the contract.

**One board broadcasts different `{prefix}-{MAC}` SSIDs under esp-hal and ESP-IDF.**
ESP-IDF adds one to the last base-MAC byte (`…:c4:f5`); `esp_hal::efuse::interface_mac_address(AccessPoint)` sets the locally-administered bit in the first byte (`a2:…:c4:f4`), as does esp-radio's AP.
Each matches its own BSSID; do not make the hal side mimic ESP-IDF.

**An IDF device reopening the portal every boot can be a `POST /save` stack overflow, not a lost record — see rustdoc of `HTTPD_STACK_SIZE`.**
Measure frames from ELF prologues (`llvm-objdump -d`, `addi sp, sp, -N`); the panic's stack dump printed the form, Wi-Fi password included, so a pasted httpd panic is a credential leak.

---

## Mermaid Diagrams in Markdown

**Mermaid `timeline` event text reserves `:`, `;`, `"`, and HTML entities; they fail as `Parse error ... got 'INVALID'`.**
- `;` → `,` or `—`.
- `::` (Rust paths) → `.` or rephrase.
- bare `:` anywhere (`store.rs:141`, `mqtt://host:1883`) → `store.rs line 141`, or move it out of the event.
- `: ` in trait bounds (`<D: WifiDriver>`) → prose.
- `"..."` → drop the quotes.
- `&lt;` / `&gt;` → rephrase (the `;` re-triggers the error).
Em-dashes, `×`, `==`, `..`, `/` and parentheses are fine.
`just lint-docs` parses every fenced mermaid block via `mermaid-cli`; it is outside `just verify` because it pulls Node.

**`mmdc` accepts bare `:` strings the IDE's newer Mermaid rejects; a clean `just lint-docs` is necessary, not sufficient — check the rendered diagram.**
The `technical-writer` agent encodes this and is mandated for non-trivial `docs/ROADMAP.md` edits.

---

## SX1262 TCXO Init — `sx126x 0.3` Missing `ClearDeviceErrors` Bug

**`sx126x 0.3`'s `init()` hangs on BUSY with a TCXO: it calibrates without `ClearDeviceErrors` after `SetDIO3AsTCXOCtrl` — see rustdoc of `juggler::lora::sx1262` (sequence and bounded BUSY wait).**
Symptom on the Heltec V3: `DevEUI loaded`, then a spin in `sx126x::wait_on_busy` (0.3.0 source line 548) and a task-watchdog fire every 5 s.
The TCXO delay is 10 ms (was 5 ms; rated ≤ 2 ms) as cold-start margin; it is chip-internal and does not lengthen the BUSY wait.

---

## LoRaWAN OTAA Join (TTN EU868) — RX path & driver loop

First hardware join: Heltec WiFi LoRa 32 V3, `sx126x 0.3`, `lorawan-device 0.12.2`, ESP-IDF v5.3.3, TTN EU868; RX delivery and timing dominated.

**`esp-idf-hal` `PinDriver` interrupts are one-shot, so DIO1 delivered only TxDone — poll the IRQ register (esp-idf-hal issue #349).**
Re-arming needs `enable_interrupt()` from a non-ISR context, and the pin is moved into `SX126x`, so the loop cannot re-arm it; the polling is in `examples/idf_esp32s3_join.rs` and `idf_esp32s3_lora_uplink.rs`.

**`Response::TimeoutRequest(ms)` is an ABSOLUTE timestamp; `DownlinkReceived` ends a `send()` cycle — see rustdoc of `juggler::lora::ticker`.**

**An SF12 join-accept (~1.8 s) cannot finish in RX1 (capped at 1000 ms) — join at DR5, see rustdoc of `idf_esp32s3_join` and `juggler::lora` `RX_WINDOW_DURATION_MS`.**
TTN alternates RX1 and RX2 by gateway duty cycle (868.x is the 1 % band, 869.525 the 10 % band), so both windows must work.

**Downlinks carry no PHY CRC: RX must use `CrcOff` — see rustdoc in `juggler::lora`.**

**Compile-time `option_env!` credentials need `.env` wiring, or the device sends `dev_eui=0000000000000000`.**
Three gaps: the justfile needs `set dotenv-load`; keys must be `LORAWAN_DEV_EUI` / `LORAWAN_APP_EUI` / `LORAWAN_APP_KEY` (not `LORA_*`); `build.rs` needs `cargo:rerun-if-env-changed=LORAWAN_*`.
EUIs are entered MSB-first and reversed in the example; AppKey is unchanged; an all-zero JoinEUI is valid; `build.rs` warns on empty or all-zero credentials; `.env` stays gitignored.

**TTN Live Data shows only raw `frm_payload` until an Uplink payload formatter is set — see the `idf_esp32s3_lora_uplink` example docs.**

---

## crates.io Publishing

**Publish ESP crates against their real target, never `--no-verify`.**
Host-target verify fails for the IDF crate; the `just release-publish*` recipes use `cargo +esp publish --target riscv32imac-esp-espidf --target-dir {{ idf_dir }}` (IDF), `cargo publish --target riscv32imac-unknown-none-elf -Zbuild-std=core,alloc` (HAL), `--target {{ host_target }}` (pure).

**The HAL publish needs `-Zbuild-std=core,alloc` because the workspace `build-std = ["std", "panic_abort"]` cannot build `std` for bare metal** (`rustyfarian-ws2812` defaults to bare metal and needs no override).

**The `-network` crates cannot be dry-run until `juggler` is on crates.io (`no matching package named 'juggler' found`).**
Stage 1 `just release-publish juggler`, wait for indexing, Stage 2 `just release-dry-run-idf` / `-hal`, Stage 3 `just release-publish-idf` / `-hal`; before that `just release-dry-run` uses `cargo package --list`.

**Publishing reads `CARGO_REGISTRY_TOKEN` from the environment; no `cargo login`.**
A new crate name needs the "publish new crates" scope and no allowlist to other names; scopes show only in the web UI (`/settings/tokens`), `/me` confirms validity only.
