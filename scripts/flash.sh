#!/usr/bin/env bash
set -euo pipefail
# flash.sh — build and flash a named example
# Usage: scripts/flash.sh <example> [hal_dir [idf_dir]]
#   example: idf_{chip}_{feature} or hal_{chip}_{name}
#   e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_connect_async, hal_esp32s3_join
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.
# Required features are read from the example's required-features in Cargo.toml.
# The IDF-built bootloader of the pinned ESP-IDF is used instead of the espflash-bundled
# one, which rejects bare-metal binaries and rejected the v5.3.3 IDF binaries
# (32 KB MMU page mismatch, the "efuse blk rev" boot failure).
# Examples whose features include `ota` get the rollback-enabled bootloader
# (<idf_dir>/bootloader/<chip>-ota.bin, see scripts/ensure-bootloader.sh --ota).
#
# FLASH_DRY_RUN=1 builds and resolves the bootloader as usual but prints the espflash
# command instead of running it.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

# run_espflash <espflash args...>
# Runs espflash, or with FLASH_DRY_RUN=1 prints the quoted command and returns.
run_espflash() {
    if [ "${FLASH_DRY_RUN:-0}" = "1" ]; then
        printf 'DRY RUN: espflash'
        printf ' %q' "$@"
        printf '\n'
        return 0
    fi
    espflash "$@"
}

if [ $# -lt 1 ]; then
    printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_connect_async, hal_esp32s3_join\n' "$0" >&2
    exit 2
fi

example="$1"
hal_dir="${2:-target/hal}"
idf_dir="${3:-target/idf}"

# Pick the USB serial port: $ESPFLASH_PORT wins; otherwise prefer the unique
# usbmodem/usbserial (macOS) or ttyUSB/ttyACM (Linux) device — espflash's own
# auto-detect picks Bluetooth devices on Macs with headphones paired and fails
# with the generic "Error while connecting to device".  With several attached,
# the chip from the example name selects the board.  See scripts/detect-port.sh.
case "$(printf '%s' "$example" | cut -d_ -f2)" in
    c3) port_chip=esp32c3 ;;
    c6) port_chip=esp32c6 ;;
    esp32) port_chip=esp32 ;;
    esp32s3) port_chip=esp32s3 ;;
    *) port_chip="" ;;
esac
port_args=()
detected_port="$("$SCRIPT_DIR/detect-port.sh" "$port_chip")"
if [ -n "$detected_port" ]; then
    port_args=(--port "$detected_port")
fi

# Extract prefix to determine HAL type
prefix=$(printf '%s' "$example" | cut -d_ -f1)

case "$prefix" in
    idf)
        # All ESP-IDF examples live in the consolidated rustyfarian-esp-idf-network crate.
        # Read required-features from Cargo.toml [[example]] block.
        pkg="rustyfarian-esp-idf-network"
        pkg_dir="crates/$pkg"

        idf_features=$(get_example_features_from_toml "$example" "$pkg_dir") || exit 1

        # Chip and bootloader variant: an example with the ota feature needs the rollback-enabled bootloader.
        chip=$(printf '%s' "$example" | cut -d_ -f2)
        bl_variant=generic
        bl_flag=""
        if printf '%s' "$idf_features" | grep -q "ota"; then
            bl_variant=ota
            bl_flag="--ota"
        fi

        # If this example requires OTA, apply the OTA sdkconfig overlay (rollback + custom partition table).
        if printf '%s' "$idf_features" | grep -q "ota"; then
            workspace_root="$(cd "$SCRIPT_DIR/.." && pwd)"
            if apply_ota_sdkconfig "$workspace_root" "$idf_dir"; then
                # espflash must write the same two-slot table and erase otadata, so a re-flash never
                # boots a stale slot left over from an earlier OTA test.
                port_args+=(--partition-table "$workspace_root/partitions.ota.csv" --flash-size 4mb --erase-parts otadata)
            fi
        fi

        # Populate the stable bootloader cache for this chip and variant (no-op once cached).
        # Runs before the example build: ensure-bootloader.sh cleans esp-idf-sys afterwards.
        "$SCRIPT_DIR/ensure-bootloader.sh" ${bl_flag:+"$bl_flag"} "$chip" "$idf_dir"

        # Detect chip and set MCU / Cargo target
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

        bl=$(bootloader_cache_path "$idf_dir" "$chip" "$bl_variant")
        if [ -f "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            run_espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "$idf_dir/$target/release/examples/$example"
        else
            printf 'Warning: IDF-built bootloader not cached, using espflash default (may fail on boot).\n' >&2
            run_espflash flash "${port_args[@]}" --ignore-app-descriptor "$idf_dir/$target/release/examples/$example"
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

        # An OTA example needs the same two-slot table as the IDF OTA examples, a blank
        # otadata so a re-flash never boots a slot left over from an earlier OTA run, and the
        # rollback-enabled bootloader so an image that never reaches mark_valid is rolled back.
        bl_variant=generic
        bl_flag=""
        if printf '%s' "$hal_features" | grep -q "ota"; then
            bl_variant=ota
            bl_flag="--ota"
            workspace_root="$(cd "$SCRIPT_DIR/.." && pwd)"
            port_args+=(--partition-table "$workspace_root/partitions.ota.csv" --flash-size 4mb --erase-parts otadata)
        fi

        # Ensure the stable IDF-built bootloader for this chip and variant is cached
        "$SCRIPT_DIR/ensure-bootloader.sh" ${bl_flag:+"$bl_flag"} "$chip" "$idf_dir"

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

        bl=$(bootloader_cache_path "$idf_dir" "$chip" "$bl_variant")
        if [ -f "$bl" ]; then
            printf 'Flashing %s with bootloader %s...\n' "$example" "$bl"
            run_espflash flash "${port_args[@]}" --bootloader "$bl" --ignore-app-descriptor "$hal_dir/$hal_target/release/examples/$example"
        else
            printf 'Warning: no IDF-built bootloader cached for %s; using espflash default (may fail on boot for some chips).\n' "$chip" >&2
            run_espflash flash "${port_args[@]}" --ignore-app-descriptor "$hal_dir/$hal_target/release/examples/$example"
        fi
        ;;

    *)
        printf 'Error: example name must start with "idf_" or "hal_".\n' >&2
        printf 'Usage: %s <example>\n  example: idf_{chip}_{feature} or hal_{chip}_{name}\n  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt, hal_c3_connect_async, hal_esp32s3_join\n' "$0" >&2
        exit 1
        ;;
esac
