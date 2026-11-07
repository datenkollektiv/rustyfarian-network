#!/usr/bin/env bash
set -euo pipefail
# detect-port.sh — pick a single USB serial port for espflash, or fall through.
#
# `espflash`'s auto-detect can pick the wrong device on macOS when multiple
# `/dev/cu.*` entries exist (Bluetooth headsets, debug consoles, etc.) and
# fails with the generic "Error while connecting to device".  This helper
# narrows the candidate set to USB-attached serial devices (`usbmodem*`,
# `usbserial*` on macOS; `ttyUSB*`, `ttyACM*` on Linux) and only picks one
# when exactly one matches — otherwise it stays out of the way so espflash
# emits its own diagnostic.
#
# Usage:
#
#     port="$(scripts/detect-port.sh [chip])"
#     [ -n "$port" ] && extra_args=(--port "$port")
#
# Honours $ESPFLASH_PORT — if it is set and non-empty, the script echoes it
# verbatim without scanning, so the env var override always wins.
#
# With several USB serial ports attached:
#   - given a chip (`esp32c3`, `esp32c6`, `esp32`, `esp32s3`), the ports are
#     probed with `espflash board-info` and the one whose chip matches is picked;
#     the port last used for that chip is probed first, so usually no other
#     board is touched (a probe resets the board it talks to);
#   - without a chip (e.g. the monitor after `just run`), the port flashed last
#     is reused while it is still attached.
# Picks are remembered in target/espflash-ports ("<chip> <port>" and "last <port>").
#
# Helpful diagnostics are written to stderr; only the chosen port (or nothing)
# goes to stdout.

chip="${1:-}"
cache="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/espflash-ports"

if [ -n "${ESPFLASH_PORT:-}" ]; then
    printf '%s' "$ESPFLASH_PORT"
    exit 0
fi

# cached <key>: the remembered port for a chip, or `last`.
cached() {
    if [ -f "$cache" ]; then
        awk -v k="$1" '$1 == k { p = $2 } END { if (p) print p }' "$cache"
    fi
}

# remember <port>: record the pick for this chip and as the last port flashed.
remember() {
    mkdir -p "$(dirname "$cache")"
    {
        if [ -f "$cache" ]; then awk -v c="$chip" '$1 != c && $1 != "last"' "$cache"; fi
        printf '%s %s\nlast %s\n' "$chip" "$1" "$1"
    } > "$cache.tmp" && mv "$cache.tmp" "$cache"
}

# probe <port>: the chip espflash reads on that port, or nothing.
probe() {
    ESPFLASH_SKIP_UPDATE_CHECK=true espflash board-info --port "$1" 2>/dev/null \
        | awk -F: '/[Cc]hip type/ { split($2, w, " "); print w[1]; exit }'
}

candidates=()
case "$(uname -s)" in
    Darwin)
        # `/dev/cu.*` is the call-up (non-blocking) variant — what espflash
        # should use.  Most setups expose both `cu.foo` and `tty.foo` for the
        # same physical device; fall back to `tty.*` only when no `cu.*`
        # candidates exist to avoid double-counting one device as two ports.
        for pattern in /dev/cu.usbmodem* /dev/cu.usbserial* /dev/cu.SLAB_USBtoUART* /dev/cu.wchusbserial*; do
            [ -e "$pattern" ] && candidates+=("$pattern")
        done
        if [ ${#candidates[@]} -eq 0 ]; then
            for pattern in /dev/tty.usbmodem* /dev/tty.usbserial* /dev/tty.SLAB_USBtoUART* /dev/tty.wchusbserial*; do
                [ -e "$pattern" ] && candidates+=("$pattern")
            done
        fi
        ;;
    Linux)
        for pattern in /dev/ttyUSB* /dev/ttyACM*; do
            [ -e "$pattern" ] && candidates+=("$pattern")
        done
        ;;
    *)
        exit 0
        ;;
esac

case ${#candidates[@]} in
    1)
        if [ -n "$chip" ]; then remember "${candidates[0]}"; fi
        printf '%s' "${candidates[0]}"
        ;;
    0)
        printf 'Note: no USB serial port detected; espflash will scan all serial devices.\n' >&2
        printf '      If connection fails, set ESPFLASH_PORT=/dev/cu.usbmodemXXXX (macOS) or /dev/ttyUSBn (Linux).\n' >&2
        ;;
    *)
        if [ -n "$chip" ]; then
            # Probe the port last used for this chip first, then the others.
            first="$(cached "$chip")"
            ordered=()
            for cand in "${candidates[@]}"; do
                if [ "$cand" = "$first" ]; then ordered=("$cand" "${ordered[@]+"${ordered[@]}"}"); else ordered+=("$cand"); fi
            done
            for cand in "${ordered[@]}"; do
                if [ "$(probe "$cand")" = "$chip" ]; then
                    printf 'Note: %s answers as %s; using it (set ESPFLASH_PORT to override).\n' "$cand" "$chip" >&2
                    remember "$cand"
                    printf '%s' "$cand"
                    exit 0
                fi
            done
            printf 'Note: no attached port answers as %s.\n' "$chip" >&2
        else
            last="$(cached last)"
            for cand in "${candidates[@]}"; do
                if [ "$cand" = "$last" ]; then
                    printf 'Note: using %s, the port flashed last (set ESPFLASH_PORT to override).\n' "$cand" >&2
                    printf '%s' "$cand"
                    exit 0
                fi
            done
        fi
        printf 'Note: multiple USB serial ports detected; espflash auto-detect may pick the wrong one:\n' >&2
        for cand in "${candidates[@]}"; do
            printf '        %s\n' "$cand" >&2
        done
        printf '      Set ESPFLASH_PORT=<one of the above> to disambiguate.\n' >&2
        ;;
esac
