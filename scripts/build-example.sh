#!/usr/bin/env bash
set -euo pipefail
# build-example.sh — build a named example (no flash)
# Usage: scripts/build-example.sh <example> [hal_dir [idf_dir]]
#   example: idf_{chip}_{feature} or hal_{chip}_{name}
#   e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.
# Required features are read from the example's required-features in Cargo.toml.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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
        ;;

    hal)
        # All bare-metal HAL examples live in the consolidated rustyfarian-esp-hal-network crate.
        # Read required-features from Cargo.toml [[example]] block.
        pkg="rustyfarian-esp-hal-network"
        pkg_dir="crates/$pkg"

        hal_features=$(get_example_features_from_toml "$example" "$pkg_dir") || exit 1

        # Detect chip and set Cargo target
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)     target="riscv32imc-unknown-none-elf";  mcu="esp32c3"  ;;
            c6)     target="riscv32imac-unknown-none-elf"; mcu="esp32c6"  ;;
            esp32)  target="xtensa-esp32-none-elf";        mcu="esp32"    ;;
            esp32s3) target="xtensa-esp32s3-none-elf";     mcu="esp32s3"  ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow hal_{c3|c6|esp32|esp32s3}_{name}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        printf 'Building %s for bare-metal %s (MCU=%s)...\n' "$example" "$target" "$mcu"

        # Build commands differ by chip: esp32 requires Xtensa toolchain
        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            cargo +esp build --release -Zbuild-std=core,alloc --target "$target" --target-dir "$hal_dir" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        else
            # RISC-V bare-metal: override the workspace [unstable] build-std (which
            # defaults to "std") with core + alloc only.  alloc is needed by crates
            # like esp-radio that use alloc::string::String.
            cargo build --release -Zbuild-std=core,alloc --target "$target" --target-dir "$hal_dir" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        fi
        ;;

    *)
        printf 'Error: example name must start with "idf_" or "hal_".\n' >&2
        printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
        exit 1
        ;;
esac
