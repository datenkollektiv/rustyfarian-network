#!/usr/bin/env bash
set -euo pipefail
# router.sh — block, unblock or inspect a bench board on the router, so bench router steps run unattended
# Usage: scripts/router.sh <block|unblock|status> <mac> <secrets_env>
#
# `block` disconnects the board and keeps it off the network until `unblock`; `status` prints
# `<mac> blocked` or `<mac> not blocked`.
# Everything router-specific stays out of this repository:
#   <secrets_env>      SOPS-encrypted dotenv (`just secrets-edit`), decrypted in memory only:
#     ROUTER_BENCH_MACS  the only MACs this script acts on (space or comma separated), so a typo
#                        cannot lock out this machine or the broker
#     ROUTER_*           whatever else the backend needs (host, credentials)
#   ROUTER_BACKEND     required environment variable (e.g. exported in .envrc): the script that talks
#                      to the router; machine-local, called as `<backend> <action> <mac>` inside
#                      `sops exec-env`, so it reads its ROUTER_* values from the environment, never from disk

action="${1:-}"
mac_arg="${2:-}"
secrets_env="${3:-}"

case "$action" in
    block | unblock | status) ;;
    *)
        printf 'Usage: %s <block|unblock|status> <mac> <secrets_env>\n' "$0" >&2
        exit 2
        ;;
esac

mac=$(printf '%s' "$mac_arg" | tr 'A-F-' 'a-f:')
if ! [[ "$mac" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]]; then
    printf 'router: %s is not a MAC address\n' "$mac_arg" >&2
    exit 2
fi

backend="${ROUTER_BACKEND:-}"
if [ -z "$backend" ]; then
    printf 'router: ROUTER_BACKEND is not set; export the path of the router backend script, e.g. in .envrc\n' >&2
    exit 1
fi
if [ ! -x "$backend" ]; then
    printf 'router: ROUTER_BACKEND %s is not an executable file\n' "$backend" >&2
    exit 1
fi

if [ ! -f "$secrets_env" ]; then
    printf 'router: %s is missing; create it with `just secrets-edit`\n' "$secrets_env" >&2
    exit 1
fi
dec="$(sops -d "$secrets_env")"
val() { printf '%s\n' "$dec" | sed -n "s/^$1=//p"; }
allowed=$(val ROUTER_BENCH_MACS | tr 'A-F-' 'a-f:' | tr ',' ' ')
unset dec

case " $allowed " in
    *" $mac "*) ;;
    *)
        printf 'router: %s is not in ROUTER_BENCH_MACS; refusing to touch it\n' "$mac" >&2
        exit 2
        ;;
esac

exec sops exec-env "$secrets_env" "$(printf '%q ' "$backend" "$action" "$mac")"
