#!/usr/bin/env bash
set -euo pipefail
# doctor.sh — check development prerequisites (RAM disk, sccache)
# Usage: scripts/doctor.sh <ramdisk> <hal_dir> <idf_dir>

ramdisk="$1"
hal_dir="$2"
idf_dir="$3"

if [ -d "$ramdisk" ]; then
    if [ -d "$ramdisk/targets/hal" ] && [ -d "$ramdisk/targets/idf" ]; then
        printf "  ramdisk    ok       %s\n" "$ramdisk"
        printf "  hal target ok       %s\n" "$hal_dir"
        printf "  idf target ok       %s\n" "$idf_dir"
    else
        printf "  ramdisk    PARTIAL  %s (subdirs missing — run: just ramdisk attach)\n" "$ramdisk"
        printf "  hal target fallback %s\n" "$hal_dir"
        printf "  idf target fallback %s\n" "$idf_dir"
    fi
else
    printf "  ramdisk    MISSING  run: just ramdisk attach\n"
    printf "  hal target fallback %s\n" "$hal_dir"
    printf "  idf target fallback %s\n" "$idf_dir"
fi

if command -v sccache >/dev/null 2>&1; then
    if [ "${RUSTC_WRAPPER:-}" = "sccache" ]; then
        printf "  sccache    ok       %s\n" "$(sccache --version 2>/dev/null)"
    else
        printf "  sccache    --       installed but RUSTC_WRAPPER not set\n"
    fi
else
    printf "  sccache    MISSING  run: brew install sccache  (optional, speeds up cold builds)\n"
fi

# mermaid-cli prerequisites — needed for `just lint-docs` to validate diagrams in docs/.
if command -v npx >/dev/null 2>&1; then
    printf "  npx        ok       %s\n" "$(npx --version 2>/dev/null)"
    puppeteer_cache="${PUPPETEER_CACHE_DIR:-$HOME/.cache/puppeteer}"
    chrome_exe="$(find "$puppeteer_cache" -type f \( -name 'Google Chrome for Testing' -o -name 'chrome-headless-shell' \) 2>/dev/null | head -1)"
    if [ -n "$chrome_exe" ]; then
        printf "  mmdc chrome ok      %s\n" "$chrome_exe"
    else
        printf "  mmdc chrome MISSING run: npx puppeteer browsers install chrome  (needed for: just lint-docs)\n"
    fi
else
    printf "  npx        MISSING  install Node.js  (needed for: just lint-docs)\n"
fi
