#!/usr/bin/env bash
set -euo pipefail
# bench-remote-env.sh — run a command with the remote bench settings rustyfarian-bench `submit` reads
# Usage: scripts/bench-remote-env.sh <secrets_env> <command...>
#
# Each setting comes from the environment when already exported there, otherwise from <secrets_env>,
# the SOPS-encrypted dotenv (`just secrets-edit`), decrypted in memory only:
#   RUSTYFARIAN_BENCH_URL      the bench host, e.g. https://bench.example.net:8443
#   RUSTYFARIAN_BENCH_TOKEN    the bearer token; reaches the command through its environment, never argv
#   RUSTYFARIAN_BENCH_CA_B64   optional, the base64 of the private CA certificate (PEM) of the bench;
#                              written to a temporary file for the run, as RUSTYFARIAN_BENCH_CA
#                              (an exported RUSTYFARIAN_BENCH_CA path wins)
# <secrets_env> is decrypted only when the URL or the token is missing; with both exported, the CA
# comes from the environment too (RUSTYFARIAN_BENCH_CA or RUSTYFARIAN_BENCH_CA_B64), or is not used.

secrets_env="${1:-}"
if [ $# -lt 2 ]; then
    printf 'Usage: %s <secrets_env> <command...>\n' "$0" >&2
    exit 2
fi
shift

ca_b64="${RUSTYFARIAN_BENCH_CA_B64:-}"
if [ -z "${RUSTYFARIAN_BENCH_URL:-}" ] || [ -z "${RUSTYFARIAN_BENCH_TOKEN:-}" ]; then
    if [ -f "$secrets_env" ]; then
        dec="$(sops -d "$secrets_env")"
        val() { printf '%s\n' "$dec" | sed -n "s/^$1=//p"; }
        [ -n "${RUSTYFARIAN_BENCH_URL:-}" ] || RUSTYFARIAN_BENCH_URL="$(val RUSTYFARIAN_BENCH_URL)"
        [ -n "${RUSTYFARIAN_BENCH_TOKEN:-}" ] || RUSTYFARIAN_BENCH_TOKEN="$(val RUSTYFARIAN_BENCH_TOKEN)"
        [ -n "$ca_b64" ] || ca_b64="$(val RUSTYFARIAN_BENCH_CA_B64)"
        unset dec
    fi
fi

for key in RUSTYFARIAN_BENCH_URL RUSTYFARIAN_BENCH_TOKEN; do
    if [ -z "${!key:-}" ]; then
        printf 'bench: %s is not set; add it with `just secrets-edit` or export it\n' "$key" >&2
        exit 1
    fi
done
export RUSTYFARIAN_BENCH_URL RUSTYFARIAN_BENCH_TOKEN

# The CA certificate is public, the temporary file only spares a second secrets entry format.
if [ -z "${RUSTYFARIAN_BENCH_CA:-}" ] && [ -n "$ca_b64" ]; then
    ca="$(mktemp)"
    trap 'rm -f "$ca"' EXIT
    printf '%s' "$ca_b64" | base64 -d > "$ca"
    export RUSTYFARIAN_BENCH_CA="$ca"
fi

"$@"
