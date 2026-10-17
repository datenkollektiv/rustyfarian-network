#!/usr/bin/env bash
set -euo pipefail
# doctor.sh — check development prerequisites (RAM disk, sccache, tooling, MQTT broker)
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

# --- MQTT broker (optional) — reachability of the broker the idf_*_mqtt examples use ---
# MQTT_HOST / MQTT_PORT come from .env via `set dotenv-load` in the justfile.
# TCP reachability only: credentials are never read and the check is never fatal.
probe_tcp() {
    # $1 host  $2 port — succeeds when a TCP connection opens within ~3 s
    bash -c 'exec 3<>"/dev/tcp/$1/$2"' _ "$1" "$2" >/dev/null 2>&1 &
    local pid=$! rc=0 i
    for i in 1 2 3 4 5 6; do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" || rc=$?
            return "$rc"
        fi
        sleep 0.5
    done
    { kill -9 "$pid" && wait "$pid"; } >/dev/null 2>&1 || true
    return 1
}

mqtt_host="${MQTT_HOST:-}"
mqtt_port="${MQTT_PORT:-1883}"
if [ -z "$mqtt_host" ]; then
    printf "  mqtt broker --       MQTT_HOST not set in .env  (optional, needed for: idf_*_mqtt examples)\n"
elif probe_tcp "$mqtt_host" "$mqtt_port"; then
    printf "  mqtt broker ok       %s:%s\n" "$mqtt_host" "$mqtt_port"
else
    printf "  mqtt broker UNREACHABLE %s:%s  (check MQTT_HOST/MQTT_PORT in .env, is the broker running?)\n" "$mqtt_host" "$mqtt_port"
fi

# --- Hardware MQTT test readiness (runbook docs/runbooks/esp-hal-mqtt-hardware-test.md) ---
# Reports key presence only: secret values are never read or printed.
for key in WIFI_SSID WIFI_PASS MQTT_HOST; do
    if [ -n "${!key:-}" ]; then
        printf "  .env %-10s ok       set\n" "$key"
    else
        printf "  .env %-10s --       not set  (needed for: hal/idf MQTT examples)\n" "$key"
    fi
done

ipv4_re='^([0-9]{1,3}\.){3}[0-9]{1,3}$'
if [ -n "$mqtt_host" ]; then
    if [[ "$mqtt_host" =~ $ipv4_re ]]; then
        printf "  mqtt host  ok       IPv4 literal (esp-hal tier has no DNS, ADR 019)\n"
    else
        printf "  mqtt host  WARN     not an IPv4 literal; esp-hal examples cannot resolve it (ADR 019)\n"
    fi
fi

if pgrep -x mosquitto >/dev/null 2>&1; then
    printf "  mosquitto  ok       local broker running (pid %s)\n" "$(pgrep -x mosquitto | head -1)"
else
    printf "  mosquitto  --       no local broker (start: just mosquitto up)\n"
fi

# A machine on Wi-Fi and Ethernet has two LANs; the board only reaches the broker on the
# LAN its access point serves, so list every candidate and mark MQTT_HOST's subnet.
lan_addrs=""
if [ "$(uname)" = "Darwin" ]; then
    lan_addrs=$(ifconfig 2>/dev/null | awk '/^[a-z0-9]+:/{ifc=$1; sub(":","",ifc)} /inet / && $2 !~ /^127\./ {print ifc, $2}')
elif command -v ip >/dev/null 2>&1; then
    lan_addrs=$(ip -4 -o addr show 2>/dev/null | awk '$4 !~ /^127\./ {split($4,a,"/"); print $2, a[1]}')
fi
host_net="${mqtt_host%.*}"
on_lan=0
if [ -z "$lan_addrs" ]; then
    printf "  lan        --       no IPv4 interface found\n"
else
    while read -r ifc addr; do
        mark=""
        if [ "$addr" = "$mqtt_host" ]; then
            mark="<- MQTT_HOST (this machine)"; on_lan=1
        elif [ -n "$mqtt_host" ] && [ "${addr%.*}" = "$host_net" ]; then
            mark="<- same /24 as MQTT_HOST"; on_lan=1
        fi
        printf "  lan        %-8s %s %s\n" "$ifc" "$addr" "$mark"
    done <<< "$lan_addrs"
    if [ -n "$mqtt_host" ] && [ "$on_lan" -eq 0 ]; then
        printf "  lan        NOTE     MQTT_HOST is on none of these LANs (routed or remote broker); the board's AP must route to it\n"
    fi
    if [ "$(echo "$lan_addrs" | wc -l | tr -d ' ')" -gt 1 ]; then
        printf "  lan        NOTE     several LANs: MQTT_HOST must be reachable from WIFI_SSID's access point\n"
    fi
fi

shopt -s nullglob
esp_ports=(/dev/cu.usbmodem* /dev/cu.usbserial* /dev/cu.wchusbserial* /dev/ttyACM* /dev/ttyUSB*)
shopt -u nullglob
case "${#esp_ports[@]}" in
    0) printf "  board port --       none attached (needed for: just flash, just monitor)\n" ;;
    1) printf "  board port ok       %s\n" "${esp_ports[0]}" ;;
    *) printf "  board port WARN     %s ports; set ESPFLASH_PORT to pick one: %s\n" "${#esp_ports[@]}" "${esp_ports[*]}" ;;
esac

# --- Cargo tooling (verify, dependency hygiene, flashing) ---
# Tiers: required = needed for `just verify`; recommended = release/maintenance;
#        optional = hardware flashing or on-demand checks.
check_cmd() {
    # $1 binary  $2 label  $3 tier(required|recommended|optional)  $4 hint
    if command -v "$1" >/dev/null 2>&1; then
        printf "  %-14s ok       %s\n" "$2" "$("$1" --version 2>/dev/null | head -1)"
    elif [ "$3" = "required" ]; then
        printf "  %-14s MISSING  %s\n" "$2" "$4"
    else
        printf "  %-14s --       %s\n" "$2" "$4"
    fi
}

check_cmd cargo-deny     cargo-deny     required    "cargo install cargo-deny      (needed for: just verify, just deny)"
check_cmd cargo-audit    cargo-audit    recommended "cargo install cargo-audit     (needed for: just audit, release validation)"
check_cmd cargo-machete  cargo-machete  optional    "cargo install cargo-machete   (needed for: just machete)"
check_cmd cargo-outdated cargo-outdated optional    "cargo install cargo-outdated  (needed for: monthly dependency review)"
check_cmd espflash       espflash       optional    "cargo install espflash        (needed for: just flash, just run)"
check_cmd mosquitto_pub  mosquitto_pub  optional    "brew install mosquitto        (needed for: just ota-* runbook commands)"
check_cmd mosquitto_sub  mosquitto_sub  optional    "brew install mosquitto        (needed for: just ota-sub)"
