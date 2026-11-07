#!/usr/bin/env bash
set -euo pipefail
# mqtt-topic.sh — resolve a `just mqtt-*` target to a topic prefix (or a filter)
# Usage: scripts/mqtt-topic.sh [target]
#
# Targets:
#   "" | hal   the esp-hal MQTT examples' prefix (default rustyfarian/hal-mqtt)
#   idf        the ESP-IDF LWT example's prefix (default rustyfarian/idf-mqtt)
#   all        every topic on the broker (`#`, watch only)
#   <other>    used as given: a prefix such as a client id (`esp32c3-mqtt`),
#              or a filter when it contains `+` or `#`
#
# MQTT_TOPIC_PREFIX (from the dotenv file) wins for `hal` and `idf`, because the
# examples read the same variable at build time.

target="${1:-}"

case "$target" in
    "" | hal) printf '%s' "${MQTT_TOPIC_PREFIX:-rustyfarian/hal-mqtt}" ;;
    idf) printf '%s' "${MQTT_TOPIC_PREFIX:-rustyfarian/idf-mqtt}" ;;
    all) printf '#' ;;
    *) printf '%s' "${target%/}" ;;
esac
