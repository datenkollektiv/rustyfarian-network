#!/usr/bin/env bash
set -euo pipefail
# flash.sh — build and flash a named example
# Usage: scripts/flash.sh <example> [hal_dir [idf_dir]]
#   example: idf_{chip}_{feature} or hal_{chip}_{name}
#   e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.
# Required features are read from the example's required-features in Cargo.toml.
# The IDF-built v5.3.3 bootloader is used instead of the espflash-bundled one:
# espflash 4.x bundles ESP-IDF v5.5.1, which is incompatible with v5.3.3 binaries
# (32 KB MMU page mismatch) and produces the "efuse blk rev" boot failure.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

# Get required-features for an example from its Cargo.toml
# Arguments: example_name crate_dir
# Output: comma-separated features (e.g., "wifi,mqtt")
# Exit: 1 if not found
get_example_features_from_toml() {
    local example_name="$1"
    local crate_dir="$2"

    if [ ! -f "$crate_dir/Cargo.toml" ]; then
        printf 'ERROR: %s/Cargo.toml not found\n' "$crate_dir" >&2
        return 1
    fi

    local in_example=0 found_example=0 features=""

    while IFS= read -r line; do
        # Check if entering an [[example]] block
        if [[ "$line" == "[[example]]" ]]; then
            in_example=1
            found_example=0
            features=""
            continue
        fi

        if [ $in_example -eq 1 ]; then
            # Check if this is the example we want
            if [[ "$line" =~ ^name\ =\ \"([^\"]+)\" ]]; then
                if [ "${BASH_REMATCH[1]}" = "$example_name" ]; then
                    found_example=1
                else
                    in_example=0
                fi
            fi

            # Extract required-features if we found the right example
            if [ $found_example -eq 1 ]; then
                if [[ "$line" =~ ^required-features\ =\ \[(.*)\] ]]; then
                    features="${BASH_REMATCH[1]}"
                    break
                fi
            fi

            # Stop if we hit another [[...]] that's not [[example]]
            if [[ "$line" =~ ^\[\[ && ! "$line" =~ ^\[\[example\]\] ]]; then
                in_example=0
            fi
        fi
    done < "$crate_dir/Cargo.toml"

    if [ -z "$features" ]; then
        printf 'ERROR: Example "%s" not found or has no required-features in %s/Cargo.toml\n' \
            "$example_name" "$crate_dir" >&2
        return 1
    fi

    # Convert TOML array format to comma-separated
    # Input: "wifi", "mqtt"  →  Output: wifi,mqtt
    features=$(printf '%s' "$features" | tr -d '"' | tr -d ' ')
    printf '%s' "$features"
}

# Pick the USB serial port: $ESPFLASH_PORT wins; otherwise prefer the unique
# usbmodem/usbserial (macOS) or ttyUSB/ttyACM (Linux) device — espflash's own
# auto-detect picks Bluetooth devices on Macs with headphones paired and fails
# with the generic "Error while connecting to device".  See scripts/detect-port.sh.
port_args=()
detected_port="$("$SCRIPT_DIR/detect-port.sh")"
if [ -n "$detected_port" ]; then
    port_args=(--port "$detected_port")
fi

if [ $# -lt 1 ]; then
    printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
    exit 2
fi

example="$1"
hal_dir="${2:-target/hal}"
idf_dir="${3:-target/idf}"

# Extract prefix to determine HAL type
prefix=$(printf '%s' "$example" | cut -d_ -f1)

case "$prefix" in
    idf)
        # All ESP-IDF examples live in the consolidated rustyfarian-esp-idf-network crate.
        # Read required-features from Cargo.toml [[example]] block.
        pkg="rustyfarian-esp-idf-network"
        pkg_dir="crates/$pkg"

        idf_features=$(get_example_features_from_toml "$example" "$pkg_dir") || exit 1

        # Detect chip and set MCU / Cargo target
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)      mcu="esp32c3";  target="riscv32imc-esp-espidf"   ;;
            c6)      mcu="esp32c6";  target="riscv32imac-esp-espidf"  ;;
            esp32)   mcu="esp32";    target="xtensa-esp32-espidf"     ;;
            esp32s3) mcu="esp32s3";  target="xtensa-esp32s3-espidf"   ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow idf_{c3|c6|esp32|esp32s3}_{feature}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        printf 'Building %s for %s (MCU=%s, features=%s)...\n' "$example" "$target" "$mcu" "$idf_features"

        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            MCU="$mcu" cargo +esp build --release --target "$target" --target-dir "$idf_dir" --features "$idf_features" --example "$example" -p "$pkg"
        else
            MCU="$mcu" cargo build --release --target "$target" --target-dir "$idf_dir" --features "$idf_features" --example "$example" -p "$pkg"
        fi

        bl=$(find_idf_bootloader "$target" "$idf_dir")
        if [ -n "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "$idf_dir/$target/release/examples/$example"
        else
            printf 'Warning: IDF-built bootloader not found, using espflash default (may fail on boot).\n' >&2
            espflash flash "${port_args[@]}" --ignore-app-descriptor "$idf_dir/$target/release/examples/$example"
        fi
        ;;

    hal)
        # All bare-metal HAL examples live in the consolidated rustyfarian-esp-hal-network crate.
        # Read required-features from Cargo.toml [[example]] block.
        pkg="rustyfarian-esp-hal-network"
        pkg_dir="crates/$pkg"

        hal_features=$(get_example_features_from_toml "$example" "$pkg_dir") || exit 1

        # Detect chip and set targets
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)      hal_target="riscv32imc-unknown-none-elf";  idf_target="riscv32imc-esp-espidf";  mcu="esp32c3"  ;;
            c6)      hal_target="riscv32imac-unknown-none-elf"; idf_target="riscv32imac-esp-espidf"; mcu="esp32c6"  ;;
            esp32)   hal_target="xtensa-esp32-none-elf";        idf_target="xtensa-esp32-espidf";    mcu="esp32"    ;;
            esp32s3) hal_target="xtensa-esp32s3-none-elf";      idf_target="xtensa-esp32s3-espidf";  mcu="esp32s3"  ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow hal_{c3|c6|esp32|esp32s3}_{name}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        # Ensure IDF-built bootloader is cached
        "$SCRIPT_DIR/ensure-bootloader.sh" "$chip" "$hal_dir" "$idf_dir"

        printf 'Building %s for bare-metal %s (MCU=%s)...\n' "$example" "$hal_target" "$mcu"

        # Build command differs by chip: esp32 requires Xtensa toolchain
        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            cargo +esp build --release -Zbuild-std=core,alloc --target "$hal_target" --target-dir "$hal_dir" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        else
            cargo build --release -Zbuild-std=core,alloc --target "$hal_target" --target-dir "$hal_dir" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        fi

        bl=$(find_idf_bootloader "$idf_target" "$idf_dir")
        if [ -n "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "$hal_dir/$hal_target/release/examples/$example"
        else
            printf 'Warning: no IDF-built bootloader cached for %s; using espflash default (may fail on boot for some chips).\n' "$chip" >&2
            espflash flash "${port_args[@]}" --ignore-app-descriptor "$hal_dir/$hal_target/release/examples/$example"
        fi
        ;;

    *)
        printf 'Error: example name must start with "idf_" or "hal_".\n' >&2
        printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
        exit 1
        ;;
esac
