#!/usr/bin/env bash
set -euo pipefail
# ddi-bundle.sh — build a DDI example with FIRMWARE_VERSION compiled in and write the fleet's release bundle
# Usage: scripts/ddi-bundle.sh <example> <version> [hal_dir [idf_dir]]
#   example: idf_{c3|c6|esp32|esp32s3}_{feature}, e.g. idf_c3_ddi (the DDI runtime is ESP-IDF only)
#   version: MAJOR.MINOR.PATCH, decimal, no leading zeros, each component at most 65535
#
# Writes the bundle to tmp/ddi-bundle/ under the workspace root. The directory is git-ignored, and the
# script refuses a git-tracked one, because the image carries Wi-Fi credentials and the DDI token.
#   <name>.bin           app image from the OTA layout (partitions.ota.csv), name = <example>-<controller id>-v<version>
#   <name>.bin.sha256    64 lowercase hex characters, the sha256 of <name>.bin
#   <name>.bin.version   the version
#   <name>.bin.chip      the ESP-IDF target name, e.g. esp32c3
# Before the sidecars are written, the header is checked the way the fleet server checks an ingest:
# byte 0 must be 0xE9 (ESP app-image magic) and the little-endian u16 at offset 12 must be the chip id.
# These four files seed the lab DDI server.
#
# The image is DEVICE-SPECIFIC: DDI_CONTROLLER_ID, DDI_TOKEN and the Wi-Fi credentials are compiled in, so the
# bundle name carries the controller id and the bundle may only be assigned to that one target.
# The example must require the `ota-ddi` feature, and DDI_CONTROLLER_ID and DDI_TOKEN must be set (from .env),
# so a bundle never holds an image without a DDI client or one that refuses to start.

usage() {
    printf 'Usage: %s <example> <version> [hal_dir [idf_dir]]\n  example: idf_{c3|c6|esp32|esp32s3}_{feature}, e.g. idf_c3_ddi\n  version: MAJOR.MINOR.PATCH, no leading zeros, each component at most 65535, e.g. 1.0.0\n' "$0" >&2
}

if [ $# -lt 2 ]; then
    usage
    exit 2
fi

example="$1"
version="$2"
hal_dir="${3:-target/hal}"
idf_dir="${4:-target/idf}"
workspace_root="$(cd "$(dirname "$0")/.." && pwd)"

case "$example" in
    idf_*) ;;
    *)
        printf 'Error: example must start with "idf_" (the DDI runtime is ESP-IDF only), got "%s".\n' "$example" >&2
        usage
        exit 1
        ;;
esac

# Chip is the second '_' field of the name; the map is ota-image.sh's, plus the chip id from the image header
chip=$(printf '%s' "$example" | cut -d_ -f2)
case "$chip" in
    c3)      mcu="esp32c3";  target="riscv32imc-esp-espidf";  chip_id=5  ;;
    c6)      mcu="esp32c6";  target="riscv32imac-esp-espidf"; chip_id=13 ;;
    esp32)   mcu="esp32";    target="xtensa-esp32-espidf";    chip_id=0  ;;
    esp32s3) mcu="esp32s3";  target="xtensa-esp32s3-espidf";  chip_id=9  ;;
    *)
        printf 'Error: unknown chip "%s" in example "%s". Name must follow idf_{c3|c6|esp32|esp32s3}_{feature}.\n' "$chip" "$example" >&2
        usage
        exit 1
        ;;
esac

# [[ =~ ]] matches the whole string, so a newline cannot smuggle a second line past the check
version_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
if [[ ! "$version" =~ $version_re ]]; then
    printf 'Error: version must be MAJOR.MINOR.PATCH (decimal, no sign, no leading zeros), got "%s".\n' "$version" >&2
    usage
    exit 1
fi
IFS=. read -r major minor patch <<< "$version"
for part in "$major" "$minor" "$patch"; do
    if [ "${#part}" -gt 5 ] || [ "$part" -gt 65535 ]; then
        printf 'Error: version component "%s" in "%s" is above 65535.\n' "$part" "$version" >&2
        exit 1
    fi
done

# Only an example whose required-features carry the DDI client can be a DDI bundle
features=$(awk -v ex="$example" '
    /^\[\[example\]\]/ { inex = 1; found = 0; next }
    /^\[/ { inex = 0 }
    inex && $0 == "name = \"" ex "\"" { found = 1 }
    inex && found && /^required-features = / { print; exit }
' "$workspace_root/crates/rustyfarian-esp-idf-network/Cargo.toml")
if ! printf '%s' "$features" | grep -q '"ota-ddi'; then
    printf 'Error: example "%s" does not require the ota-ddi feature; a DDI bundle needs the DDI client.\n' "$example" >&2
    exit 1
fi

# The image is device-specific; refuse to bundle one that would refuse to start, and never print the token
controller_id="${DDI_CONTROLLER_ID:-}"
if [ -z "$controller_id" ] || [ -z "${DDI_TOKEN:-}" ]; then
    printf 'Error: DDI_CONTROLLER_ID and DDI_TOKEN must be set (in .env) before bundling; the image carries them.\n' >&2
    exit 1
fi
id_re='^[a-z0-9][a-z0-9-]{2,19}$'
if [[ ! "$controller_id" =~ $id_re ]]; then
    printf 'Error: DDI_CONTROLLER_ID must match %s.\n' "$id_re" >&2
    exit 1
fi
# The firmware's rule (juggler::identity::is_target_token, 32 to 64 lowercase hex digits); the message never shows the token
token_re='^[0-9a-f]{32,64}$'
if [[ ! "$DDI_TOKEN" =~ $token_re ]]; then
    printf 'Error: DDI_TOKEN must be 32 to 64 lowercase hex digits; the firmware refuses any other token at boot.\n' >&2
    exit 1
fi

# The bundle name is also the file name the fleet receives, so it gets the fleet's file-name rule
name="${example}-${controller_id}-v${version}"
name_re='^[A-Za-z0-9_][A-Za-z0-9._-]*$'
if [[ ! "$name" =~ $name_re ]] || [ "${#name}" -gt 64 ]; then
    printf 'Error: bundle name "%s" must match %s and be at most 64 characters.\n' "$name" "$name_re" >&2
    exit 1
fi

out_dir="$workspace_root/tmp/ddi-bundle"
# exit 1 = inside the repo and not ignored; 0 = ignored, 128 = outside the repo
rc=0
git -C "$workspace_root" check-ignore -q "$out_dir" 2>/dev/null || rc=$?
if [ "$rc" -eq 1 ]; then
    printf 'Error: %s would be tracked by git; the bundle holds Wi-Fi credentials and the DDI token, so write it under tmp/.\n' "$out_dir" >&2
    exit 1
fi

# Build the example; the version is compiled in via option_env!("FIRMWARE_VERSION")
export FIRMWARE_VERSION="$version"
"$(dirname "$0")/build-example.sh" "$example" "$hal_dir" "$idf_dir"

elf="$idf_dir/$target/release/examples/$example"
if [ ! -f "$elf" ]; then
    printf 'Error: %s not found after the build.\n' "$elf" >&2
    exit 1
fi

mkdir -p "$out_dir"
bin="$out_dir/$name.bin"

# Every check below removes the whole bundle on failure, so a refused image is never left where it could be uploaded
reject() {
    rm -f "$bin" "$bin.sha256" "$bin.version" "$bin.chip"
    printf 'Error: %s\n' "$1" >&2
    exit 1
}

# The previous bundle goes first, so a run that fails part-way never leaves sidecars that describe another image
rm -f "$bin" "$bin.sha256" "$bin.version" "$bin.chip"

espflash save-image --chip "$mcu" --flash-size 4mb --partition-table "$workspace_root/partitions.ota.csv" "$elf" "$bin"

size=$(wc -c < "$bin" | tr -d ' ')
slot=$("$(dirname "$0")/ota-slot-size.sh" "$workspace_root/partitions.ota.csv")
if [ "$size" -gt "$slot" ]; then
    reject "image is $size bytes, larger than the $slot-byte OTA slot in partitions.ota.csv"
fi
if [ "$size" -lt 16 ]; then
    reject "image is $size bytes, too short to hold an app header"
fi

# Read the header byte by byte: od -tu2 would follow the host byte order, and the header is little-endian
magic=$(od -An -tu1 -N1 "$bin" | tr -d ' ')
lo=$(od -An -tu1 -j12 -N1 "$bin" | tr -d ' ')
hi=$(od -An -tu1 -j13 -N1 "$bin" | tr -d ' ')
image_chip_id=$((hi * 256 + lo))
if [ "$magic" -ne 233 ]; then
    reject "image byte 0 is $magic, expected 233 (0xE9, the ESP app-image magic)"
fi
if [ "$image_chip_id" -ne "$chip_id" ]; then
    reject "image chip id at offset 12 is $image_chip_id, expected $chip_id for $mcu"
fi

sha256=$(shasum -a 256 "$bin" | cut -d' ' -f1)
printf '%s\n' "$sha256" > "$bin.sha256"
printf '%s\n' "$version" > "$bin.version"
printf '%s\n' "$mcu" > "$bin.chip"

echo "Image:    $bin ($size bytes, sha256 $sha256)"
echo "Sha256:   $bin.sha256"
echo "Version:  $bin.version"
echo "Chip:     $bin.chip"
echo "Note:     device-specific image for target $controller_id; assign it to no other target"
