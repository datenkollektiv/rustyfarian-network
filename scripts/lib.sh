#!/usr/bin/env bash
# lib.sh — shared helper functions for scripts/
# Source this file; do not execute it directly.

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    printf 'Error: lib.sh must be sourced, not executed directly.\n' >&2
    exit 2
fi

# abs_dir <dir>
# Prints <dir> as an absolute path. An absolute input is returned unchanged; a relative
# input is resolved against $PWD (the workspace root when called from a just recipe).
abs_dir() {
    case "$1" in
        /*) printf '%s' "$1" ;;
        *)  printf '%s/%s' "$PWD" "$1" ;;
    esac
}

# bootloader_cache_path <idf_dir> <chip> [variant]
# Prints the stable cache path of the IDF-built bootloader for <chip>.
#   variant: generic (default) -> <idf_dir>/bootloader/<chip>.bin
#            ota               -> <idf_dir>/bootloader/<chip>-ota.bin (CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y)
# ensure-bootloader.sh writes these files; flash.sh reads them and never globs the build tree.
bootloader_cache_path() {
    local idf_dir="$1" chip="$2" variant="${3:-generic}"
    local suffix=""
    if [ "$variant" = "ota" ]; then
        suffix="-ota"
    fi
    printf '%s/bootloader/%s%s.bin' "$(abs_dir "$idf_dir")" "$chip" "$suffix"
}

# pinned_idf_version <workspace_root>
# Prints the ESP-IDF version esp-idf-sys builds: $ESP_IDF_VERSION when exported (cargo's [env]
# never overrides an existing variable), else the pin in .cargo/config.toml.
pinned_idf_version() {
    if [ -n "${ESP_IDF_VERSION:-}" ]; then
        printf '%s' "$ESP_IDF_VERSION"
        return 0
    fi
    sed -n 's/^ESP_IDF_VERSION *= *"\(.*\)".*/\1/p' "$1/.cargo/config.toml"
}

# find_idf_bootloader_out <idf_target> <idf_dir>
# Prints the esp-idf-sys out dir (the one holding build/bootloader/bootloader.bin) of the
# single IDF build for <idf_target>. Prints nothing when no such build exists.
# Returns 1 when several candidates exist: the caller must clean esp-idf-sys and rebuild
# the one build it needs rather than guess which hash it came from.
find_idf_bootloader_out() {
    local idf_target="$1"
    local root
    root="$(abs_dir "$2")/$1/release/build"
    # nullglob makes the array empty (not a literal pattern string) when nothing matches.
    shopt -s nullglob
    local bl_candidates=( "$root"/esp-idf-sys-*/out/build/bootloader/bootloader.bin )
    shopt -u nullglob
    if [ ${#bl_candidates[@]} -eq 0 ]; then
        return 0
    fi
    if [ ${#bl_candidates[@]} -gt 1 ]; then
        printf 'Error: multiple IDF-built bootloaders found for target "%s".\n' "$idf_target" >&2
        printf 'Candidates:\n' >&2
        for cand in "${bl_candidates[@]}"; do
            printf '  %s\n' "$cand" >&2
        done
        return 1
    fi
    local bl="${bl_candidates[0]}"
    printf '%s' "${bl%/build/bootloader/bootloader.bin}"
}

# check_bootloader_rollback <esp-idf-sys out dir> <variant>
# Greps the build's out/sdkconfig and returns 0 only when the rollback setting matches the
# variant (ota: enabled; generic: disabled). Never trusts a successful compile.
check_bootloader_rollback() {
    local sdk="$1/sdkconfig" variant="$2"
    [ -f "$sdk" ] || return 1
    if [ "$variant" = "ota" ]; then
        grep -qx 'CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y' "$sdk"
    else
        grep -qx '# CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE is not set' "$sdk"
    fi
}

# apply_ota_sdkconfig <workspace_root> <idf_dir>
# Exports ESP_IDF_SDKCONFIG_DEFAULTS for the OTA variant: base defaults, the OTA overlay and a
# generated partition-table overlay. Every entry is an ABSOLUTE path (a relative entry silently
# drops all of them). Returns 1 when sdkconfig.ota.defaults is missing, exporting nothing.
apply_ota_sdkconfig() {
    local root="$1"
    local ota_sdkconfig="$root/sdkconfig.ota.defaults"
    [ -f "$ota_sdkconfig" ] || return 1
    local idf_dir_abs
    idf_dir_abs="$(abs_dir "$2")"
    mkdir -p "$idf_dir_abs"
    # The partition-table path is written to a generated overlay instead of being hard-coded in
    # the committed sdkconfig.ota.defaults, because ESP-IDF resolves a relative one against the
    # generated esp-idf-sys project, not the workspace.
    local ota_partitions_overlay="$idf_dir_abs/sdkconfig.ota-partitions.defaults"
    printf 'CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="%s"\n' "$root/partitions.ota.csv" > "$ota_partitions_overlay"
    export ESP_IDF_SDKCONFIG_DEFAULTS="$root/sdkconfig.defaults;$ota_sdkconfig;$ota_partitions_overlay"
}

# get_example_features_from_toml <example_name> <crate_dir>
# Prints the required-features of an example as a comma-separated list (e.g., "wifi,mqtt").
# Exit: 1 if not found
get_example_features_from_toml() {
    local example_name="$1"
    local crate_dir="$2"

    if [ ! -f "$crate_dir/Cargo.toml" ]; then
        printf 'ERROR: %s/Cargo.toml not found\n' "$crate_dir" >&2
        return 1
    fi

    local in_example=0 found_example=0 features=""

    while IFS= read -r line; do
        # Check if entering an [[example]] block
        if [[ "$line" == "[[example]]" ]]; then
            in_example=1
            found_example=0
            features=""
            continue
        fi

        if [ $in_example -eq 1 ]; then
            # Check if this is the example we want
            if [[ "$line" =~ ^name\ =\ \"([^\"]+)\" ]]; then
                if [ "${BASH_REMATCH[1]}" = "$example_name" ]; then
                    found_example=1
                else
                    in_example=0
                fi
            fi

            # Extract required-features if we found the right example
            if [ $found_example -eq 1 ]; then
                if [[ "$line" =~ ^required-features\ =\ \[(.*)\] ]]; then
                    features="${BASH_REMATCH[1]}"
                    break
                fi
            fi

            # Stop if we hit another [[...]] that's not [[example]]
            if [[ "$line" =~ ^\[\[ && ! "$line" =~ ^\[\[example\]\] ]]; then
                in_example=0
            fi
        fi
    done < "$crate_dir/Cargo.toml"

    if [ -z "$features" ]; then
        printf 'ERROR: Example "%s" not found or has no required-features in %s/Cargo.toml\n' \
            "$example_name" "$crate_dir" >&2
        return 1
    fi

    # Convert TOML array format to comma-separated
    # Input: "wifi", "mqtt"  →  Output: wifi,mqtt
    features=$(printf '%s' "$features" | tr -d '"' | tr -d ' ')
    printf '%s' "$features"
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
