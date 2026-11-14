# Rustyfarian Network — development tasks
#
# ESP-IDF crates require the ESP-IDF toolchain (`just setup`).
# The consolidated pure crate (`juggler`) compiles and tests on any
# host without the ESP toolchain.
# Run `just setup` to initialize the environment.

# load .env file (LoRaWAN and WiFi credentials, MQTT config)
set dotenv-load

# list available recipes (default)
_default:
    @just --list

# host target, used to override the workspace ESP-IDF target for pure-logic tests
host_target := `scripts/host-target.sh`

# bare-metal target for HAL crates (publish / dry-run)
hal_target := "riscv32imac-unknown-none-elf"
hal_c6_target := "riscv32imac-unknown-none-elf"
hal_c3_target := "riscv32imc-unknown-none-elf"
hal_s3_target := "xtensa-esp32s3-none-elf"

# ESP-IDF target (publish / dry-run)
idf_target := "riscv32imac-esp-espidf"

# serial port for espflash; honoured verbatim if set, otherwise scripts/detect-port.sh
# narrows espflash auto-detect to USB serial devices (usbmodem/usbserial on macOS,
# ttyUSB/ttyACM on Linux) so paired Bluetooth ports do not get picked; with several
# boards attached, `just flash`/`run` pick the one whose chip matches the example name
# and the monitor reuses the port flashed last (remembered in target/espflash-ports).
export ESPFLASH_PORT := env("ESPFLASH_PORT", "")

ramdisk := "/Volumes/RustBuilds"
hal_dir := if shell('scripts/ramdisk-mounted.sh "$1"', ramdisk) == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir := if shell('scripts/ramdisk-mounted.sh "$1"', ramdisk) == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }

# Guided bench runner: a local rustyfarian-bench checkout if RUSTYFARIAN_BENCH points at one,
# otherwise the pinned release from GitHub (the scenarios need >= v0.5.0 for `probe`, `run`, `setup`/`teardown` and prompt placeholders).
bench_dir := env("RUSTYFARIAN_BENCH", "")
bench := if bench_dir != "" { 'uv run --project "' + bench_dir + '" rustyfarian-bench' } else { "uvx --from git+https://github.com/datenkollektiv/rustyfarian-bench@v0.5.0 rustyfarian-bench" }

# act: a local rustyfarian-ci checkout if RUSTYFARIAN_CI points at one, otherwise the pushed @v1.
ci_dir := env("RUSTYFARIAN_CI", "")
ci_repo_flag := if ci_dir != "" { '--local-repository "datenkollektiv/rustyfarian-ci@v1=' + absolute_path(ci_dir) + '"' } else { "" }

# ── env ───────────────────────────────────────────────────────────────────

# show RAM disk status, resolved target dirs, sccache, required/optional tooling, and MQTT broker reachability (.env)
[group('env')]
doctor:
    @scripts/doctor.sh "{{ ramdisk }}" "{{ hal_dir }}" "{{ idf_dir }}"

# manage the RAM disk: just ramdisk attach | detach
[group('env')]
ramdisk action:
    @scripts/ramdisk.sh "{{ action }}"

# ── build ──────────────────────────────────────────────────────────────────

# build the entire workspace (release)
[group('build')]
build:
    cargo build --release

# check the entire workspace
[group('build')]
check:
    cargo check

# check the wifi domain of the consolidated ESP-IDF network crate
[group('build')]
check-wifi:
    cargo check -p rustyfarian-esp-idf-network --features wifi --target-dir {{ idf_dir }}

# check the mqtt domain of the consolidated ESP-IDF network crate
[group('build')]
check-mqtt:
    cargo check -p rustyfarian-esp-idf-network --features wifi,mqtt --target-dir {{ idf_dir }}

# check the esp-idf lora domain of the consolidated ESP-IDF network crate
[group('build')]
check-lora:
    cargo check -p rustyfarian-esp-idf-network --features lora --target-dir {{ idf_dir }}

# check the consolidated pure crate with all features (no ESP-IDF required)
[group('build')]
check-pure:
    cargo check -p juggler --all-features

# check the pure lora feature (no ESP-IDF required)
[group('build')]
check-lora-pure:
    cargo check -p juggler --features lora

# check the pure wifi feature (no ESP-IDF required)
[group('build')]
check-wifi-pure:
    cargo check -p juggler --features wifi

# check the pure espnow feature (no ESP-IDF required)
[group('build')]
check-espnow-pure:
    cargo check -p juggler --features espnow

# check the pure ota feature (no ESP-IDF required)
[group('build')]
check-ota-pure:
    cargo check -p juggler --no-default-features --features ota

# check the OTA wire contract is no_std + alloc clean (no ESP-IDF required)
[group('build')]
check-ota-wire-pure:
    cargo check -p juggler --no-default-features --features ota-wire --target {{ hal_c3_target }} -Zbuild-std=core,alloc

# check the pure provisioning feature (no ESP-IDF required)
[group('build')]
check-provisioning-pure:
    cargo check -p juggler --features provisioning

# check the esp-idf ota domain of the consolidated ESP-IDF network crate
[group('build')]
check-ota-idf:
    cargo check -p rustyfarian-esp-idf-network --features ota --target-dir {{ idf_dir }}

# type-check (clippy) the esp-idf ota unit tests; they cannot run on the host (esp-idf-sys)
[group('build')]
clippy-ota-tests:
    cargo clippy -p rustyfarian-esp-idf-network --features ota --tests --target-dir {{ idf_dir }} -- -D warnings

# lint the pure ota-wire code and its host tests (the workspace clippy never enables `ota-wire`)
[group('build')]
clippy-ota-pure:
    cargo clippy -p juggler --features ota-wire --all-targets --target {{ host_target }} -- -D warnings

# lint all pure juggler code incl. the ESP-NOW scan machine, with every host-testable feature
[group('build')]
clippy-pure:
    cargo clippy -p juggler --features mock,std,provisioning,flash-store,ota-wire,espnow,embassy --all-targets --target {{ host_target }} -- -D warnings

# check the esp-idf ota runtime (features wifi,mqtt,ota) and its hardware example
[group('build')]
check-ota-mqtt-idf:
    cargo check -p rustyfarian-esp-idf-network --features wifi,mqtt,ota --example idf_c3_ota_runtime --target-dir {{ idf_dir }}

# lint the esp-idf ota runtime and its hardware example (they cannot run on the host)
[group('build')]
clippy-ota-mqtt-idf:
    cargo clippy -p rustyfarian-esp-idf-network --features wifi,mqtt,ota --lib --tests --example idf_c3_ota_runtime --target-dir {{ idf_dir }} -- -D warnings

# check the esp-idf provisioning domain of the consolidated ESP-IDF network crate
[group('build')]
check-provisioning:
    cargo check -p rustyfarian-esp-idf-network --features provisioning --target-dir {{ idf_dir }}

# type-check (clippy) the esp-idf provisioning unit tests (they cannot run on the host)
[group('build')]
clippy-provisioning-tests:
    cargo clippy -p rustyfarian-esp-idf-network --features provisioning --tests --target-dir {{ idf_dir }} -- -D warnings

# type-check (clippy) the esp-idf provisioning + mqtt unit tests, incl. the WifiMqttBoot boot helper (they cannot run on the host)
[group('build')]
clippy-provisioning-mqtt-tests:
    cargo clippy -p rustyfarian-esp-idf-network --features provisioning,mqtt --tests --target-dir {{ idf_dir }} -- -D warnings

# check the esp-idf espnow domain of the consolidated ESP-IDF network crate
[group('build')]
check-espnow:
    cargo check -p rustyfarian-esp-idf-network --features espnow --target-dir {{ idf_dir }}

# lint the esp-idf espnow worker, its host-less unit tests and the six ESP-NOW examples (none can run on the host)
[group('build')]
clippy-espnow:
    cargo clippy -p rustyfarian-esp-idf-network --features espnow,wifi --lib --tests --example idf_c3_espnow_coordinator --example idf_c6_espnow_coordinator --example idf_c3_espnow_peer --example idf_c3_espnow_scout --example idf_c3_espnow_scout_promisc --example idf_c3_espnow_sta_scout --target-dir {{ idf_dir }} -- -D warnings

# Fully cleans esp-idf-sys before AND after: switching sdkconfig variants forces a CMake reconfigure.
# ESP_IDF_SDKCONFIG_DEFAULTS paths MUST be absolute: embuild does not resolve relative overlay
# lists against the workspace root (relative silently applies neither file → false pass).
[doc('check that --features wifi still compiles with SoftAP disabled (STA-only consumers)')]
[group('build')]
check-sta-only:
    cargo clean -p esp-idf-sys --target-dir {{ idf_dir }}
    ESP_IDF_SDKCONFIG_DEFAULTS="{{ justfile_directory() }}/sdkconfig.defaults;{{ justfile_directory() }}/sdkconfig.sta-only.defaults" \
        cargo check -p rustyfarian-esp-idf-network --features wifi --target-dir {{ idf_dir }}
    cargo clean -p esp-idf-sys --target-dir {{ idf_dir }}

# check the ESP-IDF network crate with all features enabled together
[group('build')]
check-idf-all:
    cargo check -p rustyfarian-esp-idf-network --all-features --target-dir {{ idf_dir }}

# check all ESP-IDF domains (per-domain isolation gaps that --all-features masks)
[group('build')]
check-idf: check-wifi check-mqtt check-lora check-espnow check-ota-idf check-ota-mqtt-idf check-provisioning check-idf-all

# check the consolidated HAL network crate stub (no-default-features, host)
[group('build')]
check-hal-stub:
    cargo check -p rustyfarian-esp-hal-network --no-default-features --target-dir {{ hal_dir }}

alias check-ota-hal := check-hal-stub
alias check-provisioning-hal := check-hal-stub

# check the esp-hal provisioning crate cross-compiles cleanly to both bare-metal targets
[group('build')]
check-provisioning-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

# check the esp-hal ota crate with chip + embassy features (ESP32-C6 + ESP32-C3)
[group('build')]
check-ota-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,wifi,esp32c3,unstable,rt,embassy --example hal_c3_ota --target-dir {{ hal_dir }}

# run the bare-metal network crate's host tests (HTTP parser, provisioning substrate, OTA, provisioning)
[group('test & lint')]
test-hal:
    cargo test --target {{ host_target }} -p rustyfarian-esp-hal-network --no-default-features

alias test-dhcp := test-hal
alias test-http := test-hal
alias test-dns := test-hal
alias test-ota-hal := test-hal
alias test-provisioning-hal := test-hal
alias test-lora-hal := test-hal

# check the esp-hal lora domain cross-compiles for ESP32-C6 (bare-metal)
[group('build')]
check-lora-hal:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features lora,esp32c6,rt --target-dir {{ hal_dir }}

alias check-wifi-hal := check-hal-stub

# check the esp-hal wifi crate with the opt-in `embassy` feature (ESP32-C6 + ESP32-C3)
[group('build')]
check-wifi-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c6,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c3,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target xtensa-esp32-none-elf -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32,rt,embassy --target-dir {{ hal_dir }}

# check all HAL domains of the consolidated network crate
[group('build')]
check-mqtt-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

# check the esp-hal ESP-NOW driver cross-compiles for ESP32-C6 and ESP32-C3 (ADR 020)
[group('build')]
check-espnow-hal:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

[group('build')]
check-hal: check-wifi-hal-embassy check-lora-hal check-ota-hal-embassy check-provisioning-hal-embassy check-mqtt-hal-embassy check-espnow-hal

# check juggler compiles without the `std` feature (ADR 014 §2 no_std surface)
[group('build')]
check-network-pure-no-std:
    cargo check -p juggler --no-default-features
    cargo check -p juggler --no-default-features --features flash-store

# Lints all six domains (wifi, mqtt, espnow, lora, ota, provisioning) on both RISC-V targets, plus the Xtensa targets that support them.
# `-Zbuild-std=core,alloc` overrides the workspace [unstable] build-std default.
[doc('run clippy on the esp-hal network crate (bare-metal targets: ESP32-C6, ESP32-C3, ESP32-S3, ESP32)')]
[group('build')]
clippy-hal:
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c6,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features lora,esp32c6,rt -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features lora,esp32c3,rt -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c6,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c6,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32s3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32s3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32s3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32c6,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32c3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c6,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c6,unstable,rt,embassy --example hal_c6_espnow_sta_coordinator -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features espnow,esp32c3,unstable,rt,embassy --example hal_c3_espnow_peer -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32s3,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32s3,unstable,rt,embassy --example hal_esp32s3_provision_mqtt -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,wifi,esp32c3,unstable,rt,embassy --example hal_c3_ota -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target {{ hal_s3_target }} -p rustyfarian-esp-hal-network --no-default-features --features ota,wifi,esp32s3,unstable,rt,embassy --example hal_esp32s3_ota -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target xtensa-esp32-none-elf -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32,unstable,rt,embassy -- -D warnings
    cargo clippy -Zbuild-std=core,alloc --target xtensa-esp32-none-elf -p rustyfarian-esp-hal-network --no-default-features --features mqtt,esp32,unstable,rt,embassy -- -D warnings

# ── test & lint ───────────────────────────────────────────────────────────

# run clippy on the entire workspace
[group('test & lint')]
clippy:
    cargo clippy --all-targets --workspace -- -D warnings

# format all code
[group('test & lint')]
fmt:
    cargo fmt

# check formatting without modifying files
[group('test & lint')]
fmt-check:
    cargo fmt -- --check

# build rustdoc for all crates
[group('test & lint')]
doc:
    cargo doc --workspace --no-deps

# build juggler rustdoc on the host with rustdoc warnings as errors (all features, like docs.rs, so every intra-doc link resolves)
[group('test & lint')]
doc-juggler:
    RUSTDOCFLAGS="-D warnings" cargo doc --target {{ host_target }} -p juggler --all-features --no-deps

# build and open docs in browser
[group('test & lint')]
doc-open:
    cargo doc --workspace --no-deps --open

# validate Mermaid diagrams in markdown via mermaid-cli (requires Node.js/npx)
[group('test & lint')]
lint-docs:
    scripts/lint-docs.sh

# check dependency licenses, advisories, and bans
[group('test & lint')]
deny:
    cargo deny check

# audit dependencies for known security advisories (RUSTSEC)
[group('test & lint')]
audit:
    cargo audit

# The known exception (build-only `embuild`) is suppressed via
# `[package.metadata.cargo-machete] ignored` in Cargo.toml with justification comments.
[doc('find unused declared dependencies across the workspace')]
[group('test & lint')]
machete:
    cargo machete --with-metadata

# update dependencies (pass package flags to update specific crates, e.g. just update -p pennant)
[group('test & lint')]
update *args:
    cargo update {{ args }}

# run platform-independent backoff unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-backoff:
    cargo test --target {{ host_target }} -p juggler backoff

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-mqtt:
    cargo test --target {{ host_target }} -p juggler --features std mqtt
    cargo test --target {{ host_target }} -p juggler --features std wait_list

# run subscriber-thread and connect-thread deadlock regression tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-subscriber-thread:
    cargo test --target {{ host_target }} -p juggler --features std -- subscriber_thread connect_thread

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-wifi:
    cargo test --target {{ host_target }} -p juggler --features mock
    cargo test --target {{ host_target }} -p juggler --features wifi,std wifi::
    cargo test --target {{ host_target }} -p juggler --features wifi,std one_app_both_tiers

# run platform-independent LoRa unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-lora:
    cargo test --target {{ host_target }} -p juggler --features lora,mock

# run platform-independent ESP-NOW unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-espnow: test-wifi
    cargo test --target {{ host_target }} -p juggler --features espnow,mock,std espnow::

# run platform-independent OTA unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-ota:
    cargo test --target {{ host_target }} -p juggler --features ota-wire

# run platform-independent provisioning unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-provisioning:
    cargo test --target {{ host_target }} -p juggler --features provisioning,std
    cargo test --target {{ host_target }} -p juggler --features provisioning,std,lora
    cargo test --target {{ host_target }} -p juggler --features flash-store

# run all platform-independent unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test: test-backoff test-mqtt test-subscriber-thread test-wifi test-lora test-espnow test-ota test-provisioning test-hal

# ── examples ──────────────────────────────────────────────────────────────

# list all available hardware examples
[group('examples')]
examples:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Available examples (use with: just run <example>):"
    echo ""
    for f in crates/*/examples/*.rs; do
        name=$(basename "$f" .rs)
        crate=$(echo "$f" | cut -d/ -f2)
        printf "  %-40s  (%s)\n" "$name" "$crate"
    done

# build a named example (chip and crate auto-detected from example name)
[group('examples')]
build-example example:
    scripts/build-example.sh "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# build every example of both tier crates (discovered from examples/*.rs, common/ skipped) via build-example;
# optional filter = example name prefix (e.g. `hal_`, `idf_c3`); stops at the first failure unless KEEP_GOING=1
[doc('build every example of both tier crates via build-example (optional filter = example name prefix)')]
[group('examples')]
build-all-examples filter="":
    #!/usr/bin/env bash
    names=()
    for f in crates/rustyfarian-esp-idf-network/examples/*.rs crates/rustyfarian-esp-hal-network/examples/*.rs; do
        [ -e "$f" ] || continue
        name=$(basename "$f" .rs)
        case "$name" in
            {{ filter }}*) names+=("$name") ;;
        esac
    done
    total=${#names[@]}
    if [ "$total" -eq 0 ]; then
        echo "build-all-examples: no example matches prefix '{{ filter }}'" >&2
        exit 1
    fi
    passed=()
    failed=()
    i=0
    for name in "${names[@]}"; do
        i=$((i + 1))
        echo "==> [$i/$total] $name"
        if just build-example "$name"; then
            passed+=("$name")
        else
            failed+=("$name")
            [ -n "${KEEP_GOING:-}" ] || break
        fi
    done
    echo
    echo "build-all-examples: ${#passed[@]} built, ${#failed[@]} failed, $((total - ${#passed[@]} - ${#failed[@]})) not attempted (of $total)"
    for name in "${failed[@]}"; do
        echo "  FAILED: $name"
    done
    [ ${#failed[@]} -eq 0 ]

# build one Xtensa ESP-IDF wifi+mqtt example per chip (esp32, esp32s3); `just verify-idf` compiles only RISC-V, and the Xtensa
# LLVM backend once failed here with "Cannot scavenge register without an emergency spill slot"
[doc('build one Xtensa ESP-IDF wifi+mqtt example per chip (esp32, esp32s3)')]
[group('examples')]
build-xtensa-idf-mqtt:
    just build-example idf_esp32_mqtt
    just build-example idf_esp32s3_mqtt_lwt

# ensure the IDF-built bootloader of the pinned ESP-IDF is cached for the chip (variant: generic | ota)
[group('examples')]
ensure-bootloader chip variant="generic":
    scripts/ensure-bootloader.sh {{ if variant == "ota" { "--ota" } else { "" } }} "{{ chip }}" "{{ idf_dir }}"

# build and flash a named example (chip and crate auto-detected from example name)
[group('examples')]
flash example:
    scripts/flash.sh "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# build, flash, and open the serial monitor (run without args to list examples)
[group('examples')]
run *example:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{ example }}" ]; then
        just examples
    else
        just flash "{{ example }}"
        scripts/espflash.sh monitor --non-interactive
    fi

# erase flash (NVS + app), rebuild from clean, flash, and monitor
[group('examples')]
fresh-run example:
    just clean
    just erase-flash
    just run {{ example }}

# erase entire flash (NVS, app, bootloader) — fixes stale WiFi credentials and corrupt state
[confirm("Erase the ENTIRE flash (NVS, app, bootloader) on the connected device? [y/N]")]
[group('examples')]
erase-flash:
    scripts/espflash.sh erase-flash

# open the serial monitor for an already-flashed device
[group('examples')]
monitor:
    scripts/espflash.sh monitor --non-interactive

# show the USB serial port espflash would pick and identify the attached chip
[group('examples')]
ports:
    #!/usr/bin/env bash
    set -euo pipefail
    port="$(scripts/detect-port.sh)"
    if [ -n "$port" ]; then
        printf 'Port: %s\n' "$port"
        scripts/espflash.sh board-info
    fi

# guided hardware check: `just bench c3 mqtt-lwt`, `just bench hal_esp32s3_mqtt_lwt`, `just bench c3` (see bench/README.md)
[group('examples')]
bench target *args:
    {{ bench }} --repo . run "{{ target }}" {{ args }}

# list the bench scenarios
[group('examples')]
bench-list:
    {{ bench }} --repo . list

# probe the connected boards (chip and MAC per port)
[group('examples')]
bench-detect:
    {{ bench }} detect

# watch an MQTT example's topics with timestamps and the retain flag (`retain=1` is a replay at subscribe, `retain=0` a live message; MQTT_* from the dotenv file); target: hal (default), idf, all, a prefix such as a client id, or a filter with + or #; seconds > 0 stops after that long, e.g. `just mqtt-watch idf 30`
[group('examples')]
mqtt-watch target="" seconds="0":
    #!/usr/bin/env bash
    set -euo pipefail
    prefix="$(scripts/mqtt-topic.sh "{{ target }}")"
    case "$prefix" in *[+#]*) filter="$prefix" ;; *) filter="$prefix/#" ;; esac
    stop=()
    [ "{{ seconds }}" != "0" ] && stop=(-W "{{ seconds }}")
    rc=0
    mosquitto_sub -h "${MQTT_HOST}" -p "${MQTT_PORT:-1883}" ${MQTT_USER:+-u "$MQTT_USER"} ${MQTT_PASS:+-P "$MQTT_PASS"} -t "$filter" -F '%I %t retain=%r %p' ${stop[@]+"${stop[@]}"} || rc=$?
    # -W ends with code 27 when the time is up; that is the expected end of a timed watch.
    [ "$rc" -eq 0 ] || [ "$rc" -eq 27 ] || exit "$rc"

# publish one message to <prefix>/<topic> on the dotenv broker; target as for mqtt-watch, e.g. `just mqtt-pub cmd hello` or `just mqtt-pub cmd ping idf`
[group('examples')]
mqtt-pub topic message target="":
    mosquitto_pub -h "${MQTT_HOST}" -p "${MQTT_PORT:-1883}" ${MQTT_USER:+-u "$MQTT_USER"} ${MQTT_PASS:+-P "$MQTT_PASS"} -t "$(scripts/mqtt-topic.sh "{{ target }}")/{{ topic }}" -m "{{ message }}"

# publish <count> numbered messages (msg-1 … msg-N) to <prefix>/<topic> back to back, a message-load test; target as for mqtt-watch, e.g. `just mqtt-burst cmd 50`
[group('examples')]
mqtt-burst topic count target="":
    seq 1 {{ count }} | sed 's/^/msg-/' | mosquitto_pub -h "${MQTT_HOST}" -p "${MQTT_PORT:-1883}" ${MQTT_USER:+-u "$MQTT_USER"} ${MQTT_PASS:+-P "$MQTT_PASS"} -t "$(scripts/mqtt-topic.sh "{{ target }}")/{{ topic }}" -l

# print the retained <prefix>/status value once (expects `online` while the device is up); target as for mqtt-watch, e.g. `just mqtt-status idf`
[group('examples')]
mqtt-status target="":
    mosquitto_sub -h "${MQTT_HOST}" -p "${MQTT_PORT:-1883}" ${MQTT_USER:+-u "$MQTT_USER"} ${MQTT_PASS:+-P "$MQTT_PASS"} -t "$(scripts/mqtt-topic.sh "{{ target }}")/status" -C 1 -W 5

# submit the provisioning form with x_* extras to a running SoftAP portal (WIFI_*, MQTT_*, OTA_URL, DEVICE_NAME from the dotenv file); positional args, e.g. `just portal-save 192.168.4.1 8 13 64`; extras 9, key_len 14 or value_len 65 exceed the caps and return 400; key_len must be at least 5
[group('examples')]
portal-save host="192.168.4.1" extras="2" key_len="6" value_len="8":
    scripts/portal-save.sh "{{ host }}" "{{ extras }}" "{{ key_len }}" "{{ value_len }}"

# ── maintenance ───────────────────────────────────────────────────────────

# clean build artifacts (target/ide, hal and idf target dirs) and scratch output in tmp/
[group('maintenance')]
clean:
    cargo clean --target-dir target/ide
    cargo clean --target-dir {{ hal_dir }}
    cargo clean --target-dir {{ idf_dir }}
    find tmp -mindepth 1 -maxdepth 1 ! -name .gitkeep -exec rm -rf {} +

# remove every esp-idf-sys build (all triples and profiles in target/, target/ide and the idf dir); run once after changing ESP_IDF_VERSION, whose stale bootloader CMake cache otherwise fails with "does not match the source"
[group('maintenance')]
clean-idf-sys:
    rm -rf target/*/*/build/esp-idf-sys-* target/*/*/*/build/esp-idf-sys-* {{ idf_dir }}/*/*/build/esp-idf-sys-*

# clean only the ESP-IDF crate's build artifacts (needed after sdkconfig changes or chip switch)
[group('maintenance')]
clean-idf:
    cargo clean -p rustyfarian-esp-idf-network --target-dir {{ idf_dir }}
    rm -rf {{ idf_dir }}/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/
    rm -rf {{ idf_dir }}/riscv32imc-esp-espidf/release/build/esp-idf-sys-*/

# clean only the bare-metal (esp-hal) target dir — frees the shared RAM disk without forcing an ESP-IDF rebuild
[group('maintenance')]
clean-hal:
    cargo clean --target-dir {{ hal_dir }}

# check that provisioning library logs never interpolate credential field names
[group('maintenance')]
check-no-credential-logging:
    scripts/check-no-credential-logging.sh

# check that library boundaries are respected (no restarts in OTA store, no reboots in provisioning)
[group('maintenance')]
check-library-never-reboots:
    scripts/check-library-never-reboots.sh

# ── ota ───────────────────────────────────────────────────────────────────

# build an OTA example and produce app binary, SHA-256, manifest, and command (version first)
[group('ota')]
ota-image version example="idf_c3_ota_runtime":
    scripts/ota-image.sh "{{ version }}" "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# baseline flash of idf_c3_ota_runtime with FIRMWARE_VERSION=version
[group('ota')]
ota-flash version:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! printf '%s' "{{ version }}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
        echo "Error: version must be MAJOR.MINOR.PATCH, got '{{ version }}'" >&2
        exit 1
    fi
    FIRMWARE_VERSION="{{ version }}" scripts/flash.sh "idf_c3_ota_runtime" "{{ hal_dir }}" "{{ idf_dir }}"

# build an unhealthy OTA image (OTA_DEMO_UNHEALTHY=1, optional failure deadline in s)
[group('ota')]
ota-image-unhealthy version deadline="" example="idf_c3_ota_runtime":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! printf '%s' "{{ version }}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
        echo "Error: version must be MAJOR.MINOR.PATCH, got '{{ version }}'" >&2
        exit 1
    fi
    if [ -n "{{ deadline }}" ] && ! printf '%s' "{{ deadline }}" | grep -Eq '^[0-9]+$'; then
        echo "Error: deadline must be a whole number of seconds, got '{{ deadline }}'" >&2
        exit 1
    fi
    export FIRMWARE_VERSION="{{ version }}" OTA_DEMO_UNHEALTHY=1
    if [ -n "{{ deadline }}" ]; then
        export OTA_FAILURE_DEADLINE_SECS="{{ deadline }}"
    fi
    scripts/ota-image.sh "{{ version }}" "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# publish OTA manifest command for version (fails if not built)
[group('ota')]
ota-push version:
    scripts/ota-mqtt.sh cmd "{{ version }}"

# publish tampered manifest command (kind: target|checksum)
[group('ota')]
ota-cmd-tampered version kind:
    scripts/ota-mqtt.sh cmd-tampered "{{ version }}" "{{ kind }}"

# publish raw payload to OTA command topic
[group('ota')]
ota-pub payload:
    scripts/ota-mqtt.sh pub {{ quote(payload) }}

# publish rollback action
[group('ota')]
ota-rollback from:
    scripts/ota-mqtt.sh rollback "{{ from }}"

# publish repair action
[group('ota')]
ota-repair:
    scripts/ota-mqtt.sh repair

# burst publish count commands (kind: repair|invalid)
[group('ota')]
ota-burst count kind:
    scripts/ota-mqtt.sh burst "{{ count }}" "{{ kind }}"

# subscribe to OTA status topic with ISO timestamps (topic: default status)
[group('ota')]
ota-status topic="status":
    scripts/ota-mqtt.sh sub "{{ topic }}"

# flash the baseline of a bare-metal OTA example: it fetches target_version's image (built by ota-image) from ota_host
[group('ota')]
flash-ota-baseline example baseline target_version ota_host:
    #!/usr/bin/env bash
    set -euo pipefail
    for v in "{{ baseline }}" "{{ target_version }}"; do
        if ! printf '%s' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
            echo "Error: versions must be MAJOR.MINOR.PATCH, got '$v'" >&2
            exit 1
        fi
    done
    image="target/ota/{{ example }}-v{{ target_version }}.bin"
    if [ ! -f "$image" ]; then
        echo "$image does not exist; run 'just ota-image {{ target_version }} {{ example }}' first" >&2
        exit 1
    fi
    export FIRMWARE_VERSION="{{ baseline }}"
    export OTA_HOST="{{ ota_host }}"
    export OTA_PORT="${OTA_PORT:-8000}"
    export OTA_PATH="$(basename "$image")"
    export OTA_SHA256="$(shasum -a 256 "$image" | cut -d' ' -f1)"
    export OTA_TARGET_VERSION="{{ target_version }}"
    scripts/flash.sh "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# bench OTA round trip: build the served image (unhealthy when the check name contains `rollback`), export the
# baseline's build-time variables for the runner's build step, then run the bench check, e.g.
# `just bench-ota s3 ota hal_esp32s3_ota 1.0.0 2.0.0 192.168.40.146 --port /dev/cu.usbmodem2101`
[doc('bench OTA round trip: build the served image, export the baseline build-time variables, run the bench check')]
[group('ota')]
bench-ota board check example baseline target_version ota_host *args:
    #!/usr/bin/env bash
    set -euo pipefail
    for v in "{{ baseline }}" "{{ target_version }}"; do
        if ! printf '%s' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
            echo "Error: versions must be MAJOR.MINOR.PATCH, got '$v'" >&2
            exit 1
        fi
    done
    case "{{ check }}" in
        *rollback*) just ota-image-unhealthy "{{ target_version }}" "" "{{ example }}" ;;
        *) just ota-image "{{ target_version }}" "{{ example }}" ;;
    esac
    image="target/ota/{{ example }}-v{{ target_version }}.bin"
    export FIRMWARE_VERSION="{{ baseline }}"
    export OTA_HOST="{{ ota_host }}"
    export OTA_PORT="${OTA_PORT:-8000}"
    export OTA_PATH="$(basename "$image")"
    export OTA_SHA256="$(shasum -a 256 "$image" | cut -d' ' -f1)"
    export OTA_TARGET_VERSION="{{ target_version }}"
    just bench "{{ board }}" "{{ check }}" {{ args }}

# Usage: just ota-serve              (serve at full speed)
#        just ota-serve 10240        (throttle to 10 KB/s)
#        just ota-serve 0 102400     (full speed, then stall after 100 KiB: download_timeout test)
# Port: OTA_PORT (default 8000), the same variable `ota-image` writes into the URLs.
[doc('serve OTA images from target/ota/ over HTTP with optional throttle')]
[group('ota')]
ota-serve rate="0" stall="0":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -d target/ota ]; then
        echo "target/ota does not exist; run 'just ota-image <version>' first" >&2
        exit 1
    fi
    exec python3 scripts/ota-server.py target/ota "${OTA_PORT:-8000}" "{{ rate }}" "{{ stall }}"

# Port: LOCAL_MQTT_PORT (default 1883); `up` prints the MQTT_HOST and MQTT_PORT to use.
[doc('manage a local test MQTT broker: just mosquitto up | down | status | restart')]
[group('ota')]
mosquitto action:
    @scripts/mosquitto.sh "{{ action }}"

# ── ci ────────────────────────────────────────────────────────────────────

# local pre-commit gate: format (modifies files), then verify
[group('ci')]
pre-commit: fmt verify

# read-only, offline, host-only gate: format check, host lints and host tests
[group('ci')]
verify: fmt-check clippy-pure clippy-ota-pure check-no-credential-logging check-library-never-reboots test

# ESP-IDF device checks and lints on the workspace default RISC-V target (needs the ESP toolchain)
[group('ci')]
verify-idf: check check-ota-pure clippy check-idf clippy-provisioning-tests clippy-provisioning-mqtt-tests clippy-ota-mqtt-idf clippy-espnow

# bare-metal esp-hal device checks (needs the ESP toolchain for -Zbuild-std)
[group('ci')]
verify-hal: check-hal check-ota-wire-pure

# CI-equivalent: verify, deny, then the device gates
[group('ci')]
ci: verify deny verify-idf verify-hal

# CI job (fmt.yml): format check on stable, no ESP toolchain
[group('ci')]
ci-fmt: fmt-check

# CI job (rust.yml): cargo-deny licenses, advisories, bans and sources, on stable
[group('ci')]
ci-deny: deny

# CI job (rust.yml): all platform-independent host unit tests, on stable
[group('ci')]
ci-test: test

# CI job (clippy.yml): lint the pure juggler crate on the host, on stable
[group('ci')]
ci-clippy: clippy-pure clippy-ota-pure

# CI job (rust.yml): workspace check and the pure OTA checks on the ESP-IDF default target
[group('ci')]
ci-check: check check-ota-pure check-ota-wire-pure

# CI job (rust.yml): check the Wi-Fi STA-only build with SoftAP disabled (ESP-IDF)
[group('ci')]
ci-check-sta-only: check-sta-only

# CI job (clippy.yml): lint the whole workspace on the ESP-IDF default target
[group('ci')]
ci-clippy-idf: clippy

# CI job (clippy.yml): lint the bare-metal esp-hal tier on its own C3/C6/S3/classic ESP32 targets
[group('ci')]
ci-clippy-hal: clippy-hal

# CI job (rust.yml): build the ESP-IDF and bare-metal smoke examples
[group('ci')]
ci-build-examples:
    just build-example idf_c3_connect
    just build-example hal_c3_connect_async

# CI job (audit.yml): cargo-audit advisories
[group('ci')]
ci-audit: audit

# run the CI jobs locally via act: all jobs, or one (`just act fmt`; requires Docker + act)
[group('ci')]
act job="":
    #!/usr/bin/env bash
    set -euo pipefail
    for j in {{ if job == "" { "fmt clippy test deny audit clippy-idf clippy-hal check check-sta-only build-examples" } else { job } }}; do
        act -j "$j" --pull=false -P ubuntu-latest=catthehacker/ubuntu:act-latest {{ ci_repo_flag }}
    done

# ── release ───────────────────────────────────────────────────────────────

# pre-flight release validation: version lockstep, verify, package contents, and `cargo publish --dry-run`
[group('release')]
release-publish-validate:
    scripts/release-validate.sh

# check semver compatibility against v0.7.0 tag (tool-checkable crates only; manual review for cross-target crates)
[group('release')]
semver:
    scripts/release-semver-check.sh

# juggler gets a full `cargo publish --dry-run` (host-buildable); the two -network crates get
# `cargo package --list` because their `cargo publish --dry-run` resolves `juggler ^0.7` against
# the crates.io index, which only succeeds AFTER juggler is published — their real dry-run therefore
# happens as the ordered publish proceeds.
[doc('pre-publish packaging validation (needs a clean tree)')]
[group('release')]
release-dry-run:
    cargo publish --dry-run -p juggler --target {{ host_target }} --all-features
    cargo package --list -p rustyfarian-esp-idf-network > /dev/null
    cargo package --list -p rustyfarian-esp-hal-network > /dev/null

# verify IDF network crate packages cleanly against IDF target (no upload; requires espup)
[group('release')]
release-dry-run-idf:
    cargo +esp publish --dry-run -p rustyfarian-esp-idf-network --target {{ idf_target }} --target-dir {{ idf_dir }}

# verify HAL network crate packages cleanly against bare-metal target (no upload)
[group('release')]
release-dry-run-hal:
    cargo publish --dry-run -p rustyfarian-esp-hal-network -Zbuild-std=core,alloc --target {{ hal_target }} --target-dir {{ hal_dir }}

# publish pure crate (juggler) to crates.io
[confirm]
[group('release')]
release-publish crate:
    cargo publish -p {{ crate }} --target {{ host_target }}

# publish IDF network crate to crates.io (requires espup)
[confirm]
[group('release')]
release-publish-idf:
    cargo +esp publish -p rustyfarian-esp-idf-network --target {{ idf_target }} --target-dir {{ idf_dir }}

# publish HAL network crate to crates.io
[confirm]
[group('release')]
release-publish-hal:
    cargo publish -p rustyfarian-esp-hal-network -Zbuild-std=core,alloc --target {{ hal_target }} --target-dir {{ hal_dir }}

# ── setup ─────────────────────────────────────────────────────────────────

# install what a fresh clone is missing: component esp (ESP-IDF toolchain, ldproxy, espflash) or tools (cargo-deny, cargo-audit); empty = both; idempotent; ends with doctor
[group('setup')]
setup component="":
    #!/usr/bin/env bash
    set -euo pipefail
    ensure() { command -v "$1" >/dev/null 2>&1 || cargo install --locked --target {{ host_target }} "$2"; }
    case "{{ component }}" in
        ""|esp|tools) ;;
        *) echo "setup: unknown component '{{ component }}' (use esp or tools)" >&2; exit 1 ;;
    esac
    if [ -z "{{ component }}" ] || [ "{{ component }}" = esp ]; then
        ensure espup espup
        if ! rustup toolchain list | awk '/^esp/ { found = 1 } END { exit !found }'; then
            echo "Installing ESP-IDF toolchain..."
            espup install
        fi
        ensure ldproxy ldproxy
        ensure espflash espflash
    fi
    if [ -z "{{ component }}" ] || [ "{{ component }}" = tools ]; then
        ensure cargo-deny cargo-deny
        ensure cargo-audit cargo-audit
    fi
    just doctor
