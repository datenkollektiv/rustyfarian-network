#!/usr/bin/env bash
set -euo pipefail
# mqtt-topic.sh — resolve a `just mqtt-*` target to a topic prefix (or a filter)
# Usage: scripts/mqtt-topic.sh [target]
#
# Targets:
#   "" | hal   the esp-hal MQTT examples' prefix (default rustyfarian/hal-mqtt)
#   idf        the ESP-IDF LWT example's prefix (default rustyfarian/idf-mqtt)
#   all        every topic on the broker (`#`, watch only)
#   <example>  an MQTT example's name, so a bench probe can pass `{example}`:
#              hal_*_mqtt_lwt as hal, idf_*_mqtt_lwt as idf, idf_c3_mqtt,
#              idf_c6_mqtt and idf_esp32_mqtt as their client id
#   <other>    used as given: a prefix such as a client id (`esp32c3-mqtt`),
#              or a filter when it contains `+` or `#`
#
# MQTT_TOPIC_PREFIX (from the dotenv file) wins for `hal` and `idf`, and
# MQTT_CLIENT_ID for the client-id examples, because the examples read the same
# variables at build time.

target="${1:-}"

case "$target" in
    "" | hal | hal_*_mqtt_lwt) printf '%s' "${MQTT_TOPIC_PREFIX:-rustyfarian/hal-mqtt}" ;;
    idf | idf_*_mqtt_lwt) printf '%s' "${MQTT_TOPIC_PREFIX:-rustyfarian/idf-mqtt}" ;;
    idf_c3_mqtt) printf '%s' "${MQTT_CLIENT_ID:-esp32c3-mqtt}" ;;
    idf_c6_mqtt) printf '%s' "${MQTT_CLIENT_ID:-esp32c6-mqtt}" ;;
    idf_esp32_mqtt) printf '%s' "${MQTT_CLIENT_ID:-esp32-mqtt}" ;;
    all) printf '#' ;;
    *) printf '%s' "${target%/}" ;;
esac
