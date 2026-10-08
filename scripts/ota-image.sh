#!/usr/bin/env bash
set -euo pipefail
# ota-image.sh — build an OTA example and produce app binary, SHA-256, manifest, and update command
# Usage: scripts/ota-image.sh <version> [example] [hal_dir] [idf_dir]
# Requires: version in MAJOR.MINOR.PATCH format
# Optional env: OTA_HOST (auto-detected if unset), OTA_PORT (default 8000)

version="$1"
example="${2:-idf_c3_ota_runtime}"
hal_dir="${3:-.}"
idf_dir="${4:-.}"

if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "version must be MAJOR.MINOR.PATCH, got '$version'" >&2
    exit 1
fi

# Build the example; the version is compiled in via option_env!("FIRMWARE_VERSION")
export FIRMWARE_VERSION="$version"
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

# Auto-detect OTA_HOST if not set
host="${OTA_HOST:-}"
if [ -z "$host" ]; then
    if [ "$(uname)" = "Darwin" ]; then
        # macOS: get default interface and its IP
        default_if=$(route -n get default 2>/dev/null | grep interface | awk '{print $2}' || echo "")
        if [ -n "$default_if" ]; then
            host=$(ipconfig getifaddr "$default_if" 2>/dev/null || echo "")
        fi
    else
        # Linux: use `ip route get`
        host=$(ip route get 1.1.1.1 2>/dev/null | head -1 | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}' || echo "")
    fi

    if [ -z "$host" ]; then
        echo "Error: OTA_HOST could not be auto-detected. Please set it explicitly." >&2
        exit 1
    fi
    echo "Auto-detected OTA_HOST: $host" >&2
fi

port="${OTA_PORT:-8000}"

printf '{"version":"%s","sha256":"%s","url":"http://%s:%s/%s","target":"%s"}\n' \
    "$version" "$sha256" "$host" "$port" "$(basename "$app_bin")" "$mcu" > "$ota_dir/manifest-v${version}.json"
printf '{"manifest_url":"http://%s:%s/manifest-v%s.json"}\n' "$host" "$port" "$version" > "$ota_dir/command-v${version}.json"

echo "Image:    $app_bin ($size bytes, sha256 $sha256)"
echo "Manifest: $ota_dir/manifest-v${version}.json"
echo "Command:  $ota_dir/command-v${version}.json (publish it NON-retained)"
