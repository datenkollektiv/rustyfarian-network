# Rustyfarian Network — development tasks
#
# ESP-IDF crates require the ESP-IDF toolchain (`just setup-toolchain`).
# Pure crates (rustyfarian-network-pure, wifi-pure, lora-pure) compile and
# test on any host without the ESP toolchain.
# Run `just setup-cargo-config` to create the local cargo config.

# list available recipes (default)
_default:
    @just --list

# build the entire workspace (release)
build:
    cargo build --release

# check the entire workspace
check:
    cargo check

# check the wifi crate
check-wifi:
    cargo check -p rustyfarian-esp-idf-wifi

# check the mqtt crate
check-mqtt:
    cargo check -p rustyfarian-esp-idf-mqtt

# check the esp-idf lora crate
check-lora:
    cargo check -p rustyfarian-esp-idf-lora

# check the pure lora crate (no ESP-IDF required)
check-lora-pure:
    cargo check -p lora-pure

# check the pure wifi crate (no ESP-IDF required)
check-wifi-pure:
    cargo check -p wifi-pure

# check the pure espnow crate (no ESP-IDF required)
check-espnow-pure:
    cargo check -p espnow-pure

# check the esp-idf espnow crate
check-espnow:
    cargo check -p rustyfarian-esp-idf-espnow

# check the esp-hal lora stub (no-default-features to avoid esp-hal target conflict)
check-lora-hal:
    cargo check -p rustyfarian-esp-hal-lora --no-default-features

# check the esp-hal wifi stub (no-default-features to avoid esp-hal target conflict)
check-wifi-hal:
    cargo check -p rustyfarian-esp-hal-wifi --no-default-features

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

# check dependency licenses, advisories, and bans
deny:
    cargo deny check

# update dependencies (pass package flags to update specific crates, e.g. just update -p led-effects)
update *args:
    cargo update {{args}}

# clean build artifacts
clean:
    cargo clean

# host target, used to override the workspace ESP-IDF target for pure-logic tests
host_target := `scripts/host-target.sh`

# platform-independent crates that can be compiled and tested on the host
pure_crates := "-p rustyfarian-network-pure -p wifi-pure -p espnow-pure"

# run platform-independent backoff unit tests (host toolchain, no ESP-IDF needed)
test-backoff:
    cargo test --target {{host_target}} -p rustyfarian-network-pure backoff

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
test-mqtt:
    cargo test --target {{host_target}} {{pure_crates}} mqtt

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
test-wifi:
    cargo test --target {{host_target}} -p wifi-pure --features mock

# run platform-independent LoRa unit tests (host toolchain, no ESP-IDF needed)
test-lora:
    cargo test --target {{host_target}} -p lora-pure --features mock

# run platform-independent ESP-NOW unit tests (host toolchain, no ESP-IDF needed)
test-espnow:
    cargo test --target {{host_target}} -p espnow-pure --features mock

# run all platform-independent unit tests using {{pure_crates}} (host toolchain, no ESP-IDF needed)
test: test-backoff test-mqtt test-wifi test-lora test-espnow

# ── Examples ────────────────────────────────────────────────────────────────

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
    scripts/build-example.sh "{{example}}"

# build and flash a named example; chip and crate auto-detected from example name
flash example:
    scripts/flash.sh "{{example}}"

# build, flash, and open the serial monitor (run without args to list examples)
run *example:
    #!/usr/bin/env bash
    if [ -z "{{example}}" ]; then
        just examples
    else
        just flash "{{example}}"
        espflash monitor
    fi

# open the serial monitor for an already-flashed device
monitor:
    espflash monitor

# clean only the ESP-IDF crate's build artifacts (needed after sdkconfig changes or chip switch)
clean-idf:
    cargo clean -p rustyfarian-esp-idf-wifi
    cargo clean -p rustyfarian-esp-idf-mqtt
    rm -rf target/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/
    rm -rf target/riscv32imc-esp-espidf/release/build/esp-idf-sys-*/

# full pre-commit verification: format, check, lint (local use only — modifies files)
pre-commit: fmt check clippy

# non-modifying full verification: fails on any anomaly; suggests fix recipe on failure
verify:
    just fmt-check || (echo; echo "Formatting issues found — run 'just pre-commit' to auto-fix."; echo; exit 1)
    just ci

# CI-equivalent verification (non-modifying): format check, deny, check, lint
ci: fmt-check deny check clippy

# ── Local CI via act ───────────────────────────────────────────────────────

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
