# Rustyfarian Network — development tasks
#
# ESP-IDF crates require the ESP-IDF toolchain (`just setup-toolchain`).
# The consolidated pure crate (`juggler`) compiles and tests on any
# host without the ESP toolchain.
# Run `just setup-cargo-config` to create the local cargo config.

# load .env file (LoRaWAN and WiFi credentials, MQTT config)
set dotenv-load

# list available recipes (default)
_default:
    @just --list

# host target, used to override the workspace ESP-IDF target for pure-logic tests
host_target := `scripts/host-target.sh`

# bare-metal target for HAL crates (publish / dry-run)
hal_target := "riscv32imac-unknown-none-elf"

# ESP-IDF target (publish / dry-run)
idf_target := "riscv32imac-esp-espidf"

ramdisk := "/Volumes/RustBuilds"
hal_dir := if path_exists(ramdisk + "/targets/hal") == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir := if path_exists(ramdisk + "/targets/idf") == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }

# ── Build Environment ─────────────────────────────────────────────────────

# show RAM disk status, resolved target dirs, and sccache
doctor:
    @scripts/doctor.sh "{{ ramdisk }}" "{{ hal_dir }}" "{{ idf_dir }}"

# manage the RAM disk: just ramdisk attach | detach
ramdisk action:
    @scripts/ramdisk.sh "{{ action }}"

# ── Build & Check ─────────────────────────────────────────────────────────

# build the entire workspace (release)
build:
    cargo build --release

# check the entire workspace
check:
    cargo check

# check the wifi domain of the consolidated ESP-IDF network crate
check-wifi:
    cargo check -p rustyfarian-esp-idf-network --features wifi --target-dir {{ idf_dir }}

# check the mqtt domain of the consolidated ESP-IDF network crate
check-mqtt:
    cargo check -p rustyfarian-esp-idf-network --features wifi,mqtt --target-dir {{ idf_dir }}

# check the esp-idf lora domain of the consolidated ESP-IDF network crate
check-lora:
    cargo check -p rustyfarian-esp-idf-network --features lora --target-dir {{ idf_dir }}

# check the consolidated pure crate with all features (no ESP-IDF required)
check-pure:
    cargo check -p juggler --all-features

# check the pure lora feature (no ESP-IDF required)
check-lora-pure:
    cargo check -p juggler --features lora

# check the pure wifi feature (no ESP-IDF required)
check-wifi-pure:
    cargo check -p juggler --features wifi

# check the pure espnow feature (no ESP-IDF required)
check-espnow-pure:
    cargo check -p juggler --features espnow

# check the pure ota feature (no ESP-IDF required)
check-ota-pure:
    cargo check -p juggler --features ota

# check the pure provisioning feature (no ESP-IDF required)
check-provisioning-pure:
    cargo check -p juggler --features provisioning

# check the esp-idf ota domain of the consolidated ESP-IDF network crate
check-ota-idf:
    cargo check -p rustyfarian-esp-idf-network --features ota --target-dir {{ idf_dir }}

# check the esp-idf provisioning domain of the consolidated ESP-IDF network crate
check-provisioning:
    cargo check -p rustyfarian-esp-idf-network --features provisioning --target-dir {{ idf_dir }}

# check the esp-idf espnow domain of the consolidated ESP-IDF network crate
check-espnow:
    cargo check -p rustyfarian-esp-idf-network --features espnow --target-dir {{ idf_dir }}

# check the ESP-IDF network crate with all features enabled together
check-idf-all:
    cargo check -p rustyfarian-esp-idf-network --all-features --target-dir {{ idf_dir }}

# check all ESP-IDF domains: representative per-domain feature combos (catches
# per-domain isolation gaps that --all-features masks) plus the all-features build
check-idf: check-wifi check-mqtt check-lora check-espnow check-ota-idf check-provisioning check-idf-all

# check the consolidated HAL network crate stub (no-default-features, host)
check-hal-stub:
    cargo check -p rustyfarian-esp-hal-network --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal ota stub (no-default-features to avoid esp-hal target conflict)
check-ota-hal:
    cargo check -p rustyfarian-esp-hal-network --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal provisioning stub (no-default-features to avoid esp-hal target conflict)
check-provisioning-hal:
    cargo check -p rustyfarian-esp-hal-network --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal provisioning crate cross-compiles cleanly to both bare-metal targets
check-provisioning-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features provisioning,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

# check the esp-hal ota crate with chip + embassy features (ESP32-C6 + ESP32-C3)
check-ota-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c6,unstable,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features ota,esp32c3,unstable,rt,embassy --target-dir {{ hal_dir }}

# run platform-independent HTTP parser unit tests (host toolchain, no ESP toolchain needed)
test-ota-hal:
    cargo test --target {{ host_target }} -p rustyfarian-esp-hal-network --no-default-features

# run platform-independent provisioning unit tests for the bare-metal crate (host toolchain, no ESP toolchain needed)
test-provisioning-hal:
    cargo test --target {{ host_target }} -p rustyfarian-esp-hal-network --no-default-features

# check the esp-hal lora stub (no-default-features to avoid esp-hal target conflict)
check-lora-hal:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features lora,esp32c6,rt --target-dir {{ hal_dir }}

# check the esp-hal wifi stub (no-default-features to avoid esp-hal target conflict)
check-wifi-hal:
    cargo check -p rustyfarian-esp-hal-network --no-default-features --target-dir {{ hal_dir }}

# check the esp-hal wifi crate with the opt-in `embassy` feature (ESP32-C6 + ESP32-C3)
# `-Zbuild-std=core,alloc` overrides the workspace [unstable] build-std default.
check-wifi-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c6,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target riscv32imc-unknown-none-elf -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c3,rt,embassy --target-dir {{ hal_dir }}

# check all HAL domains of the consolidated network crate
check-hal: check-wifi-hal-embassy check-lora-hal check-ota-hal-embassy check-provisioning-hal-embassy

# check juggler compiles without the `std` feature (ADR 014 §2 no_std surface)
check-network-pure-no-std:
    cargo check -p juggler --no-default-features

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

# audit dependencies for known security advisories (RUSTSEC)
audit:
    cargo audit

# update dependencies (pass package flags to update specific crates, e.g. just update -p led-effects)
update *args:
    cargo update {{ args }}

# run platform-independent backoff unit tests (host toolchain, no ESP-IDF needed)
test-backoff:
    cargo test --target {{ host_target }} -p juggler backoff

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
test-mqtt:
    cargo test --target {{ host_target }} -p juggler --features std mqtt

# run subscriber-thread deadlock regression tests (host toolchain, no ESP-IDF needed)
test-subscriber-thread:
    cargo test --target {{ host_target }} -p juggler --features std subscriber_thread

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
test-wifi:
    cargo test --target {{ host_target }} -p juggler --features mock

# run platform-independent LoRa unit tests (host toolchain, no ESP-IDF needed)
test-lora:
    cargo test --target {{ host_target }} -p juggler --features lora,mock

# run platform-independent ESP-NOW unit tests (host toolchain, no ESP-IDF needed)
test-espnow:
    cargo test --target {{ host_target }} -p juggler --features mock

# run platform-independent OTA unit tests (host toolchain, no ESP-IDF needed)
test-ota:
    cargo test --target {{ host_target }} -p juggler --features ota

# run platform-independent provisioning unit tests (host toolchain, no ESP-IDF needed)
test-provisioning:
    cargo test --target {{ host_target }} -p juggler --features provisioning

# run all substrate unit tests (DHCP codec + allocation policy, DNS
# catch-all codec, HTTP parser + routing + minimal-500 fallback) on the host
# toolchain.  The substrate modules live in `rustyfarian-esp-hal-network/src/provisioning/`
# after Phase 3 consolidation; no chip feature is needed for host tests since
# each module's async `run()` is compiled away without a chip feature.
test-provisioning-substrate:
    cargo test --target {{ host_target }} -p rustyfarian-esp-hal-network --no-default-features

# Back-compat aliases for muscle-memory: test-dhcp / test-http / test-dns all run
# test-provisioning-substrate (cargo cannot isolate a crate's per-module tests
# without a per-test filter, so the whole substrate suite runs in each case).

# run DHCP substrate unit tests (alias for test-provisioning-substrate)
test-dhcp: test-provisioning-substrate
# run HTTP substrate unit tests (alias for test-provisioning-substrate)
test-http: test-provisioning-substrate
# run DNS substrate unit tests (alias for test-provisioning-substrate)
test-dns: test-provisioning-substrate

# run all platform-independent unit tests (host toolchain, no ESP-IDF needed)
test: test-backoff test-mqtt test-subscriber-thread test-wifi test-lora test-espnow test-ota test-ota-hal test-provisioning test-provisioning-substrate test-provisioning-hal

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
    scripts/build-example.sh "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# serial port for espflash; honoured verbatim if set, otherwise scripts/detect-port.sh
# narrows espflash auto-detect to USB serial devices (usbmodem/usbserial on macOS,
# ttyUSB/ttyACM on Linux) so paired Bluetooth ports do not get picked.
export ESPFLASH_PORT := env("ESPFLASH_PORT", "")

# ensure the IDF-built v5.3.3 bootloader is in the build cache for the given chip
ensure-bootloader chip:
    scripts/ensure-bootloader.sh "{{ chip }}" "{{ hal_dir }}" "{{ idf_dir }}"

# build and flash a named example; chip and crate auto-detected from example name
flash example:
    scripts/flash.sh "{{ example }}" "{{ hal_dir }}" "{{ idf_dir }}"

# build, flash, and open the serial monitor (run without args to list examples)
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
fresh-run example:
    just clean
    just erase-flash
    just run {{ example }}

# erase entire flash (NVS, app, bootloader) — fixes stale WiFi credentials and corrupt state
[confirm("Erase the ENTIRE flash (NVS, app, bootloader) on the connected device? [y/N]")]
erase-flash:
    scripts/espflash.sh erase-flash

# open the serial monitor for an already-flashed device
monitor:
    scripts/espflash.sh monitor --non-interactive

# ── Maintenance ───────────────────────────────────────────────────────────

# clean build artifacts (target/ide, hal and idf target dirs)
clean:
    cargo clean --target-dir target/ide
    cargo clean --target-dir {{ hal_dir }}
    cargo clean --target-dir {{ idf_dir }}

# clean only the ESP-IDF crate's build artifacts (needed after sdkconfig changes or chip switch)
clean-idf:
    cargo clean -p rustyfarian-esp-idf-network --target-dir {{ idf_dir }}
    rm -rf {{ idf_dir }}/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/
    rm -rf {{ idf_dir }}/riscv32imc-esp-espidf/release/build/esp-idf-sys-*/

# check that provisioning library logs never interpolate credential field names
check-no-credential-logging:
    #!/usr/bin/env bash
    # Grep for log macro calls where credential names appear as actual macro arguments
    # (not in comments or test assertions about containing these names).
    # We exclude lines that are comments or assertions about NOT containing.
    exit_code=0
    grep -rn 'log::\(debug\|info\|warn\|error\)!' \
      crates/rustyfarian-esp-hal-network/src/provisioning/ \
      | grep -E '\(wifi_pass\|mqtt_pass\|body_str\|body_in_buf\)' \
      | grep -v '//' \
      | grep -v '!.*".*"' && exit_code=1 || true
    exit $exit_code

# check that provisioning library never calls reset/reboot or erase_all
check-library-never-reboots:
    #!/usr/bin/env bash
    exit_code=0
    if find crates/rustyfarian-esp-hal-network/src/provisioning/ -name '*.rs' -exec \
      grep -Hn 'esp_hal::reset\|software_reset\|esp_hal_reset' {} \; > /tmp/resets.txt 2>&1; then
      if [ -s /tmp/resets.txt ]; then
        cat /tmp/resets.txt
        echo "ERROR: Found reset/reboot calls in library source"
        exit_code=1
      fi
    fi
    if grep -Hn 'erase_all' crates/rustyfarian-esp-hal-network/src/provisioning/portal.rs > /tmp/erase.txt 2>&1; then
      if [ -s /tmp/erase.txt ]; then
        cat /tmp/erase.txt
        echo "ERROR: Found erase_all call in portal.rs"
        exit_code=1
      fi
    fi
    exit $exit_code

# ── CI ────────────────────────────────────────────────────────────────────

# full pre-commit verification: format, check, lint (local use only — modifies files)
pre-commit: fmt check clippy

# non-modifying full verification: fails on any anomaly; suggests fix recipe on failure
verify:
    just fmt-check || (echo; echo "Formatting issues found — run 'just pre-commit' to auto-fix."; echo; exit 1)
    just ci
    just check-idf
    just check-hal
    just check-no-credential-logging
    just check-library-never-reboots

# CI-equivalent verification (non-modifying): format check, deny, check, lint
ci: fmt-check deny check clippy

# ── Local CI via act ──────────────────────────────────────────────────────

# run all CI workflows locally via act (requires Docker + act)
act *job:
    #!/usr/bin/env bash
    if [ -z "{{ job }}" ]; then
        just act fmt && just act clippy && just act ci && just act audit
    else
        act -j "{{ job }}"
    fi

# ── Publishing ────────────────────────────────────────────────────────────

# pre-flight release validation: version lockstep, verify, package contents, and
# `cargo publish --dry-run` for all three crates (no actual publish)
# See release-plan.md for the full publication sequence and post-publication steps
[group('Release')]
release-publish-validate:
    scripts/release-validate.sh

# pre-publish packaging validation (needs a clean tree): juggler gets a full
# `cargo publish --dry-run` (host-buildable); the two -network crates get
# `cargo package --list` because their `cargo publish --dry-run` resolves
# `juggler ^0.4` against the crates.io index, which only succeeds AFTER juggler is
# published — their real dry-run therefore happens as the ordered publish proceeds.
[group('Release')]
release-dry-run:
    cargo publish --dry-run -p juggler --target {{ host_target }} --all-features
    cargo package --list -p rustyfarian-esp-idf-network > /dev/null
    cargo package --list -p rustyfarian-esp-hal-network > /dev/null

# verify IDF network crate packages cleanly against IDF target (no upload; requires espup)
# NOTE: only succeeds AFTER juggler is published to crates.io (resolves juggler ^0.4 from index)
[group('Release')]
release-dry-run-idf:
    cargo +esp publish --dry-run -p rustyfarian-esp-idf-network --target {{ idf_target }} --target-dir {{ idf_dir }}

# verify HAL network crate packages cleanly against bare-metal target (no upload)
# NOTE: only succeeds AFTER juggler is published to crates.io (resolves juggler ^0.4 from index)
[group('Release')]
release-dry-run-hal:
    cargo publish --dry-run -p rustyfarian-esp-hal-network -Zbuild-std=core,alloc --target {{ hal_target }} --target-dir {{ hal_dir }}

# publish pure crate (juggler) to crates.io
[group('Release')]
[confirm]
release-publish crate:
    cargo publish -p {{ crate }} --target {{ host_target }}

# publish IDF network crate to crates.io (requires espup)
[group('Release')]
[confirm]
release-publish-idf:
    cargo +esp publish -p rustyfarian-esp-idf-network --target {{ idf_target }} --target-dir {{ idf_dir }}

# publish HAL network crate to crates.io
[group('Release')]
[confirm]
release-publish-hal:
    cargo publish -p rustyfarian-esp-hal-network -Zbuild-std=core,alloc --target {{ hal_target }} --target-dir {{ hal_dir }}

# ── Setup ─────────────────────────────────────────────────────────────────

# set up local cargo config from the template
setup-cargo-config:
    cp .cargo/config.toml.dist .cargo/config.toml

# install the ESP-IDF toolchain via espup
setup-toolchain:
    espup install
