#!/usr/bin/env bash
set -euo pipefail
# ota-image.sh — build an OTA example and produce app binary, SHA-256, manifest, and update command
# Usage: scripts/ota-image.sh <example> <hal_dir> <idf_dir>
# Requires: FIRMWARE_VERSION env (MAJOR.MINOR.PATCH), OTA_HOST/OTA_PORT env (default localhost:8000)

example="$1"
hal_dir="$2"
idf_dir="$3"

version="${FIRMWARE_VERSION:-}"
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "FIRMWARE_VERSION must be set to MAJOR.MINOR.PATCH (dotenv file or environment), got '$version'" >&2
    exit 1
fi

# Build the example
"$(dirname "$0")/build-example.sh" "$example" "$hal_dir" "$idf_dir"

# Extract chip from example name (format: {idf|hal}_<chip>_<name>)
chip=$(printf '%s' "$example" | cut -d_ -f2)
case "$chip" in
    c3)      mcu="esp32c3";  target="riscv32imc-esp-espidf"   ;;
    c6)      mcu="esp32c6";  target="riscv32imac-esp-espidf"  ;;
    esp32)   mcu="esp32";    target="xtensa-esp32-espidf"     ;;
    esp32s3) mcu="esp32s3";  target="xtensa-esp32s3-espidf"   ;;
    *) echo "Unknown chip in example name: $chip" >&2; exit 1 ;;
esac

elf="$idf_dir/$target/release/examples/$example"
ota_dir="target/ota"
mkdir -p "$ota_dir"
app_bin="$ota_dir/${example}-v${version}.bin"

espflash save-image --chip "$mcu" --flash-size 4mb --partition-table partitions.ota.csv "$elf" "$app_bin"

size=$(wc -c < "$app_bin" | tr -d ' ')
slot=$((0x1E0000))
if [ "$size" -gt "$slot" ]; then
    echo "App image is $size bytes, larger than the $slot-byte OTA slot in partitions.ota.csv" >&2
    exit 1
fi

sha256=$(shasum -a 256 "$app_bin" | cut -d' ' -f1)
host="${OTA_HOST:-localhost}"
port="${OTA_PORT:-8000}"

printf '{"version":"%s","sha256":"%s","url":"http://%s:%s/%s","target":"%s"}\n' \
    "$version" "$sha256" "$host" "$port" "$(basename "$app_bin")" "$mcu" > "$ota_dir/manifest-v${version}.json"
printf '{"manifest_url":"http://%s:%s/manifest-v%s.json"}\n' "$host" "$port" "$version" > "$ota_dir/command-v${version}.json"

echo "Image:    $app_bin ($size bytes, sha256 $sha256)"
echo "Manifest: $ota_dir/manifest-v${version}.json"
echo "Command:  $ota_dir/command-v${version}.json (publish it NON-retained)"
