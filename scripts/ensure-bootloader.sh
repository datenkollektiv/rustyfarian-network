#!/usr/bin/env bash
set -euo pipefail
# ensure-bootloader.sh — ensure the IDF-built v5.3.3 bootloader is cached for a chip
# Usage: scripts/ensure-bootloader.sh [--ota] <chip> [idf_dir]   chip: c3 | c6 | esp32 | esp32s3
#
# Writes one stable file per chip and variant:
#   <idf_dir>/bootloader/<chip>.bin      generic (IDF defaults, rollback off)
#   <idf_dir>/bootloader/<chip>-ota.bin  --ota (CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y, OTA overlay)
# plus a <chip>[-ota].meta sidecar naming the esp-idf-sys build the file was harvested from.
# flash.sh reads these files and never globs the build tree.
#
# Each populate runs: clean esp-idf-sys -> build one representative example (with the OTA overlay
# for --ota) -> harvest the single resulting bootloader -> verify its out/sdkconfig -> clean again.
# Cleaning before and after keeps the two variants from sharing one esp-idf-sys build, so the
# generic bootloader cannot inherit the rollback setting of an OTA build.
#
# espflash 4.x bundles an ESP-IDF v5.5.1 bootloader that rejects both v5.3.3 IDF
# binaries (32 KB MMU page mismatch) and bare-metal esp-hal binaries (app descriptor
# format). The v5.3.3 bootloader built by esp-idf-sys works for both.
#
# c6 and esp32 build idf_c6_mqtt and idf_esp32_mqtt. A missing RISC-V bootloader (c3, c6) is
# non-fatal for the generic variant: flash.sh then falls back to the espflash bundled one.
# A missing Xtensa bootloader and any missing --ota bootloader are fatal, because the
# bundled bootloader is not a safe fallback there (and not rollback-enabled).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

usage() {
    printf 'Usage: %s [--ota] <chip> [idf_dir]\n  chip: c3 | c6 | esp32 | esp32s3\n' "$0" >&2
}

# clean_idf_sys
# Removes every esp-idf-sys build for the chip's target. cargo clean -p only drops cargo's
# fingerprints: CMake would reuse the old out/ tree, reconfigure sdkconfig and skip the
# bootloader rebuild, so the harvested bootloader could be stale. Deleting the build dirs
# (as `just clean-idf` does) forces a full ESP-IDF build.
clean_idf_sys() {
    cargo clean -p esp-idf-sys --target-dir "$idf_dir_abs"
    local profile
    for profile in release debug; do
        rm -rf "$idf_dir_abs/$idf_target/$profile/build"/esp-idf-sys-*
    done
}

variant="generic"
if [ "${1:-}" = "--ota" ]; then
    variant="ota"
    shift
fi

if [ $# -lt 1 ]; then
    usage
    exit 2
fi

chip="$1"
idf_dir="${2:-target/idf}"
workspace_root="$(cd "$SCRIPT_DIR/.." && pwd)"
idf_dir_abs="$(abs_dir "$idf_dir")"
pkg="rustyfarian-esp-idf-network"

# Map chip to IDF target, MCU, and the representative example (the OTA example for --ota)
case "$chip" in
    c3)
        idf_target="riscv32imc-esp-espidf"
        mcu="esp32c3"
        if [ "$variant" = "ota" ]; then example="idf_c3_ota_runtime"; else example="idf_c3_connect"; fi
        ;;
    c6)
        idf_target="riscv32imac-esp-espidf"
        mcu="esp32c6"
        example="idf_c6_mqtt"
        ;;
    esp32)
        idf_target="xtensa-esp32-espidf"
        mcu="esp32"
        example="idf_esp32_mqtt"
        ;;
    esp32s3)
        idf_target="xtensa-esp32s3-espidf"
        mcu="esp32s3"
        if [ "$variant" = "ota" ]; then example="idf_esp32s3_mqtt_lwt"; else example="idf_esp32s3_join"; fi
        ;;
    *)
        printf 'Error: Unknown chip "%s". Supported: c3, c6, esp32, esp32s3\n' "$chip" >&2
        exit 1
        ;;
esac

if [ "$variant" = "ota" ]; then
    case "$chip" in
        c3|esp32s3) ;;
        *)
            printf 'Error: no --ota bootloader variant for %s (OTA is validated on c3 and esp32s3 only).\n' "$chip" >&2
            exit 1
            ;;
    esac
fi

cache="$(bootloader_cache_path "$idf_dir" "$chip" "$variant")"
meta="${cache%.bin}.meta"

if [ -f "$cache" ]; then
    printf 'Bootloader already cached for %s (%s): %s\n' "$chip" "$variant" "$cache"
    exit 0
fi

# Feature list of the representative example; the OTA variant always needs the ota feature.
features=$(get_example_features_from_toml "$example" "$workspace_root/crates/$pkg") || exit 1
if [ "$variant" = "ota" ]; then
    case ",$features," in
        *,ota,*) ;;
        *) features="$features,ota" ;;
    esac
fi

printf 'IDF %s bootloader not cached for %s — building %s (features=%s) to populate it...\n' \
    "$variant" "$chip" "$example" "$features"

# Clean first so the build below is the only esp-idf-sys build the harvest can see.
clean_idf_sys

if [ "$variant" = "ota" ]; then
    if ! apply_ota_sdkconfig "$workspace_root" "$idf_dir_abs"; then
        printf 'Error: %s/sdkconfig.ota.defaults not found — cannot build the OTA bootloader.\n' "$workspace_root" >&2
        exit 1
    fi
else
    # An inherited ESP_IDF_SDKCONFIG_DEFAULTS (e.g. from a direnv export) would poison the generic build.
    unset ESP_IDF_SDKCONFIG_DEFAULTS
fi

case "$mcu" in
    esp32|esp32s3)
        # shellcheck source=/dev/null
        source "$SCRIPT_DIR/xtensa-toolchain.sh"
        setup_xtensa_toolchain
        MCU="$mcu" cargo +esp build --release --target "$idf_target" --target-dir "$idf_dir_abs" --features "$features" --example "$example" -p "$pkg"
        ;;
    *)
        MCU="$mcu" cargo build --release --target "$idf_target" --target-dir "$idf_dir_abs" --features "$features" --example "$example" -p "$pkg"
        ;;
esac

# Only this build's esp-idf-sys output exists after the clean above; anything else is an error.
out=$(find_idf_bootloader_out "$idf_target" "$idf_dir_abs") || exit 1
if [ -z "$out" ]; then
    printf 'Error: IDF bootloader still not found after building %s.\n' "$example" >&2
    printf 'Expected under: %s/release/build/esp-idf-sys-*/out/build/bootloader/\n' "$idf_dir_abs/$idf_target" >&2
    case "$variant:$idf_target" in
        ota:*|generic:xtensa-*)
            # Fatal: the espflash bundled bootloader is incompatible on Xtensa and lacks rollback.
            printf 'Fatal (%s, %s) — the espflash bundled bootloader is not a safe fallback.\n' "$chip" "$variant" >&2
            exit 1
            ;;
        *)
            printf 'Continuing on RISC-V (%s) — flash.sh will fall back to the espflash bundled bootloader.\n' "$chip" >&2
            exit 0
            ;;
    esac
fi

# Verify the build's own sdkconfig: the rollback setting must match the variant.
if ! check_bootloader_rollback "$out" "$variant"; then
    printf 'Error: %s/sdkconfig does not match the %s variant (CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE).\n' "$out" "$variant" >&2
    exit 1
fi
rollback_line=$(grep -E '^(# )?CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE' "$out/sdkconfig" || true)
if [ "$variant" = "ota" ]; then
    if ! grep -qxF "CONFIG_PARTITION_TABLE_CUSTOM_FILENAME=\"$workspace_root/partitions.ota.csv\"" "$out/sdkconfig"; then
        printf 'Error: %s/sdkconfig does not use partitions.ota.csv.\n' "$out" >&2
        exit 1
    fi
fi

mkdir -p "$(dirname "$cache")"
tmp="$cache.tmp.$$"
cp "$out/build/bootloader/bootloader.bin" "$tmp"
mv "$tmp" "$cache"
printf 'source_out=%s\nvariant=%s\nexample=%s\nfeatures=%s\nsdkconfig=%s\n' \
    "$out" "$variant" "$example" "$features" "$rollback_line" > "$meta"

# Leave no esp-idf-sys output behind that a later build of another variant could reuse.
clean_idf_sys

printf 'Bootloader cached for %s (%s): %s\n' "$chip" "$variant" "$cache"
printf '  harvested from: %s\n  %s\n' "$out" "$rollback_line"
