#!/usr/bin/env bash
set -euo pipefail
# ensure-bootloader.sh — ensure IDF-built v5.3.3 bootloader is cached
# Usage: scripts/ensure-bootloader.sh <chip> [hal_dir [idf_dir]]   chip: c3 | c6 | esp32 | esp32s3
#
# espflash 4.x bundles an ESP-IDF v5.5.1 bootloader that rejects both v5.3.3 IDF
# binaries (32 KB MMU page mismatch) and bare-metal esp-hal binaries (app descriptor
# format). The v5.3.3 bootloader built by esp-idf-sys works for both.
#
# For c6: no IDF example exists yet so no bootloader is cached; exits 0 to signal the
# caller (flash.sh) to fall through to the espflash bundled bootloader.  The Xtensa
# MMU page-size mismatch that makes bundled bootloaders problematic on esp32/esp32s3
# does not apply to RISC-V targets.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

if [ $# -lt 1 ]; then
    printf 'Usage: %s <chip>\n  chip: c3 | c6 | esp32 | esp32s3\n' "$0" >&2
    exit 2
fi

chip="$1"
hal_dir="${2:-target/hal}"
idf_dir="${3:-target/idf}"

# Map chip to IDF target, representative example, and MCU
case "$chip" in
    c3)
        idf_target="riscv32imc-esp-espidf"
        idf_example="idf_c3_connect"
        ;;
    esp32)
        idf_target="xtensa-esp32-espidf"
        idf_example="idf_esp32_mqtt"
        ;;
    esp32s3)
        idf_target="xtensa-esp32s3-espidf"
        idf_example="idf_esp32s3_join"
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

bl=$(find_idf_bootloader "$idf_target" "$idf_dir")
if [ -z "$bl" ]; then
    printf 'IDF bootloader not cached for %s — building %s to populate it...\n' "$chip" "$idf_example"
    "$SCRIPT_DIR/build-example.sh" "$idf_example" "$hal_dir" "$idf_dir"
    # Re-check after building: the build should have generated the bootloader via esp-idf-sys.
    # If it's still missing, that indicates a build or cache issue that must not be silently ignored.
    bl=$(find_idf_bootloader "$idf_target" "$idf_dir")
    if [ -z "$bl" ]; then
        printf 'Error: IDF bootloader still not found after building %s.\n' "$idf_example" >&2
        printf 'Expected under: %s/%s/release/build/esp-idf-sys-*/out/build/bootloader/\n' "$idf_dir" "$idf_target" >&2
        printf 'Verify esp-idf-sys built successfully and the build output was not corrupted.\n' >&2
        case "$idf_target" in
            xtensa-*)
                # Fatal on Xtensa: the espflash-bundled v5.5.1 bootloader has a 32 KB vs
                # 64 KB MMU page-size mismatch with v5.3.3 binaries, so there is no safe
                # fallback and flashing would produce a board that does not boot.
                printf 'Fatal on Xtensa (%s) — the espflash bundled bootloader is not a safe fallback.\n' "$chip" >&2
                exit 1
                ;;
            *)
                # Non-fatal on RISC-V: the bundled bootloader is generally compatible there
                # (same rationale as the c6 early exit above), so warn loudly and let
                # flash.sh fall through to its existing bundled-bootloader path rather than
                # blocking the flash entirely.
                printf 'Continuing on RISC-V (%s) — flash.sh will fall back to the espflash bundled bootloader.\n' "$chip" >&2
                exit 0
                ;;
        esac
    fi
    printf 'Bootloader cached for %s: %s\n' "$chip" "$bl"
else
    printf 'Bootloader already cached for %s: %s\n' "$chip" "$bl"
fi
