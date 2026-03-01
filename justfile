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
host_target := `host=$(rustc -vV 2>/dev/null | grep '^host:' | awk '{print $2}'); if [ -z "$host" ]; then printf 'Error: Failed to determine rustc host target.\n' >&2; exit 1; fi; echo "$host"`

# platform-independent crates that can be compiled and tested on the host
pure_crates := "-p rustyfarian-network-pure"

# run platform-independent MQTT unit tests (host toolchain, no ESP-IDF needed)
test-mqtt:
    cargo test --target {{host_target}} {{pure_crates}} mqtt

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
test-wifi:
    cargo test --target {{host_target}} {{pure_crates}} wifi

# run all platform-independent unit tests (host toolchain, no ESP-IDF needed)
test: test-mqtt test-wifi

# full pre-commit verification: format, check, lint (local use only — modifies files)
pre-commit: fmt check clippy

# non-modifying full verification: fails on any anomaly; suggests fix recipe on failure
verify:
    just fmt-check || (printf "\nFormatting issues found — run 'just pre-commit' to auto-fix.\n\n"; exit 1)
    cargo deny check
    cargo check
    just clippy

# CI-equivalent verification (non-modifying): format check, deny, check, lint
ci: fmt-check deny check clippy

# set up local cargo config from the template
setup-cargo-config:
    cp .cargo/config.toml.dist .cargo/config.toml

# install the ESP-IDF toolchain via espup
setup-toolchain:
    espup install
