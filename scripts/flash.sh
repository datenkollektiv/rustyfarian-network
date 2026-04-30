#!/usr/bin/env bash
set -euo pipefail
# flash.sh — build and flash a named example
# Usage: scripts/flash.sh <example>
#   example: idf_{chip}_{feature} or hal_{chip}_{name}
#   e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.
# The IDF-built v5.3.3 bootloader is used instead of the espflash-bundled one:
# espflash 4.x bundles ESP-IDF v5.5.1, which is incompatible with v5.3.3 binaries
# (32 KB MMU page mismatch) and produces the "efuse blk rev" boot failure.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

# Extract prefix to determine HAL type
prefix=$(printf '%s' "$example" | cut -d_ -f1)

case "$prefix" in
    idf)
        # ESP-IDF HAL examples: detect package from feature name
        case "$example" in
            *mqtt*)    pkg="rustyfarian-esp-idf-mqtt" ;;
            *connect*|*wifi*) pkg="rustyfarian-esp-idf-wifi" ;;
            *join*|*lora*) pkg="rustyfarian-esp-idf-lora" ;;
            *espnow*)  pkg="rustyfarian-esp-idf-espnow" ;;
            *) printf 'Cannot detect crate for example "%s".\nName must contain "mqtt", "connect", "wifi", "join", "lora", or "espnow".\n' "$example" >&2; exit 1 ;;
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

        # Use the IDF-built bootloader; espflash 4.x bundles v5.5.1 which is incompatible
        # with v5.3.3 binaries.
        bl_candidates=( "$PWD/target/$target/release/build"/esp-idf-sys-*/out/build/bootloader/bootloader.bin )
        bl=""
        if [ ${#bl_candidates[@]} -gt 0 ] && [ -e "${bl_candidates[0]}" ]; then
            if [ ${#bl_candidates[@]} -gt 1 ]; then
                printf 'Error: multiple IDF-built bootloaders found for target "%s".\n' "$target" >&2
                printf 'Please clean old builds or remove unused esp-idf-sys-* build directories.\nCandidates:\n' >&2
                for cand in "${bl_candidates[@]}"; do
                    printf '  %s\n' "$cand" >&2
                done
                exit 1
            fi
            bl="${bl_candidates[0]}"
        fi
        if [ -n "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "target/$target/release/examples/$example"
        else
            printf 'Warning: IDF-built bootloader not found, using espflash default (may fail on boot).\n' >&2
            espflash flash "${port_args[@]}" --ignore-app-descriptor "target/$target/release/examples/$example"
        fi
        ;;

    hal)
        # Bare-metal HAL examples: detect package from feature name
        case "$example" in
            *join*) pkg="rustyfarian-esp-hal-lora" ;;
            *connect*|*wifi*) pkg="rustyfarian-esp-hal-wifi" ;;
            *) printf 'Cannot detect crate for example "%s".\nName must contain "join", "connect", or "wifi".\n' "$example" >&2; exit 1 ;;
        esac

        # Detect chip and set targets
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        case "$chip" in
            c3)      hal_target="riscv32imc-unknown-none-elf";  idf_target="riscv32imc-esp-espidf";  mcu="esp32c3"  ;;
            c6)      hal_target="riscv32imac-unknown-none-elf"; idf_target="riscv32imac-esp-espidf"; mcu="esp32c6"  ;;
            esp32)   hal_target="xtensa-esp32-none-elf";        idf_target="xtensa-esp32-espidf";    mcu="esp32"    ;;
            esp32s3) hal_target="xtensa-esp32s3-none-elf";      idf_target="xtensa-esp32s3-espidf";  mcu="esp32s3"  ;;
            *) printf 'Unknown chip "%s" in example "%s". Name must follow hal_{c3|c6|esp32|esp32s3}_{name}.\n' "$chip" "$example" >&2; exit 1 ;;
        esac

        # Base features
        hal_features="${mcu},unstable,rt"

        # Append optional features based on example name
        case "$example" in
            *_rgb*|hal_c6_*_led*) hal_features="${hal_features},rustyfarian-esp-hal-ws2812" ;;
        esac
        case "$example" in
            *_async*) hal_features="${hal_features},embassy" ;;
        esac

        # Ensure IDF-built bootloader is cached
        "$SCRIPT_DIR/ensure-bootloader.sh" "$chip"

        printf 'Building %s for bare-metal %s (MCU=%s)...\n' "$example" "$hal_target" "$mcu"

        # Build command differs by chip: esp32 requires Xtensa toolchain
        if [ "$mcu" = "esp32" ] || [ "$mcu" = "esp32s3" ]; then
            # Source xtensa toolchain for esp32
            # shellcheck source=/dev/null
            source "$SCRIPT_DIR/xtensa-toolchain.sh"
            setup_xtensa_toolchain
            cargo +esp build --release -Zbuild-std=core,alloc --target "$hal_target" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        else
            cargo build --release -Zbuild-std=core,alloc --target "$hal_target" --no-default-features --features "$hal_features" --example "$example" -p "$pkg"
        fi

        # Get bootloader from IDF target's release cache; HAL targets don't produce bootloaders
        bl_candidates=( "$PWD/target/$idf_target/release/build"/esp-idf-sys-*/out/build/bootloader/bootloader.bin )
        bl=""
        if [ ${#bl_candidates[@]} -gt 0 ] && [ -e "${bl_candidates[0]}" ]; then
            if [ ${#bl_candidates[@]} -gt 1 ]; then
                printf 'Error: multiple IDF-built bootloaders found for target "%s".\n' "$idf_target" >&2
                printf 'Please clean old builds or remove unused esp-idf-sys-* build directories.\nCandidates:\n' >&2
                for cand in "${bl_candidates[@]}"; do
                    printf '  %s\n' "$cand" >&2
                done
                exit 1
            fi
            bl="${bl_candidates[0]}"
        fi

        if [ -n "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "target/$hal_target/release/examples/$example"
        else
            printf 'Warning: no IDF-built bootloader cached for %s; using espflash default (may fail on boot for some chips).\n' "$chip" >&2
            espflash flash "${port_args[@]}" --ignore-app-descriptor "target/$hal_target/release/examples/$example"
        fi
        ;;

    *)
        printf 'Error: example name must start with "idf_" or "hal_".\n' >&2
        printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_join, hal_esp32_join\n' "$0" >&2
        exit 1
        ;;
esac
