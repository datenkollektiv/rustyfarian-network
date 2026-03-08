#!/usr/bin/env bash
# xtensa-toolchain.sh — shared helper: add xtensa-esp32-elf-gcc to PATH if needed
# Source this file; do not execute it directly.

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    printf 'Error: xtensa-toolchain.sh must be sourced, not executed directly.\n' >&2
    exit 2
fi

setup_xtensa_toolchain() {
    if ! command -v xtensa-esp32-elf-gcc >/dev/null 2>&1; then
        local xtensa_bin
        xtensa_bin=$(ls -td \
            "$HOME/.rustup/toolchains/esp/xtensa-esp-elf/"*/xtensa-esp-elf/bin \
            2>/dev/null | head -1 || true)
        if [ -z "$xtensa_bin" ]; then
            xtensa_bin=$(ls -td \
                "$HOME/.espressif/tools/xtensa-esp-elf/"*/xtensa-esp-elf/bin \
                2>/dev/null | head -1 || true)
        fi
        if [ -n "$xtensa_bin" ]; then
            export PATH="$xtensa_bin:$PATH"
        else
            printf 'Error: xtensa-esp32-elf-gcc not found. Searched:\n' >&2
            printf '  ~/.rustup/toolchains/esp/xtensa-esp-elf/*/xtensa-esp-elf/bin\n' >&2
            printf '  ~/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin\n' >&2
            printf 'Source your ESP-IDF or toolchain environment export script (e.g. ". ~/export.sh" or ". ~/export-esp32.sh"; actual name and path may vary by installation).\n' >&2
            return 1
        fi
    fi
}
