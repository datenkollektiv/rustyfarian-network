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

# ESP-IDF target (publish / dry-run)
idf_target := "riscv32imac-esp-espidf"

# serial port for espflash; honoured verbatim if set, otherwise scripts/detect-port.sh
# narrows espflash auto-detect to USB serial devices (usbmodem/usbserial on macOS,
# ttyUSB/ttyACM on Linux) so paired Bluetooth ports do not get picked.
export ESPFLASH_PORT := env("ESPFLASH_PORT", "")

ramdisk := "/Volumes/RustBuilds"
hal_dir := if path_exists(ramdisk + "/targets/hal") == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir := if path_exists(ramdisk + "/targets/idf") == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }

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

# check the esp-idf espnow domain of the consolidated ESP-IDF network crate
[group('build')]
check-espnow:
    cargo check -p rustyfarian-esp-idf-network --features espnow --target-dir {{ idf_dir }}

# Fully cleans esp-idf-sys before AND after: switching sdkconfig variants forces a CMake reconfigure.
# ESP_IDF_SDKCONFIG_DEFAULTS paths MUST be absolute: embuild does not resolve relative overlay
# lists against the workspace root (relative silently applies neither file → false pass).
# check that --features wifi still compiles with SoftAP disabled (STA-only consumers)
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

# alias for check-hal-stub (ota domain)
[group('build')]
check-ota-hal: check-hal-stub

# alias for check-hal-stub (provisioning domain)
[group('build')]
check-provisioning-hal: check-hal-stub

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

# run the bare-metal network crate's host tests (HTTP parser, provisioning substrate, OTA, provisioning)
[group('test & lint')]
test-hal:
    cargo test --target {{ host_target }} -p rustyfarian-esp-hal-network --no-default-features

# alias for test-hal
[group('test & lint')]
test-dhcp: test-hal

# alias for test-hal
[group('test & lint')]
test-http: test-hal

# alias for test-hal
[group('test & lint')]
test-dns: test-hal

# alias for test-hal
[group('test & lint')]
test-ota-hal: test-hal

# alias for test-hal
[group('test & lint')]
test-provisioning-hal: test-hal

# check the esp-hal lora domain cross-compiles for ESP32-C6 (bare-metal)
[group('build')]
check-lora-hal:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features lora,esp32c6,rt --target-dir {{ hal_dir }}

# alias for check-hal-stub (wifi domain)
[group('build')]
check-wifi-hal: check-hal-stub

# check the esp-hal wifi crate with the opt-in `embassy` feature (ESP32-C6 + ESP32-C3)
[group('build')]
check-wifi-hal-embassy:
    cargo check -Zbuild-std=core,alloc --target {{ hal_c6_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c6,rt,embassy --target-dir {{ hal_dir }}
    cargo check -Zbuild-std=core,alloc --target {{ hal_c3_target }} -p rustyfarian-esp-hal-network --no-default-features --features wifi,esp32c3,rt,embassy --target-dir {{ hal_dir }}

# check all HAL domains of the consolidated network crate
[group('build')]
check-hal: check-wifi-hal-embassy check-lora-hal check-ota-hal-embassy check-provisioning-hal-embassy

# check juggler compiles without the `std` feature (ADR 014 §2 no_std surface)
[group('build')]
check-network-pure-no-std:
    cargo check -p juggler --no-default-features

# Lints all four domains (wifi, lora, ota, provisioning) on both RISC-V targets.
# `-Zbuild-std=core,alloc` overrides the workspace [unstable] build-std default.
# run clippy on the esp-hal network crate (bare-metal targets: ESP32-C6 + ESP32-C3)
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

# Known exceptions (build-only `embuild`, the reserved `lorawan-device`) are suppressed via
# `[package.metadata.cargo-machete] ignored` in Cargo.toml with justification comments.
# find unused declared dependencies across the workspace
[group('test & lint')]
machete:
    cargo machete --with-metadata

# update dependencies (pass package flags to update specific crates, e.g. just update -p led-effects)
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

# run subscriber-thread and connect-thread deadlock regression tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-subscriber-thread:
    cargo test --target {{ host_target }} -p juggler --features std -- subscriber_thread connect_thread

# run platform-independent Wi-Fi unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-wifi:
    cargo test --target {{ host_target }} -p juggler --features mock

# run platform-independent LoRa unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-lora:
    cargo test --target {{ host_target }} -p juggler --features lora,mock

# run platform-independent ESP-NOW unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-espnow: test-wifi

# run platform-independent OTA unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-ota:
    cargo test --target {{ host_target }} -p juggler --features ota-wire

# run platform-independent provisioning unit tests (host toolchain, no ESP-IDF needed)
[group('test & lint')]
test-provisioning:
    cargo test --target {{ host_target }} -p juggler --features provisioning,std

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

# ensure the IDF-built v5.3.3 bootloader is in the build cache for the given chip
[group('examples')]
ensure-bootloader chip:
    scripts/ensure-bootloader.sh "{{ chip }}" "{{ hal_dir }}" "{{ idf_dir }}"

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

# ── maintenance ───────────────────────────────────────────────────────────

# clean build artifacts (target/ide, hal and idf target dirs)
[group('maintenance')]
clean:
    cargo clean --target-dir target/ide
    cargo clean --target-dir {{ hal_dir }}
    cargo clean --target-dir {{ idf_dir }}

# clean only the ESP-IDF crate's build artifacts (needed after sdkconfig changes or chip switch)
[group('maintenance')]
clean-idf:
    cargo clean -p rustyfarian-esp-idf-network --target-dir {{ idf_dir }}
    rm -rf {{ idf_dir }}/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/
    rm -rf {{ idf_dir }}/riscv32imc-esp-espidf/release/build/esp-idf-sys-*/

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
ota-cmd version:
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
ota-sub topic="status":
    scripts/ota-mqtt.sh sub "{{ topic }}"

# Usage: just ota-serve              (serve at full speed)
#        just ota-serve 10240        (throttle to 10 KB/s)
#        just ota-serve 0 102400     (full speed, then stall after 100 KiB: download_timeout test)
# Port: OTA_PORT (default 8000), the same variable `ota-image` writes into the URLs.
# serve OTA images from target/ota/ over HTTP with optional throttle
[group('ota')]
ota-serve rate="0" stall="0":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -d target/ota ]; then
        echo "target/ota does not exist; run 'just ota-image <example>' first" >&2
        exit 1
    fi
    exec python3 scripts/ota-server.py target/ota "${OTA_PORT:-8000}" "{{ rate }}" "{{ stall }}"

# Port: LOCAL_MQTT_PORT (default 1883); `up` prints the MQTT_HOST and MQTT_PORT to use.
# manage a local test MQTT broker: just mosquitto up | down | status | restart
[group('ota')]
mosquitto action:
    @scripts/mosquitto.sh "{{ action }}"

# ── ci ────────────────────────────────────────────────────────────────────

# full pre-commit verification: format, check, lint (local use only — modifies files)
[group('ci')]
pre-commit: fmt check clippy

# non-modifying full verification: fails on any anomaly; suggests fix recipe on failure
[group('ci')]
verify:
    just fmt-check || (echo; echo "Formatting issues found — run 'just pre-commit' to auto-fix."; echo; exit 1)
    just ci
    just check-idf
    just clippy-provisioning-tests
    just clippy-ota-pure
    just clippy-ota-mqtt-idf
    just check-hal
    just check-no-credential-logging
    just check-library-never-reboots
    just test

# CI-equivalent verification (non-modifying): format check, deny, check, lint
[group('ci')]
ci: fmt-check deny check clippy check-ota-pure check-ota-wire-pure

# run all CI workflows locally via act (requires Docker + act)
[group('ci')]
act *job:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{ job }}" ]; then
        just act fmt && just act clippy && just act check-and-test && just act audit
    else
        act -j "{{ job }}"
    fi

# ── release ───────────────────────────────────────────────────────────────

# pre-flight release validation: version lockstep, verify, package contents, and `cargo publish --dry-run`
[group('release')]
release-publish-validate:
    scripts/release-validate.sh

# juggler gets a full `cargo publish --dry-run` (host-buildable); the two -network crates get
# `cargo package --list` because their `cargo publish --dry-run` resolves `juggler ^0.5` against
# the crates.io index, which only succeeds AFTER juggler is published — their real dry-run therefore
# happens as the ordered publish proceeds.
# pre-publish packaging validation (needs a clean tree)
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

# initialize the development environment: cargo config, ESP-IDF toolchain, and tooling check
[group('setup')]
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f .cargo/config.toml ]; then
        echo "Copying .cargo/config.toml.dist to .cargo/config.toml..."
        cp .cargo/config.toml.dist .cargo/config.toml
    else
        echo ".cargo/config.toml already exists"
    fi
    echo "Installing ESP-IDF toolchain..."
    espup install
    echo "Checking development tooling..."
    just doctor

# set up local cargo config from the template (or print message if it exists)
[group('setup')]
setup-cargo-config:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f .cargo/config.toml ]; then
        echo ".cargo/config.toml already exists; delete it first to re-create from template"
        exit 0
    fi
    cp .cargo/config.toml.dist .cargo/config.toml
    echo "Created .cargo/config.toml from template"

# install the ESP-IDF toolchain via espup
[group('setup')]
setup-toolchain:
    espup install
