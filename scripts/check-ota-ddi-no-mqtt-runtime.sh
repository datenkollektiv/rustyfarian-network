#!/usr/bin/env bash
set -euo pipefail
# check-ota-ddi-no-mqtt-runtime.sh — assert a wifi,mqtt,ota-ddi build carries no MQTT OTA runtime, and that
# ota-ddi with ota-wire does not build without the bench-only opt-in
# Usage: scripts/check-ota-ddi-no-mqtt-runtime.sh <target> <target-dir>
# 1. Feature graph: normal edges only, so dev-dependencies (the esp-hal crate's juggler dev-dep enables ota-wire)
#    cannot leak in; fails when juggler or rustyfarian-esp-idf-network has the ota-wire feature.
# 2. Expanded source: the crate after cfg stripping and macro expansion must not define the MQTT command
#    intake (`struct OtaSubmitter`) or the MQTT runtime module; doc comments that name them do not count.
# 3. The guard: `--features ota-ddi,ota-wire` must fail to compile naming `ota-ddi-bench-mqtt-wire` (fleet DDI
#    profile 0.4.0 §1: a release image accepts no OTA command over MQTT), and that opt-in alone must build.

target="${1:?usage: scripts/check-ota-ddi-no-mqtt-runtime.sh <target> <target-dir>}"
target_dir="${2:?usage: scripts/check-ota-ddi-no-mqtt-runtime.sh <target> <target-dir>}"
features="wifi,mqtt,ota-ddi"

graph=$(cargo tree -p rustyfarian-esp-idf-network --features "$features" -e normal \
  --target "$target" --prefix none -f '{p} {f}')

offending=$(printf '%s\n' "$graph" | grep -E '^(juggler|rustyfarian-esp-idf-network) .*ota-wire' || true)

if [ -n "$offending" ]; then
  printf '%s\n' "$offending"
  echo "ERROR: $features enables ota-wire; the DDI build must not carry the MQTT OTA runtime"
  exit 1
fi

echo "ok: $features feature graph has no ota-wire (juggler, rustyfarian-esp-idf-network)"

expanded=$(cargo rustc -p rustyfarian-esp-idf-network --lib --profile check --features "$features" \
  --target-dir "$target_dir" -- -Zunpretty=expanded)

if ! grep -qE '^ *pub mod ddi \{' <<<"$expanded"; then
  echo "ERROR: expanded source lacks the ota::ddi module; the check is not looking at a DDI build"
  exit 1
fi

found=$(grep -nE '\bstruct OtaSubmitter\b|^ *pub mod runtime \{' <<<"$expanded" || true)

if [ -n "$found" ]; then
  printf '%s\n' "$found" | head -5
  echo "ERROR: the $features build compiles the MQTT OTA runtime (OtaSubmitter or ota::runtime)"
  exit 1
fi

echo "ok: $features expanded source has no OtaSubmitter and no ota::runtime"

guarded="ota-ddi,ota-wire"
opt_in="ota-ddi-bench-mqtt-wire"

if output=$(cargo check -p rustyfarian-esp-idf-network --features "$guarded" --target-dir "$target_dir" 2>&1); then
  echo "ERROR: --features $guarded builds; the ota-ddi + ota-wire guard is missing"
  exit 1
fi

if ! grep -q -- "$opt_in" <<<"$output"; then
  printf '%s\n' "$output" | tail -20
  echo "ERROR: --features $guarded fails, but not with the ota-ddi + ota-wire guard"
  exit 1
fi

echo "ok: $guarded does not build without $opt_in"

cargo check -p rustyfarian-esp-idf-network --features "$opt_in" --target-dir "$target_dir"

echo "ok: $opt_in builds ota-ddi with ota-wire"
