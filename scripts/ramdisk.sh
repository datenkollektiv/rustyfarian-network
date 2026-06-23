#!/usr/bin/env bash
set -euo pipefail
# ramdisk.sh — manage the build RAM disk at /Volumes/RustBuilds
# Usage: scripts/ramdisk.sh attach|detach

if [ "$(uname)" != "Darwin" ]; then
    printf 'error: ramdisk.sh requires macOS (hdiutil/diskutil not available)\n' >&2
    exit 1
fi

RAMDISK_NAME="RustBuilds"
RAMDISK_SIZE_GB="${RUSTBUILDS_RAMDISK_SIZE_GB:-${RAMDISK_SIZE_GB:-6}}"

case "${1:-}" in
    attach)
        if [ -d "/Volumes/$RAMDISK_NAME" ]; then
            echo "RAM disk already attached at /Volumes/$RAMDISK_NAME"
        else
            SECTORS=$(( RAMDISK_SIZE_GB * 1024 * 1024 * 1024 / 512 ))
            DEV=$(hdiutil attach -nomount "ram://$SECTORS" | xargs)
            # HFS+ is used deliberately: `diskutil erasevolume HFS+` is the
            # canonical one-step formatter for hdiutil-created RAM devices and
            # works on all macOS versions this project targets.  APFS on a RAM
            # disk requires `newfs_apfs` / separate container creation steps
            # and offers no benefit for an ephemeral build cache.
            diskutil erasevolume HFS+ "$RAMDISK_NAME" "$DEV"
            echo "RAM disk attached at /Volumes/$RAMDISK_NAME (${RAMDISK_SIZE_GB} GB)"
        fi
        mkdir -p "/Volumes/$RAMDISK_NAME/targets/idf"
        mkdir -p "/Volumes/$RAMDISK_NAME/targets/hal"
        ;;
    detach)
        if [ -d "/Volumes/$RAMDISK_NAME" ]; then
            hdiutil detach "/Volumes/$RAMDISK_NAME"
            echo "RAM disk detached."
        else
            echo "RAM disk not attached."
        fi
        ;;
    *)
        printf 'Usage: scripts/ramdisk.sh attach|detach\n' >&2
        exit 1
        ;;
esac
