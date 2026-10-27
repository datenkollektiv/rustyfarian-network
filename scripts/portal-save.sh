#!/usr/bin/env bash
set -euo pipefail
# portal-save.sh — submit the provisioning form (with x_* extras) to a running SoftAP portal
# Usage: scripts/portal-save.sh <host> <extras> <key_len> <value_len>
#
# Fetches the nonce from GET /, then posts the canonical fields plus <extras>
# extra fields named x_k1.. (exactly <key_len> bytes) with values of exactly
# <value_len> bytes to POST /save. Exits non-zero unless the portal answers 200.
#
# Environment (required): WIFI_SSID, MQTT_HOST
# Environment (optional): WIFI_PASS, MQTT_PORT (1883), MQTT_USER, MQTT_PASS,
#                         MQTT_CLIENT_ID, OTA_URL, DEVICE_NAME (hal-extras-test)
#
# WIFI_PASS and MQTT_PASS go to curl on stdin (--config -), never on argv,
# so they do not show up in ps.

host="${1:-192.168.4.1}"
extras="${2:-2}"
key_len="${3:-6}"
value_len="${4:-8}"

: "${WIFI_SSID:?WIFI_SSID must be set}"
: "${MQTT_HOST:?MQTT_HOST must be set}"

for n in "$extras" "$key_len" "$value_len"; do
    case "$n" in
        '' | *[!0-9]*) echo "portal-save: extras, key_len and value_len must be non-negative integers" >&2; exit 2 ;;
    esac
done

# Keys are x_k<i> padded or truncated to key_len; below 5 bytes they collide
# (x_k1 and x_k10 both become x_k1) and the portal answers 400 Duplicate,
# which would look like a cap rejection.
if [ "$key_len" -lt 5 ]; then
    echo "portal-save: key_len must be at least 5 (shorter keys collide and the portal rejects them as duplicates)" >&2
    exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_dir="${TMPDIR_PROJECT:-${repo_root}/tmp}"
mkdir -p "$tmp_dir"

# curl_quote <value>: escape backslash and double quote for a curl config string.
curl_quote() {
    local v="$1"
    v="${v//\\/\\\\}"
    v="${v//\"/\\\"}"
    printf '%s' "$v"
}

# pad <seed> <len>: seed padded with 'a' or truncated to exactly len bytes.
pad() {
    local s="$1" len="$2"
    while [ "${#s}" -lt "$len" ]; do s="${s}a"; done
    printf '%s' "${s:0:len}"
}

page="$(curl -sS --max-time 10 "http://${host}/")"
nonce="$(printf '%s' "$page" | sed -n 's/.*name="_nonce" value="\([0-9a-f]\{8\}\)".*/\1/p' | head -n 1)"
if [ -z "$nonce" ]; then
    echo "portal-save: no _nonce found on http://${host}/" >&2
    exit 1
fi

args=(
    --data-urlencode "_nonce=${nonce}"
    --data-urlencode "wifi_ssid=${WIFI_SSID}"
    --data-urlencode "mqtt_uri=mqtt://${MQTT_HOST}:${MQTT_PORT:-1883}"
    --data-urlencode "mqtt_user=${MQTT_USER:-}"
    --data-urlencode "mqtt_client=${MQTT_CLIENT_ID:-}"
    --data-urlencode "ota_url=${OTA_URL:-}"
    --data-urlencode "dev_name=${DEVICE_NAME:-hal-extras-test}"
)

i=1
while [ "$i" -le "$extras" ]; do
    key="$(pad "x_k${i}" "$key_len")"
    value="$(pad "v${i}" "$value_len")"
    args+=(--data-urlencode "${key}=${value}")
    i=$((i + 1))
done

echo "portal-save: posting to http://${host}/save (extras=${extras}, key_len=${key_len}, value_len=${value_len})"
out="$(mktemp "${tmp_dir}/portal-save.XXXXXX")"
trap 'rm -f "$out"' EXIT
code="$(curl -sS --max-time 20 -o "$out" -w '%{http_code}' -X POST --config - "${args[@]}" "http://${host}/save" <<CURLCONFIG
data-urlencode = "wifi_pass=$(curl_quote "${WIFI_PASS:-}")"
data-urlencode = "mqtt_pass=$(curl_quote "${MQTT_PASS:-}")"
CURLCONFIG
)"
echo "HTTP ${code}"
head -n 10 "$out"
echo
[ "$code" = "200" ]
