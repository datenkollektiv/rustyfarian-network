#!/usr/bin/env bash
set -euo pipefail
# ensure-bootloader.sh — ensure IDF-built v5.3.3 bootloader is cached
# Usage: scripts/ensure-bootloader.sh <chip>
#   chip: c3 | c6 | esp32 | esp32s3
#
# For c3/esp32/esp32s3: builds a representative IDF example if the bootloader is not
# already cached under target/<idf-target>/release/build/.
#
# For c6: no IDF example exists yet so no bootloader is cached; exits 0 to signal the
# caller (flash.sh) to fall through to the espflash bundled bootloader.  The Xtensa
# MMU page-size mismatch that makes bundled bootloaders problematic on esp32/esp32s3
# does not apply to RISC-V targets.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -lt 1 ]; then
    printf 'Usage: %s <chip>\n  chip: c3 | c6 | esp32 | esp32s3\n' "$0" >&2
    exit 2
fi

chip="$1"

# Map chip to IDF target, representative example, and MCU
case "$chip" in
    c3)
        idf_target="riscv32imc-esp-espidf"
        idf_example="idf_c3_connect"
        mcu="esp32c3"
        ;;
    esp32)
        idf_target="xtensa-esp32-espidf"
        idf_example="idf_esp32_mqtt"
        mcu="esp32"
        ;;
    esp32s3)
        idf_target="xtensa-esp32s3-espidf"
        idf_example="idf_esp32s3_join"
        mcu="esp32s3"
        ;;
    c6)
        # No IDF example exists for C6 yet, so no IDF-built bootloader can be cached.
        # espflash's bundled bootloader is used instead (see flash.sh fallback path).
        # The Xtensa MMU page-size mismatch that affects ESP32/ESP32-S3 does not apply
        # to RISC-V targets, so the bundled bootloader is generally compatible.
        printf 'Note: no IDF example for c6 — espflash bundled bootloader will be used.\n'
        exit 0
        ;;
    *)
        printf 'Error: Unknown chip "%s". Supported: c3, c6, esp32, esp32s3\n' "$chip" >&2
        exit 1
        ;;
esac

# Check if bootloader already exists in the IDF target's release cache
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
    printf 'Bootloader already cached for %s: %s\n' "$chip" "$bl"
else
    printf 'Building %s to populate bootloader cache...\n' "$idf_example"
    "$SCRIPT_DIR/build-example.sh" "$idf_example"
fi
