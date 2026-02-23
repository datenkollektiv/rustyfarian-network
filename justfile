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

# full pre-commit verification: format, check, lint
verify: fmt check clippy

# CI-equivalent verification (non-modifying): format check, deny, check, lint
ci: fmt-check deny check clippy

# set up local cargo config from the template
setup-cargo-config:
    cp .cargo/config.toml.dist .cargo/config.toml

# install the ESP-IDF toolchain via espup
setup-toolchain:
    espup install
