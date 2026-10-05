#!/usr/bin/env bash
set -euo pipefail
# check-no-credential-logging.sh — verify provisioning library logs never interpolate credentials
# Usage: scripts/check-no-credential-logging.sh

# Flag a log macro line that names a credential outside a string literal: string
# literals are stripped first, so `info!("wifi_pass missing")` passes and
# `info!("{}", wifi_pass)` fails. Comment lines are ignored. Single-line calls only.
hits=$(grep -rnE 'log::(debug|info|warn|error)!' \
  crates/rustyfarian-esp-hal-network/src/provisioning/ \
  | sed -E 's/"([^"\\]|\\.)*"//g' \
  | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
  | grep -E '\b(wifi_pass|mqtt_pass|body_str|body_in_buf)\b' || true)
if [ -n "$hits" ]; then
  echo "$hits"
  echo "ERROR: credential field logged in provisioning library"
  exit 1
fi
