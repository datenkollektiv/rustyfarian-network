#!/usr/bin/env bash
set -euo pipefail
# build-example.sh — build a named example (no flash)
# Usage: scripts/build-example.sh <example>
#   example: idf_{chip}_{feature} or hal_{chip}_{name}
#   e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -lt 1 ]; then
    printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
    exit 2
fi

example="$1"

# Extract prefix to determine HAL type
prefix=$(printf '%s' "$example" | cut -d_ -f1)

case "$prefix" in
    idf)
        # ESP-IDF HAL examples: detect package from feature name
        case "$example" in
            *mqtt*)    pkg="rustyfarian-esp-idf-mqtt" ;;
            *connect*) pkg="rustyfarian-esp-idf-wifi" ;;
            *join*|*lora*) pkg="rustyfarian-esp-idf-lora" ;;
            *) printf 'Cannot detect crate for example "%s".\nName must contain "mqtt", "connect", "join", or "lora".\n' "$example" >&2; exit 1 ;;
        esac

        # Detect chip and set MCU / Cargo target
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)      mcu="esp32c3";  target="riscv32imc-esp-espidf"   ;;
            c6)      mcu="esp32c6";  target="riscv32imac-esp-espidf"  ;;
            esp32)   mcu="esp32";    target="xtensa-esp32-espidf"     ;;
            esp32s3) mcu="esp32s3";  target="xtensa-esp32s3-espidf"   ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow idf_{c3|c6|esp32|esp32s3}_{feature}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        printf 'Building %s for %s (MCU=%s)...\n' "$example" "$target" "$mcu"

        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            MCU="$mcu" cargo +esp build --release --target "$target" --example "$example" -p "$pkg"
        else
            MCU="$mcu" cargo build --release --target "$target" --example "$example" -p "$pkg"
        fi
        ;;

    hal)
        # Bare-metal HAL examples: detect package from feature name
        case "$example" in
            *join*) pkg="rustyfarian-esp-hal-lora" ;;
            *connect*) pkg="rustyfarian-esp-hal-wifi" ;;
            *) printf 'Cannot detect crate for example "%s".\nName must contain "join" or "connect".\n' "$example" >&2; exit 1 ;;
        esac

        # Detect chip and set Cargo target
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)     target="riscv32imc-unknown-none-elf";  mcu="esp32c3"  ;;
            c6)     target="riscv32imac-unknown-none-elf"; mcu="esp32c6"  ;;
            esp32)  target="xtensa-esp32-none-elf";        mcu="esp32"    ;;
            esp32s3) target="xtensa-esp32s3-none-elf";     mcu="esp32s3"  ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow hal_{c3|c6|esp32|esp32s3}_{name}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        # Base features
        hal_features="${mcu},unstable,rt"

        printf 'Building %s for bare-metal %s (MCU=%s)...\n' "$example" "$target" "$mcu"

        # Build commands differ by chip: esp32 requires Xtensa toolchain
        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # Source xtensa toolchain for esp32
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            cargo +esp build --release -Zbuild-std=core,alloc --target "$target" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        else
            # RISC-V bare-metal: override the workspace [unstable] build-std (which
            # defaults to "std") with core + alloc only.  alloc is needed by crates
            # like esp-radio that use alloc::string::String.
            cargo build --release -Zbuild-std=core,alloc --target "$target" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        fi
        ;;

    *)
        printf 'Error: example name must start with "idf_" or "hal_".\n' >&2
        printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
        exit 1
        ;;
esac
