#!/usr/bin/env bash
# lib.sh — shared helper functions for scripts/
# Source this file; do not execute it directly.

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    printf 'Error: lib.sh must be sourced, not executed directly.\n' >&2
    exit 2
fi

# find_idf_bootloader <idf_target> [idf_dir]
# Prints the path of the single IDF-built bootloader to stdout.
# Prints nothing if no bootloader is found.
# Exits with an error if multiple candidates are found (ambiguous — build dirs must be cleaned first).
find_idf_bootloader() {
    local idf_target="$1"
    local idf_dir="${2:-target/idf}"
    # Normalize idf_dir: if absolute (starts with /), use as-is; if relative, prefix with PWD.
    # This allows callers to pass either relative paths (e.g., target/idf) or absolute
    # paths (e.g., /Volumes/RustBuilds/targets/idf/project-name) from the justfile.
    local resolved_idf_dir
    if [[ "$idf_dir" = /* ]]; then
        resolved_idf_dir="$idf_dir"
    else
        resolved_idf_dir="$PWD/$idf_dir"
    fi
    # nullglob makes the array empty (not a literal pattern string) when nothing matches,
    # so the zero/one/many logic below is reliable without an additional -e check.
    shopt -s nullglob
    local bl_candidates=( "$resolved_idf_dir/$idf_target/release/build"/esp-idf-sys-*/out/build/bootloader/bootloader.bin )
    shopt -u nullglob
    if [ ${#bl_candidates[@]} -gt 0 ]; then
        if [ ${#bl_candidates[@]} -gt 1 ]; then
            printf 'Error: multiple IDF-built bootloaders found for target "%s".\n' "$idf_target" >&2
            printf 'Run: cargo clean -p esp-idf-sys, or remove unused esp-idf-sys-* build directories.\nCandidates:\n' >&2
            for cand in "${bl_candidates[@]}"; do
                printf '  %s\n' "$cand" >&2
            done
            exit 1
        fi
        echo "${bl_candidates[0]}"
    fi
}

# detect_lan_ip
# Detect the LAN IP of the default interface.
# On macOS: uses route + ipconfig
# On Linux: uses ip route get
# Outputs the IP address to stdout; returns 0 on success, 1 on failure.
detect_lan_ip() {
    if [ "$(uname)" = "Darwin" ]; then
        # macOS: get default interface and its IP
        local default_if
        default_if=$(route -n get default 2>/dev/null | grep interface | awk '{print $2}' || echo "")
        if [ -n "$default_if" ]; then
            ipconfig getifaddr "$default_if" 2>/dev/null && return 0
        fi
    else
        # Linux: use `ip route get`
        local lan_ip
        lan_ip=$(ip route get 1.1.1.1 2>/dev/null | head -1 | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}' || echo "")
        if [ -n "$lan_ip" ]; then
            printf '%s' "$lan_ip"
            return 0
        fi
    fi
    return 1
}
