#!/usr/bin/env bash
# Asserts that the esp-hal crate refuses `ota-ddi` + `ota-wire` without the bench-only
# `ota-ddi-bench-mqtt-wire` opt-in (the compile_error! guard in src/lib.rs), and builds with it.
# Usage: scripts/check-ota-ddi-hal-guard.sh <riscv-c3-target> <target-dir>
set -euo pipefail

target=$1
target_dir=$2
chip="esp32c3,unstable,rt,embassy"
guarded="ota-ddi,ota-wire"
opt_in="ota-ddi-bench-mqtt-wire"

check() {
  cargo check -Zbuild-std=core,alloc --target "$target" -p rustyfarian-esp-hal-network \
    --no-default-features --features "$1,$chip" --target-dir "$target_dir"
}

if output=$(check "$guarded" 2>&1); then
  echo "ERROR: --features $guarded builds; the ota-ddi + ota-wire guard is missing"
  exit 1
fi

if ! grep -q -- "$opt_in" <<<"$output"; then
  printf '%s\n' "$output" | tail -20
  echo "ERROR: --features $guarded fails, but not with the ota-ddi + ota-wire guard"
  exit 1
fi

echo "ok: $guarded does not build without $opt_in"

check "$opt_in"

echo "ok: $opt_in builds ota-ddi with ota-wire"
