#!/usr/bin/env bash
set -euo pipefail
# ota-mqtt.sh — MQTT operations for OTA hardware tests
# Usage: scripts/ota-mqtt.sh <command> [args...]
#
# Commands:
#   pub <payload>                           Publish raw payload to command topic
#   sub [topic]                             Subscribe to topic with ISO timestamp (default: status)
#   cmd <version>                           Publish manifest command for version
#   cmd-tampered <version> <kind>           Publish tampered manifest (kind: target|checksum)
#   rollback <from>                         Publish rollback action
#   repair                                  Publish repair action
#   burst <count> <kind>                    Burst count commands (kind: repair|invalid)
#
# Environment:
#   MQTT_HOST        (required) Broker host
#   MQTT_PORT        (default 1883) Broker port
#   MQTT_USER        (optional) Username
#   MQTT_PASS        (optional) Password
#   OTA_TOPIC_PREFIX (default rustyfarian/example/ota) Topic prefix
#   OTA_HOST         (auto-detect if unset) OTA server host
#   OTA_PORT         (default 8000) OTA server port

usage() {
    echo "Usage: $0 <command> [args...]" >&2
    exit 2
}

[ $# -ge 1 ] || usage

command="$1"
shift

# Broker settings
mqtt_host="${MQTT_HOST:-}"
mqtt_port="${MQTT_PORT:-1883}"
mqtt_user="${MQTT_USER:-}"
mqtt_pass="${MQTT_PASS:-}"
topic_prefix="${OTA_TOPIC_PREFIX:-rustyfarian/example/ota}"

# OTA server settings
ota_port="${OTA_PORT:-8000}"
ota_host="${OTA_HOST:-}"

# Validate required settings
if [ -z "$mqtt_host" ]; then
    echo "Error: MQTT_HOST is required" >&2
    exit 1
fi

# Auto-detect OTA_HOST if not set
if [ -z "$ota_host" ]; then
    if [ "$(uname)" = "Darwin" ]; then
        # macOS: get default interface and its IP
        default_if=$(route -n get default 2>/dev/null | grep interface | awk '{print $2}' || echo "")
        if [ -n "$default_if" ]; then
            ota_host=$(ipconfig getifaddr "$default_if" 2>/dev/null || echo "")
        fi
    else
        # Linux: use `ip route get`
        ota_host=$(ip route get 1.1.1.1 2>/dev/null | head -1 | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}' || echo "")
    fi

    if [ -z "$ota_host" ]; then
        echo "Error: OTA_HOST could not be auto-detected. Please set it explicitly." >&2
        exit 1
    fi
    echo "[ota-mqtt] Auto-detected OTA_HOST: $ota_host" >&2
fi

# Build mosquitto_pub/sub arguments
mosquitto_args=(-h "$mqtt_host" -p "$mqtt_port")
if [ -n "$mqtt_user" ]; then
    mosquitto_args+=(-u "$mqtt_user")
fi
if [ -n "$mqtt_pass" ]; then
    mosquitto_args+=(-P "$mqtt_pass")
fi

# Publish a raw payload to the command topic (non-retained)
cmd_pub() {
    local payload="$1"
    mosquitto_pub "${mosquitto_args[@]}" -t "$topic_prefix/command" -m "$payload"
}

# Publish to the command topic and ensure it's not retained
cmd_pub_file() {
    local file="$1"
    if [ ! -f "$file" ]; then
        echo "Error: file not found: $file" >&2
        return 1
    fi
    cmd_pub "$(cat "$file")"
}

case "$command" in
    pub)
        [ $# -ge 1 ] || { echo "Error: pub requires <payload>" >&2; exit 1; }
        cmd_pub "$1"
        ;;

    sub)
        topic="${1:-status}"
        mosquitto_sub "${mosquitto_args[@]}" -v -F '%I %t %p' -t "$topic_prefix/$topic"
        ;;

    cmd)
        [ $# -ge 1 ] || { echo "Error: cmd requires <version>" >&2; exit 1; }
        version="$1"
        cmd_pub_file "target/ota/command-v${version}.json"
        ;;

    cmd-tampered)
        [ $# -ge 2 ] || { echo "Error: cmd-tampered requires <version> <kind>" >&2; exit 1; }
        version="$1"
        kind="$2"

        if [ ! -f "target/ota/manifest-v${version}.json" ]; then
            echo "Error: manifest not found: target/ota/manifest-v${version}.json" >&2
            exit 1
        fi

        case "$kind" in
            target)
                # Replace target field
                sed "s/\"esp32c3\"/\"esp32s3\"/" "target/ota/manifest-v${version}.json" > "target/ota/manifest-tampered-${kind}-${version}.json"
                ;;
            checksum)
                # Replace sha256 with all zeros
                sed -E 's/"sha256":"[0-9a-f]+"/"sha256":"0000000000000000000000000000000000000000000000000000000000000000"/' \
                    "target/ota/manifest-v${version}.json" > "target/ota/manifest-tampered-${kind}-${version}.json"
                ;;
            *)
                echo "Error: unknown kind '$kind' (must be target or checksum)" >&2
                exit 1
                ;;
        esac

        # Publish command pointing to tampered manifest
        cmd_json="{\"manifest_url\":\"http://${ota_host}:${ota_port}/manifest-tampered-${kind}-${version}.json\"}"
        cmd_pub "$cmd_json"
        ;;

    rollback)
        [ $# -ge 1 ] || { echo "Error: rollback requires <from>" >&2; exit 1; }
        from="$1"
        cmd_pub "{\"action\":\"rollback\",\"from\":\"${from}\"}"
        ;;

    repair)
        cmd_pub "{\"action\":\"repair\"}"
        ;;

    burst)
        [ $# -ge 2 ] || { echo "Error: burst requires <count> <kind>" >&2; exit 1; }
        count="$1"
        kind="$2"

        case "$kind" in
            repair)
                for _ in $(seq 1 "$count"); do
                    cmd_pub "{\"action\":\"repair\"}"
                done
                ;;
            invalid)
                for _ in $(seq 1 "$count"); do
                    cmd_pub "not-json"
                done
                ;;
            *)
                echo "Error: unknown kind '$kind' (must be repair or invalid)" >&2
                exit 1
                ;;
        esac
        ;;

    *)
        echo "Error: unknown command '$command'" >&2
        usage
        ;;
esac
