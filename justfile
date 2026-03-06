# Rustyfarian Network — development tasks
#
# All crates depend on esp-idf-svc and require the ESP-IDF toolchain.
# Run `just setup-toolchain` and `just setup-cargo-config` first.

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

# check the esp-hal lora stub (no-default-features to avoid esp-hal target conflict)
check-lora-hal:
    cargo check -p rustyfarian-esp-hal-lora --no-default-features

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

# update dependencies
update:
    cargo update

# clean build artifacts
clean:
    cargo clean

# host target, used to override the workspace ESP-IDF target for pure-logic tests
host_target := `host=$(rustc -vV 2>/dev/null | grep '^host:' | awk '{print $2}'); if [ -z "$host" ]; then echo 'Error: Failed to determine rustc host target.' >&2; exit 1; fi; echo "$host"`

# platform-independent crates that can be compiled and tested on the host
pure_crates := "-p rustyfarian-network-pure"

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
test-mqtt:
    cargo test --target {{host_target}} {{pure_crates}} mqtt

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
test-wifi:
    cargo test --target {{host_target}} {{pure_crates}} wifi

# run lora unit tests on the host (no ESP-IDF required)
# Tests live in lora-pure; --features mock enables MockLoraRadio and mock-gated test blocks.
test-lora:
    cargo test --target {{host_target}} -p lora-pure --features mock

# run all platform-independent unit tests using {{pure_crates}} (host toolchain, no ESP-IDF needed)
test: test-mqtt test-wifi test-lora

# ── Examples ────────────────────────────────────────────────────────────────

# build a named example; chip and crate auto-detected from example name
build-example example:
    scripts/build-example.sh "{{example}}"

# build and flash a named example; chip and crate auto-detected from example name
flash example:
    scripts/flash.sh "{{example}}"

# build, flash, and open the serial monitor
run example: (flash example)
    espflash monitor

# open the serial monitor for an already-flashed device
monitor:
    espflash monitor

# convenience: build the blocking Wi-Fi connect example
build-wifi-connect:
    just build-example idf_c3_connect

# convenience: build the non-blocking Wi-Fi connect example
build-wifi-connect-nonblocking:
    just build-example idf_c3_connect_nonblocking

# convenience: build the MQTT builder example
build-mqtt:
    just build-example idf_c3_mqtt

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

# set up local cargo config from the template
setup-cargo-config:
    cp .cargo/config.toml.dist .cargo/config.toml

# install the ESP-IDF toolchain via espup
setup-toolchain:
    espup install
