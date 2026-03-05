#!/usr/bin/env bash
set -euo pipefail
# flash.sh — auto-detect driver crate and flash an example
# Usage: scripts/flash.sh <example>
#   example: {driver}_{chip}_{name}  e.g. idf_c3_connect, idf_c3_mqtt

example="$1"

case "$example" in
    *mqtt*)    pkg="rustyfarian-esp-idf-mqtt" ;;
    *connect*) pkg="rustyfarian-esp-idf-wifi" ;;
    *) printf 'Cannot detect crate for example "%s".\nName must contain "mqtt" or "connect".\n' "$example" >&2; exit 1 ;;
esac

cargo build --release --example "$example" -p "$pkg"
espflash flash target/riscv32imac-esp-espidf/release/examples/"$example"
