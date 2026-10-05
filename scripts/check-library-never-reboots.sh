#!/usr/bin/env bash
set -euo pipefail
# check-library-never-reboots.sh — verify library boundaries (no restarts, state changes, or resets)
# Usage: scripts/check-library-never-reboots.sh

mkdir -p tmp
exit_code=0

if find crates/rustyfarian-esp-hal-network/src/provisioning/ -name '*.rs' -exec \
  grep -Hn 'esp_hal::reset\|software_reset\|esp_hal_reset' {} \; > tmp/resets.txt 2>&1; then
  if [ -s tmp/resets.txt ]; then
    cat tmp/resets.txt
    echo "ERROR: Found reset/reboot calls in library source"
    exit_code=1
  fi
fi

if grep -Hn 'erase_all' crates/rustyfarian-esp-hal-network/src/provisioning/portal.rs > tmp/erase.txt 2>&1; then
  if [ -s tmp/erase.txt ]; then
    cat tmp/erase.txt
    echo "ERROR: Found erase_all call in portal.rs"
    exit_code=1
  fi
fi

if find crates/rustyfarian-esp-idf-network/src/ota crates/juggler/src/ota/persist -name '*.rs' \
  \( -name store.rs -o -name boot_facts.rs -o -path '*/persist/*' \) -exec \
  grep -HnE 'esp_restart|\brestart\(|esp_ota_set_boot_partition|set_boot_slot|initiate_update|\.complete\(|mark_running_slot_invalid_and_reboot|mark_running_slot_valid|esp_ota_mark_app_valid|esp_ota_mark_app_invalid_rollback_and_reboot|\.rollback\(' {} \; > tmp/ota-reboots.txt 2>&1; then
  if [ -s tmp/ota-reboots.txt ]; then
    cat tmp/ota-reboots.txt
    echo "ERROR: Found restart, boot-slot, update-start, rollback or image-state calls in the OTA record store"
    exit_code=1
  fi
fi

runtime_dirs=""
for dir in crates/juggler/src/ota/runtime crates/rustyfarian-esp-idf-network/src/ota/runtime; do
  if [ -d "$dir" ]; then runtime_dirs="$runtime_dirs $dir"; fi
done
if [ -n "$runtime_dirs" ]; then
  grep -rHnE 'esp_restart|reset::restart|\brestart\(|esp_ota_|mark_running_slot|set_boot_slot|initiate_update' $runtime_dirs > tmp/ota-runtime-reboots.txt 2>&1 || true
  if [ -s tmp/ota-runtime-reboots.txt ]; then
    cat tmp/ota-runtime-reboots.txt
    echo "ERROR: Found a restart, boot-slot or update-start call in the OTA runtime; restarts go through the app's restart callback"
    exit_code=1
  fi
fi

exit $exit_code
