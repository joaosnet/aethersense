#!/bin/sh
set -eu

load_options_file() {
    if [ ! -f /data/options.json ] || ! command -v jq >/dev/null 2>&1; then
        return 0
    fi

    AETHERSENSE_HOST="$(jq -r '.host // empty' /data/options.json)"
    AETHERSENSE_PORT="$(jq -r '.port // empty' /data/options.json)"
    AETHERSENSE_UDP_PORT="$(jq -r '.udp_port // empty' /data/options.json)"
    AETHERSENSE_API_TOKEN="$(jq -r '.api_token // empty' /data/options.json)"
    AETHERSENSE_MQTT_ENABLED="$(jq -r '.mqtt_enabled // empty' /data/options.json)"
    AETHERSENSE_MQTT_HOST="$(jq -r '.mqtt_host // empty' /data/options.json)"
    AETHERSENSE_MQTT_PORT="$(jq -r '.mqtt_port // empty' /data/options.json)"
    AETHERSENSE_MQTT_USERNAME="$(jq -r '.mqtt_username // empty' /data/options.json)"
    AETHERSENSE_MQTT_PASSWORD="$(jq -r '.mqtt_password // empty' /data/options.json)"
    AETHERSENSE_MQTT_CLIENT_ID="$(jq -r '.mqtt_client_id // empty' /data/options.json)"
    AETHERSENSE_MQTT_BASE_TOPIC="$(jq -r '.mqtt_base_topic // empty' /data/options.json)"
    AETHERSENSE_MQTT_DISCOVERY_PREFIX="$(jq -r '.mqtt_discovery_prefix // empty' /data/options.json)"
    AETHERSENSE_MQTT_RETAIN="$(jq -r '.mqtt_retain // empty' /data/options.json)"
    AETHERSENSE_MQTT_PUBLISH_INTERVAL_MS="$(jq -r '.mqtt_publish_interval_ms // empty' /data/options.json)"
    AETHERSENSE_MQTT_KEEP_ALIVE_SECS="$(jq -r '.mqtt_keep_alive_secs // empty' /data/options.json)"

    export AETHERSENSE_HOST
    export AETHERSENSE_PORT
    export AETHERSENSE_UDP_PORT
    export AETHERSENSE_API_TOKEN
    export AETHERSENSE_MQTT_ENABLED
    export AETHERSENSE_MQTT_HOST
    export AETHERSENSE_MQTT_PORT
    export AETHERSENSE_MQTT_USERNAME
    export AETHERSENSE_MQTT_PASSWORD
    export AETHERSENSE_MQTT_CLIENT_ID
    export AETHERSENSE_MQTT_BASE_TOPIC
    export AETHERSENSE_MQTT_DISCOVERY_PREFIX
    export AETHERSENSE_MQTT_RETAIN
    export AETHERSENSE_MQTT_PUBLISH_INTERVAL_MS
    export AETHERSENSE_MQTT_KEEP_ALIVE_SECS
}

load_mqtt_from_supervisor() {
    if [ "${AETHERSENSE_MQTT_ENABLED:-false}" != "true" ]; then
        return 0
    fi

    if [ -z "${SUPERVISOR_TOKEN:-}" ] || ! command -v curl >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
        return 0
    fi

    if [ -z "${AETHERSENSE_MQTT_HOST:-}" ] || [ -z "${AETHERSENSE_MQTT_USERNAME:-}" ] || [ -z "${AETHERSENSE_MQTT_PASSWORD:-}" ]; then
        mqtt_service="$(curl -fsS -H "Authorization: Bearer ${SUPERVISOR_TOKEN}" http://supervisor/services/mqtt 2>/dev/null || true)"

        if [ -n "$mqtt_service" ]; then
            if [ -z "${AETHERSENSE_MQTT_HOST:-}" ]; then
                AETHERSENSE_MQTT_HOST="$(printf '%s' "$mqtt_service" | jq -r '.. | objects | .host? // empty' | head -n 1)"
            fi

            if [ -z "${AETHERSENSE_MQTT_PORT:-}" ]; then
                AETHERSENSE_MQTT_PORT="$(printf '%s' "$mqtt_service" | jq -r '.. | objects | .port? // empty' | head -n 1)"
            fi

            if [ -z "${AETHERSENSE_MQTT_USERNAME:-}" ]; then
                AETHERSENSE_MQTT_USERNAME="$(printf '%s' "$mqtt_service" | jq -r '.. | objects | .username? // empty' | head -n 1)"
            fi

            if [ -z "${AETHERSENSE_MQTT_PASSWORD:-}" ]; then
                AETHERSENSE_MQTT_PASSWORD="$(printf '%s' "$mqtt_service" | jq -r '.. | objects | .password? // empty' | head -n 1)"
            fi

            export AETHERSENSE_MQTT_HOST
            export AETHERSENSE_MQTT_PORT
            export AETHERSENSE_MQTT_USERNAME
            export AETHERSENSE_MQTT_PASSWORD
        fi
    fi
}

load_options_file
load_mqtt_from_supervisor

exec aethersense-server "$@"
