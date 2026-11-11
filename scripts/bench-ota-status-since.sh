#!/usr/bin/env bash
# Streams the OTA statuses a bench check's subscriber wrote to tmp/bench-ota-status.log after the marker line
# MARKER (written by the check's `run` step), following the file until the runner ends the probe.
# Usage: scripts/bench-ota-status-since.sh MARKER
set -euo pipefail
marker="$1"
log="tmp/bench-ota-status.log"
tail -n +1 -f "$log" | awk -v m="$marker" '$0 == m { f = 1; next } f { print; fflush() }'
