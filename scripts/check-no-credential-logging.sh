#!/usr/bin/env bash
set -euo pipefail
# check-no-credential-logging.sh — verify provisioning and bare-metal DDI logs never interpolate credentials
# Usage: scripts/check-no-credential-logging.sh

# Flag a log macro line that names a credential outside a string literal: string
# literals are stripped first, so `info!("wifi_pass missing")` passes and
# `info!("{}", wifi_pass)` fails. Comment lines are ignored. Single-line calls only.
hits=$(grep -rnE 'log::(debug|info|warn|error)!' \
  crates/rustyfarian-esp-hal-network/src/provisioning/ \
  crates/rustyfarian-esp-hal-network/src/ota/ddi/ \
  crates/rustyfarian-esp-hal-network/src/ota/ddi.rs \
  crates/rustyfarian-esp-hal-network/examples/common/ddi_app.rs \
  crates/rustyfarian-esp-hal-network/examples/hal_c3_ddi.rs \
  crates/rustyfarian-esp-hal-network/examples/hal_c6_ddi.rs \
  | sed -E 's/"([^"\\]|\\.)*"//g' \
  | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
  | grep -E '\b(wifi_pass|mqtt_pass|ddi_token|body_str|body_in_buf)\b' || true)
if [ -n "$hits" ]; then
  echo "$hits"
  echo "ERROR: credential field logged in provisioning or DDI code"
  exit 1
fi
