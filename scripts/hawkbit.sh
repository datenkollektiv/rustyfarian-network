#!/usr/bin/env bash
set -euo pipefail
# hawkbit.sh — run a local Eclipse hawkBit 1.2.0 update server in Docker as the lab DDI server
# Usage: scripts/hawkbit.sh <up|down|status|restart>
#
# This is the lab DDI server behind the `c3/ddi` and `c6/ddi` bench checks. hawkBit 1.2.0 is the fleet DDI profile's
# oracle version (see scripts/ddi-lab.sh). The server runs with hawkBit's built-in default admin account (see the
# hawkBit documentation for the default credentials); that password is not in this file and `up` does not print it.
# The port is bound to 127.0.0.1 on purpose and is not configurable to another address: the device never talks to
# hawkBit directly, the lab proxy (`just ddi-proxy`, scripts/ddi-lab-proxy.py) is the only LAN-facing listener.
#
# Actions:
#   up       Start the server container (idempotent) and wait until its API answers
#   down     Remove the container (idempotent); the in-memory database is lost with it
#   status   Show running, stopped or absent, with the port; when running, the last 3 log lines
#   restart  Down, then up
#
# Environment:
#   LOCAL_HAWKBIT_PORT (default 18090) loopback port of the hawkBit Management and DDI API

action="${1:-}"
port="${LOCAL_HAWKBIT_PORT:-18090}"
hawkbit_version="1.2.0"
image="hawkbit/hawkbit-update-server:$hawkbit_version"
container="ddi-lab-hawkbit"
ready_timeout=180
poll_interval=2

usage() {
    echo "Usage: $0 <up|down|status|restart>" >&2
    exit 2
}

[ -n "$action" ] || usage

case "$action" in
    up | down | status | restart) ;;
    *)
        echo "Error: unknown action '$action'" >&2
        usage
        ;;
esac

# Docker must be installed and its daemon reachable for every action
require_docker() {
    if ! command -v docker >/dev/null 2>&1; then
        echo "Error: docker not found. Install Docker Desktop (or another Docker engine) first." >&2
        exit 1
    fi
    if ! docker info >/dev/null 2>&1; then
        echo "Error: docker daemon not reachable. Start Docker and try again." >&2
        exit 1
    fi
}

# Print running, stopped or absent for the lab container
container_state() {
    local state
    # a failing inspect still prints an empty line, so decide on its exit status, not on its output
    if ! state=$(docker container inspect --format '{{.State.Status}}' "$container" 2>/dev/null); then
        echo "absent"
    elif [ "$state" = "running" ]; then
        echo "running"
    else
        echo "stopped"
    fi
}

# Check if a port is accepting connections on loopback
port_ready() {
    if command -v nc >/dev/null 2>&1; then
        nc -z 127.0.0.1 "$1" 2>/dev/null || return 1
    else
        bash -c "exec 3<>/dev/tcp/127.0.0.1/$1" >/dev/null 2>&1 || return 1
    fi
    return 0
}

# The loopback port the running container actually publishes for hawkBit's 8080, empty when there is none
published_port() {
    docker port "$container" 8080/tcp 2>/dev/null | head -n 1 | sed 's/.*://'
}

# True once the API answers with any HTTP status (a 401 without credentials counts as ready)
api_answers() {
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$port/rest/v1/targets" || true)
    [ -n "$code" ] && [ "$code" != "000" ]
}

print_use() {
    echo "[hawkbit] Use: DDI_MGMT_URL=http://127.0.0.1:$port DDI_MGMT_USER=admin DDI_MGMT_PASS=<hawkBit's default admin password>"
    echo "[hawkbit] Next: just ddi-tenant-init, just ddi-ensure-target, just ddi-publish <example> <version>, then just ddi-proxy"
}

# Start the server
action_up() {
    local state waited
    state=$(container_state)
    if [ "$state" = "running" ]; then
        local actual
        actual=$(published_port)
        if [ "$actual" != "$port" ]; then
            echo "Error: container $container is running on 127.0.0.1:${actual:-<none>}, not on port $port (LOCAL_HAWKBIT_PORT)." >&2
            echo "Run 'just hawkbit restart' to rebind it to $port, or set LOCAL_HAWKBIT_PORT=${actual:-<its port>}." >&2
            exit 1
        fi
        echo "[hawkbit] already running (container $container, listening on 127.0.0.1:$port)"
        print_use
        return 0
    fi

    if [ "$state" = "stopped" ]; then
        echo "[hawkbit] removing stopped container $container"
        docker rm -f "$container" >/dev/null
    fi

    # Check if port is already in use by another process
    if port_ready "$port"; then
        echo "Error: port $port is already in use by another process" >&2
        exit 1
    fi

    if ! docker run -d --name "$container" --platform linux/amd64 \
        -p "127.0.0.1:$port:8080" \
        -e JAVA_TOOL_OPTIONS="-Xms256m -Xmx768m" \
        "$image" >/dev/null; then
        echo "Error: docker run failed for $image" >&2
        exit 1
    fi

    # The amd64 image runs emulated on Apple Silicon and Spring Boot starts slowly, so poll for a while
    printf '[hawkbit] waiting for the API on 127.0.0.1:%s (up to %ss)' "$port" "$ready_timeout"
    waited=0
    while ! api_answers; do
        if [ "$(container_state)" != "running" ]; then
            printf '\n'
            echo "Error: container $container exited during startup. Last log lines:" >&2
            docker logs --tail 20 "$container" >&2 2>&1 || true
            exit 1
        fi
        if [ "$waited" -ge "$ready_timeout" ]; then
            printf '\n'
            echo "Error: hawkBit did not answer on 127.0.0.1:$port within ${ready_timeout}s. Last log lines:" >&2
            docker logs --tail 20 "$container" >&2 2>&1 || true
            exit 1
        fi
        printf '.'
        sleep "$poll_interval"
        waited=$((waited + poll_interval))
    done
    printf '\n'

    echo "[hawkbit] listening on 127.0.0.1:$port (hawkBit $hawkbit_version, in-memory database)"
    print_use
}

# Remove the container
action_down() {
    local state
    state=$(container_state)
    if [ "$state" = "absent" ]; then
        echo "[hawkbit] not running (no container $container)"
        return 0
    fi

    docker rm -f "$container" >/dev/null
    echo "[hawkbit] stopped and removed container $container"
    echo "[hawkbit] the in-memory database is gone: create the targets and published sets again after the next up"
}

# Show status
action_status() {
    local state
    state=$(container_state)
    case "$state" in
        running)
            local actual
            actual=$(published_port)
            echo "[hawkbit] running (container $container, listening on 127.0.0.1:${actual:-<none>}, hawkBit $hawkbit_version)"
            if [ "$actual" != "$port" ]; then
                echo "[hawkbit] warning: LOCAL_HAWKBIT_PORT is $port; 'just hawkbit restart' rebinds the container to it"
            fi
            echo ""
            echo "Recent log entries:"
            docker logs --tail 3 "$container" 2>&1 || true
            ;;
        stopped)
            echo "[hawkbit] stopped (container $container exists but is not running, port $port)"
            ;;
        *)
            echo "[hawkbit] absent (no container $container, port $port)"
            ;;
    esac
}

require_docker

# Main dispatch
case "$action" in
    up)
        action_up
        ;;
    down)
        action_down
        ;;
    status)
        action_status
        ;;
    restart)
        action_down
        action_up
        ;;
esac
