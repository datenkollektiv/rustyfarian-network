#!/usr/bin/env bash
set -euo pipefail
# build-example.sh — build a named example (no flash)
# Usage: scripts/build-example.sh <example>
#   example: idf_{chip}_{feature}  e.g. idf_c3_connect, idf_c3_mqtt
#
# Chip and crate are auto-detected from the example name.
# MCU and Cargo target are set per chip so the image matches the physical hardware.

if [ $# -lt 1 ]; then
    printf 'Usage: %s <example>\n  example: idf_{chip}_{feature}  e.g. idf_c3_connect, idf_c3_mqtt, idf_esp32_mqtt\n' "$0" >&2
    exit 2
fi

example="$1"

# Detect package from example feature name
case "$example" in
    *mqtt*)    pkg="rustyfarian-esp-idf-mqtt" ;;
    *connect*) pkg="rustyfarian-esp-idf-wifi" ;;
    *) printf 'Cannot detect crate for example "%s".\nName must contain "mqtt" or "connect".\n' "$example" >&2; exit 1 ;;
esac

# Detect chip and set MCU / Cargo target
chip=$(printf '%s' "$example" | cut -d_ -f2)
case "$chip" in
    c3) mcu="esp32c3"; target="riscv32imc-esp-espidf" ;;
    c6) mcu="esp32c6"; target="riscv32imac-esp-espidf" ;;
    esp32) mcu="esp32"; target="xtensa-esp32-espidf" ;;
    *) printf 'Unknown chip "%s" in example "%s". Name must follow idf_{c3|c6|esp32}_{feature}.\n' "$chip" "$example" >&2; exit 1 ;;
esac

printf 'Building %s for %s (MCU=%s)...\n' "$example" "$target" "$mcu"
MCU="$mcu" cargo build --release --target "$target" --example "$example" -p "$pkg"
