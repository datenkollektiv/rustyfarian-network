#!/usr/bin/env bash
set -euo pipefail
# ota-slot-size.sh — print the size in bytes of the ota_0 app slot of a partition table
# Usage: scripts/ota-slot-size.sh <partitions.csv>
# The one place the OTA image scripts (ota-image.sh, ddi-bundle.sh) read the slot size from.

csv="${1:?usage: scripts/ota-slot-size.sh <partitions.csv>}"
size=$(awk -F',' '$1 ~ /^[[:space:]]*ota_0[[:space:]]*$/ { gsub(/[[:space:]]/, "", $5); print $5; exit }' "$csv")
if [ -z "$size" ]; then
    printf 'Error: no ota_0 partition in %s.\n' "$csv" >&2
    exit 1
fi
printf '%d\n' "$((size))"
