# Rustyfarian Network — development tasks
#
# ESP-IDF crates require the ESP-IDF toolchain (`just setup-toolchain`).
# Pure crates (rustyfarian-network-pure, wifi-pure, lora-pure) compile and
# test on any host without the ESP toolchain.
# Run `just setup-cargo-config` to create the local cargo config.

# list available recipes (default)
_default:
    @just --list

# host target, used to override the workspace ESP-IDF target for pure-logic tests
host_target := `scripts/host-target.sh`

# platform-independent crates that can be compiled and tested on the host
pure_crates := "-p rustyfarian-network-pure -p wifi-pure -p espnow-pure"

ramdisk := "/Volumes/RustBuilds"
hal_dir  := if path_exists(ramdisk + "/targets/hal") == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir  := if path_exists(ramdisk + "/targets/idf") == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }

# ── Build Environment ─────────────────────────────────────────────────────

# show RAM disk status, resolved target dirs, and sccache
doctor:
    @scripts/doctor.sh "{{ramdisk}}" "{{hal_dir}}" "{{idf_dir}}"

# manage the RAM disk: just ramdisk attach | detach
ramdisk action:
    @scripts/ramdisk.sh "{{action}}"

# ── Build & Check ─────────────────────────────────────────────────────────

# build the entire workspace (release)
build:
    cargo build --release

# check the entire workspace
check:
    cargo check

# check the wifi crate
check-wifi:
    cargo check -p rustyfarian-esp-idf-wifi --target-dir {{ idf_dir }}

# check the mqtt crate
check-mqtt:
    cargo check -p rustyfarian-esp-idf-mqtt --target-dir {{ idf_dir }}

# check the esp-idf lora crate
check-lora:
    cargo check -p rustyfarian-esp-idf-lora --target-dir {{ idf_dir }}

# check the pure lora crate (no ESP-IDF required)
check-lora-pure:
    cargo check -p lora-pure

# check the pure wifi crate (no ESP-IDF required)
check-wifi-pure:
    cargo check -p wifi-pure

# check the pure espnow crate (no ESP-IDF required)
check-espnow-pure:
    cargo check -p espnow-pure

# check the pure ota crate (no ESP-IDF required)
check-ota-pure:
    cargo check -p ota-pure

# check the pure provisioning crate (no ESP-IDF required)
check-provisioning-pure:
    cargo check -p provisioning-pure

# check the esp-idf ota crate
check-ota-idf:
    cargo check -p rustyfarian-esp-idf-ota --target-dir {{ idf_dir }}

# check the esp-idf provisioning crate
check-provisioning:
    cargo check -p rustyfarian-esp-idf-provisioning --target-dir {{ idf_dir }}

# check the esp-hal ota stub (no-default-features to avoid esp-hal target conflict)
check-ota-hal:
    cargo check -p rustyfarian-esp-hal-ota --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal ota crate with chip + embassy features (ESP32-C6 + ESP32-C3)
check-ota-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-ota --no-default-features --features esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-ota --no-default-features --features esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

# run platform-independent HTTP parser unit tests (host toolchain, no ESP toolchain needed)
test-ota-hal:
    cargo test --target {{host_target}} -p rustyfarian-esp-hal-ota --no-default-features

# check the esp-idf espnow crate
check-espnow:
    cargo check -p rustyfarian-esp-idf-espnow --target-dir {{ idf_dir }}

# check the esp-hal lora stub (no-default-features to avoid esp-hal target conflict)
check-lora-hal:
    cargo check -p rustyfarian-esp-hal-lora --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal wifi stub (no-default-features to avoid esp-hal target conflict)
check-wifi-hal:
    cargo check -p rustyfarian-esp-hal-wifi --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal wifi crate with the opt-in `embassy` feature (ESP32-C6 + ESP32-C3)
# `-Zbuild-std=core,alloc` overrides the workspace [unstable] build-std default.
# Also checks the `provisioning-spike` feature (hand-rolled DHCP server) on both chips.
check-wifi-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-wifi --no-default-features --features esp32c6,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-wifi --no-default-features --features esp32c3,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-wifi --no-default-features --features esp32c6,rt,provisioning-spike --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-wifi --no-default-features --features esp32c3,rt,provisioning-spike --target-dir {{ hal_dir }}

# check rustyfarian-network-pure compiles without the `std` feature (ADR 014 §2 no_std surface)
check-network-pure-no-std:
    cargo check -p rustyfarian-network-pure --no-default-features

# ── Test & Lint ───────────────────────────────────────────────────────────

# run clippy on the entire workspace
clippy:
    cargo clippy --all-targets --workspace -- -D warnings

# format all code
fmt:
    cargo fmt

# check formatting without modifying files
fmt-check:
    cargo fmt -- --check

# build rustdoc for all crates
doc:
    cargo doc --workspace --no-deps

# build and open docs in browser
doc-open:
    cargo doc --workspace --no-deps --open

# validate Mermaid diagrams in markdown via mermaid-cli (requires Node.js/npx)
lint-docs:
    scripts/lint-docs.sh

# check dependency licenses, advisories, and bans
deny:
    cargo deny check

# update dependencies (pass package flags to update specific crates, e.g. just update -p led-effects)
update *args:
    cargo update {{args}}

# run platform-independent backoff unit tests (host toolchain, no ESP-IDF needed)
test-backoff:
    cargo test --target {{host_target}} -p rustyfarian-network-pure backoff

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
test-mqtt:
    cargo test --target {{host_target}} {{pure_crates}} mqtt

# run subscriber-thread deadlock regression tests (host toolchain, no ESP-IDF needed)
test-subscriber-thread:
    cargo test --target {{host_target}} -p rustyfarian-network-pure subscriber_thread

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
test-wifi:
    cargo test --target {{host_target}} -p wifi-pure --features mock

# run platform-independent LoRa unit tests (host toolchain, no ESP-IDF needed)
test-lora:
    cargo test --target {{host_target}} -p lora-pure --features mock

# run platform-independent ESP-NOW unit tests (host toolchain, no ESP-IDF needed)
test-espnow:
    cargo test --target {{host_target}} -p espnow-pure --features mock

# run platform-independent OTA unit tests (host toolchain, no ESP-IDF needed)
test-ota:
    cargo test --target {{host_target}} -p ota-pure

# run platform-independent provisioning unit tests (host toolchain, no ESP-IDF needed)
test-provisioning:
    cargo test --target {{host_target}} -p provisioning-pure

# run platform-independent DHCP codec and allocation-policy unit tests (host toolchain)
# Uses the `provisioning-spike` feature which enables the dhcp module; no chip feature
# is selected so the async `run()` function is compiled away and no ESP toolchain is needed.
test-dhcp:
    cargo test --target {{host_target}} -p rustyfarian-esp-hal-wifi --no-default-features --features provisioning-spike

# run platform-independent HTTP parser and routing unit tests (host toolchain)
# Uses the `provisioning-spike` feature which enables the http_server module; no chip
# feature is selected so the async `run()` function is compiled away and no ESP toolchain
# is needed.
test-http:
    cargo test --target {{host_target}} -p rustyfarian-esp-hal-wifi --no-default-features --features provisioning-spike

# run platform-independent DNS catch-all codec unit tests (host toolchain)
# Uses the `provisioning-spike` feature which enables the dns_catchall module; no chip
# feature is selected so the async `run()` function is compiled away and no ESP toolchain
# is needed.
test-dns:
    cargo test --target {{host_target}} -p rustyfarian-esp-hal-wifi --no-default-features --features provisioning-spike

# run all platform-independent unit tests using {{pure_crates}} (host toolchain, no ESP-IDF needed)
test: test-backoff test-mqtt test-subscriber-thread test-wifi test-lora test-espnow test-ota test-ota-hal test-provisioning test-dhcp test-http test-dns

# ── Examples ──────────────────────────────────────────────────────────────

# list all available hardware examples
examples:
    #!/usr/bin/env bash
    echo "Available examples (use with: just run <example>):"
    echo ""
    for f in crates/*/examples/*.rs; do
        name=$(basename "$f" .rs)
        crate=$(echo "$f" | cut -d/ -f2)
        printf "  %-40s  (%s)\n" "$name" "$crate"
    done

# build a named example; chip and crate auto-detected from example name
build-example example:
    scripts/build-example.sh "{{example}}" "{{hal_dir}}" "{{idf_dir}}"

# serial port for espflash; honoured verbatim if set, otherwise scripts/detect-port.sh
# narrows espflash auto-detect to USB serial devices (usbmodem/usbserial on macOS,
# ttyUSB/ttyACM on Linux) so paired Bluetooth ports do not get picked.
export ESPFLASH_PORT := env("ESPFLASH_PORT", "")

# ensure the IDF-built v5.3.3 bootloader is in the build cache for the given chip
ensure-bootloader chip:
    scripts/ensure-bootloader.sh "{{chip}}" "{{hal_dir}}" "{{idf_dir}}"

# build and flash a named example; chip and crate auto-detected from example name
flash example:
    scripts/flash.sh "{{example}}" "{{hal_dir}}" "{{idf_dir}}"

# build, flash, and open the serial monitor (run without args to list examples)
run *example:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{example}}" ]; then
        just examples
    else
        just flash "{{example}}"
        port="$(scripts/detect-port.sh)"
        port_args=()
        [ -n "$port" ] && port_args=(--port "$port")
        espflash monitor --non-interactive "${port_args[@]}"
    fi

# erase flash (NVS + app), rebuild from clean, flash, and monitor
fresh-run example:
    just clean
    just erase-flash
    just run {{example}}

# erase entire flash (NVS, app, bootloader) — fixes stale WiFi credentials and corrupt state
erase-flash:
    #!/usr/bin/env bash
    set -euo pipefail
    port="$(scripts/detect-port.sh)"
    port_args=()
    [ -n "$port" ] && port_args=(--port "$port")
    espflash erase-flash "${port_args[@]}"

# open the serial monitor for an already-flashed device
monitor:
    #!/usr/bin/env bash
    set -euo pipefail
    port="$(scripts/detect-port.sh)"
    port_args=()
    [ -n "$port" ] && port_args=(--port "$port")
    espflash monitor --non-interactive "${port_args[@]}"

# ── Maintenance ───────────────────────────────────────────────────────────

# clean build artifacts (target/ide, hal and idf target dirs)
clean:
    cargo clean --target-dir target/ide
    cargo clean --target-dir {{ hal_dir }}
    cargo clean --target-dir {{ idf_dir }}

# clean only the ESP-IDF crate's build artifacts (needed after sdkconfig changes or chip switch)
clean-idf:
    cargo clean -p rustyfarian-esp-idf-wifi --target-dir {{ idf_dir }}
    cargo clean -p rustyfarian-esp-idf-mqtt --target-dir {{ idf_dir }}
    rm -rf {{ idf_dir }}/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/
    rm -rf {{ idf_dir }}/riscv32imc-esp-espidf/release/build/esp-idf-sys-*/

# ── CI ────────────────────────────────────────────────────────────────────

# full pre-commit verification: format, check, lint (local use only — modifies files)
pre-commit: fmt check clippy

# non-modifying full verification: fails on any anomaly; suggests fix recipe on failure
verify:
    just fmt-check || (echo; echo "Formatting issues found — run 'just pre-commit' to auto-fix."; echo; exit 1)
    just ci

# CI-equivalent verification (non-modifying): format check, deny, check, lint
ci: fmt-check deny check clippy

# ── Local CI via act ──────────────────────────────────────────────────────

# run all CI workflows locally via act (requires Docker + act)
act *job:
    #!/usr/bin/env bash
    if [ -z "{{job}}" ]; then
        just act fmt && just act clippy && just act ci && just act audit
    else
        act -j "{{job}}"
    fi

# ── Setup ─────────────────────────────────────────────────────────────────

# set up local cargo config from the template
setup-cargo-config:
    cp .cargo/config.toml.dist .cargo/config.toml

# install the ESP-IDF toolchain via espup
setup-toolchain:
    espup install
