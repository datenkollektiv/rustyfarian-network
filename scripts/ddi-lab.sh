#!/usr/bin/env bash
set -euo pipefail
# ddi-lab.sh — operator side of the DDI lab: the hawkBit Management API subset the fleet DDI profile 0.4.0 §7
# names as its reference conformance binding (upload a module with a target-visible `chip` entry, create a set,
# assign it forced, cancel an action, read an action's state and last code), plus the bench helpers around it.
# Usage: scripts/ddi-lab.sh <command> [args...]      (every `just ddi-*` recipe is one command)
#
# Commands (the controller id is always DDI_CONTROLLER_ID, the device's own, as `just ddi-bundle` names the bundle):
#   tenant-init                              enable target-token auth and set the 30 s polling interval (tenant keys)
#   ensure-target                            create the target with the token in DDI_TOKEN, or set that token on it
#   target-reset                             delete and recreate the target with DDI_TOKEN, so the server forgets the
#                                            installed set of the last run (hawkBit creates no action for a set that is
#                                            installed or running); on a fleet-owned target use quiesce and new versions
#   upload <example> <version> [--tampered] [--downgrade-allowed]
#                                            software module (type os) + artifact + metadata from the bundle
#                                            tmp/ddi-bundle/<example>-<id>-v<version>.bin and its sidecars; prints module=<id>
#   set <example> <version> [--tampered]     distribution set (type os) from that module; prints set=<id>
#   publish <example> <version> [flags]      upload, then set
#   assign <example> <version> [--tampered] [--nudge]
#                                            forced assignment of the set to the target; prints action=<id>
#   cancel [action-id] [--force] [--nudge]   cancel the action (default: the newest open one); --force also force-quits
#   quiesce                                  cancel and force-quit every open action of the target
#   action [action-id]                       one line: action=<id> type=<type> status=<status> code=<n|none> detail=<text|none>
#   watch [action-id]                        that line whenever it changes, polled every 2 s, until killed (a bench probe)
#   nudge                                    publish the MQTT poll-now nudge on rustyfarian/ota/<id>/nudge
#   proxy                                    run the lab proxy the device's DDI_BASE_URL points at (scripts/ddi-lab-proxy.py)
#   proxy-wait                               wait up to 10 s until the proxy answers on loopback (a bench setup step)
#
# `--tampered` names the module, the set and the artifact `<bundle>-tampered`; the lab proxy flips one byte of such an
# artifact, so the device reports 1002 checksum_mismatch (the server itself verifies every upload). `--downgrade-allowed`
# adds the target-visible metadata entry `downgrade=allowed` (profile §6).
#
# Environment:
#   DDI_MGMT_URL         (required) origin of the lab server's Management API, e.g. http://<lab-host>:<port>
#   DDI_MGMT_USER        (required) operator user
#   DDI_MGMT_PASS        (required) operator password; sent to curl on stdin, never on argv, never printed
#   DDI_CONTROLLER_ID    (required) the device id, ^[a-z0-9][a-z0-9-]{2,19}$
#   DDI_TOKEN            (ensure-target, target-reset) the target token, 32 to 64 lowercase hex; never printed
#   MQTT_HOST, MQTT_PORT, MQTT_USER, MQTT_PASS   (nudge) the broker; the nudge is the only MQTT traffic here
#   DDI_LAB_PROXY_PORT   (proxy, default 8090) the port the device's DDI_BASE_URL names
#   DDI_LAB_UPSTREAM     (proxy, default: the origin of DDI_MGMT_URL) where the proxy forwards to
#   DDI_LAB_PROXY_RATE   (proxy, default 20480) artifact bytes per second, 0 = full speed
# No credential has a default and none is printed; HTTP basic auth over plain HTTP is a lab-only setting.
#
# Backend assumption: the lab server is hawkBit 1.2.0 (the profile's oracle) or a Management-API-compatible server
# such as the fleet's own. Only these calls are used: PUT /system/configs/{key}, POST/PUT/DELETE /targets,
# GET/POST /softwaremodules (+ /artifacts, /metadata), GET/POST /distributionsets, POST /targets/{id}/assignedDS,
# GET /targets/{id}/actions[/{aid}[/status]], DELETE /targets/{id}/actions/{aid}[?force=true]. The lab backend is
# still open on the fleet side; a server that lacks a tenant key answers tenant-init with 400, which is reported and
# tolerated. Dry-run on 2026-11-22 against hawkBit 1.2.0 on loopback.

usage() {
    sed -n '3,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

die() {
    printf 'Error: %s\n' "$1" >&2
    exit 1
}

[ $# -ge 1 ] || usage
command="$1"
shift

workspace_root="$(cd "$(dirname "$0")/.." && pwd)"
bundle_dir="$workspace_root/tmp/ddi-bundle"
controller_id="${DDI_CONTROLLER_ID:-}"
id_re='^[a-z0-9][a-z0-9-]{2,19}$'
version_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
# An open action: hawkBit's `active` flag, or (a server without it) a status that is not terminal.
open_filter='select(.active // ((.status // "") | test("^(finished|error|canceled)$") | not))'

command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null || die "jq is required (brew install jq)"

require_controller() {
    [ -n "$controller_id" ] || die "DDI_CONTROLLER_ID must be set (in .env or exported)"
    [[ "$controller_id" =~ $id_re ]] || die "DDI_CONTROLLER_ID must match $id_re"
}

mgmt_url="${DDI_MGMT_URL:-}"
mgmt_user="${DDI_MGMT_USER:-}"
mgmt_pass="${DDI_MGMT_PASS:-}"
api="${mgmt_url%/}/rest/v1"

require_mgmt() {
    if [ -z "$mgmt_url" ] || [ -z "$mgmt_user" ] || [ -z "$mgmt_pass" ]; then
        die "DDI_MGMT_URL, DDI_MGMT_USER and DDI_MGMT_PASS must be set (they have no default)"
    fi
    case "$mgmt_url" in
        http://*|https://*) ;;
        *) die "DDI_MGMT_URL must be an origin such as http://<lab-host>:<port>" ;;
    esac
}

# curl_quote <value>: escape backslash and double quote for a curl config string.
curl_quote() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# mgmt <method> <path> [curl args...]: one Management API request. Sets http_status and http_body.
# The credentials go to curl on stdin (--config -), so they never appear on argv.
http_status=""
http_body=""
mgmt() {
    local method="$1" path="$2"
    shift 2
    local out
    out="$(curl -sS --max-time 120 -X "$method" --config - -w '\n%{http_code}' "$@" "$api$path" <<CURLCONFIG
user = "$(curl_quote "$mgmt_user:$mgmt_pass")"
CURLCONFIG
)" || die "request $method $path failed (is DDI_MGMT_URL reachable?)"
    http_status="${out##*$'\n'}"
    http_body="${out%$'\n'*}"
}

# mgmt_json <method> <path> <json> [curl args...]: the body reaches curl through file descriptor 3, never argv
# (a target body carries the token).
mgmt_json() {
    local method="$1" path="$2" json="$3"
    shift 3
    mgmt "$method" "$path" -H 'Content-Type: application/json' --data-binary @/dev/fd/3 "$@" 3<<<"$json"
}

# expect_status <regex> <what>: fails with the server's message when the last status does not match.
expect_status() {
    local regex="$1" what="$2"
    if ! [[ "$http_status" =~ ^($regex)$ ]]; then
        local message
        message="$(printf '%s' "$http_body" | jq -r '.message // .error // empty' 2>/dev/null | head -c 300 || true)"
        die "$what answered HTTP $http_status${message:+: $message}"
    fi
}

check_version() {
    [[ "$1" =~ $version_re ]] || die "version must be MAJOR.MINOR.PATCH (decimal, no leading zeros), got \"$1\""
}

check_example() {
    case "$1" in
        idf_*_*) ;;
        *) die "example must be idf_{c3|c6|esp32|esp32s3}_{feature}, got \"$1\"" ;;
    esac
}

# bundle_name <example> <version> [variant]: the name `just ddi-bundle` gives the bundle, plus the variant suffix.
bundle_name() {
    local name="$1-$controller_id-v$2"
    [ -z "${3:-}" ] || name="$name-$3"
    printf '%s' "$name"
}

# parse_flags <allowed...> -- <args...>: sets flag_tampered, flag_downgrade, flag_nudge, flag_force; refuses others.
parse_flags() {
    local allowed=" "
    while [ $# -gt 0 ] && [ "$1" != "--" ]; do
        allowed="$allowed$1 "
        shift
    done
    [ $# -gt 0 ] && shift
    flag_tampered=""
    flag_downgrade=""
    flag_nudge=""
    flag_force=""
    for arg in "$@"; do
        case "$allowed" in *" $arg "*) ;; *) die "unknown option \"$arg\"" ;; esac
        case "$arg" in
            --tampered) flag_tampered=1 ;;
            --downgrade-allowed) flag_downgrade=1 ;;
            --nudge) flag_nudge=1 ;;
            --force) flag_force=1 ;;
        esac
    done
}

variant_of() {
    [ -z "${flag_tampered:-}" ] || printf 'tampered'
}

# find_module <name> <version>: prints the module id or nothing.
find_module() {
    mgmt GET "/softwaremodules" -G --data-urlencode "q=name==$1;version==$2"
    expect_status '200' "module lookup"
    printf '%s' "$http_body" | jq -r '.content[0].id // empty'
}

# find_set <name> <version>: prints the set id or nothing.
find_set() {
    mgmt GET "/distributionsets" -G --data-urlencode "q=name==$1;version==$2"
    expect_status '200' "set lookup"
    printf '%s' "$http_body" | jq -r '.content[0].id // empty'
}

# newest_action [open]: prints the id of the target's newest (open) action or nothing.
newest_action() {
    mgmt GET "/targets/$controller_id/actions?sort=id:DESC&limit=50"
    expect_status '200' "action list"
    if [ "${1:-}" = "open" ]; then
        printf '%s' "$http_body" | jq -r "[.content[]? | $open_filter][0].id // empty"
    else
        printf '%s' "$http_body" | jq -r '.content[0].id // empty'
    fi
}

check_action_id() {
    [[ "$1" =~ ^[0-9]+$ ]] || die "action id must be a number, got \"$1\""
}

# action_line <action-id>: prints `action=… type=… status=… code=… detail=…`; returns 1 (with the reason on stderr)
# when the action is unreadable. `status` is the action's state: hawkBit keeps it in `detailStatus` (finished, error,
# canceled, canceling, running, …) and sets `status` only to pending or finished, so detailStatus wins when present.
action_line() {
    local aid="$1"
    mgmt GET "/targets/$controller_id/actions/$aid"
    if [ "$http_status" != "200" ]; then
        printf 'action %s of %s answered HTTP %s\n' "$aid" "$controller_id" "$http_status" >&2
        return 1
    fi
    local head
    head="$(printf '%s' "$http_body" | jq -r '"action=\(.id) type=\(.type // "none") status=\(.detailStatus // .status // "none") code=\(.lastStatusCode // "none")"')"
    mgmt GET "/targets/$controller_id/actions/$aid/status?sort=id:DESC&limit=20"
    if [ "$http_status" != "200" ]; then
        printf 'status list of action %s answered HTTP %s\n' "$aid" "$http_status" >&2
        return 1
    fi
    local detail
    detail="$(printf '%s' "$http_body" | jq -r '[.content[]? | select(.code != null)][0].messages[0] // "none"' | tr -s ' \n\r\t' ' ' | sed 's/ *$//')"
    printf '%s detail=%s\n' "$head" "$detail"
}

# require_nudge: checked before the server is touched, so a failing nudge never leaves an action behind.
require_nudge() {
    [ -n "${MQTT_HOST:-}" ] || die "MQTT_HOST is required for --nudge"
    command -v mosquitto_pub >/dev/null || die "mosquitto_pub is required for --nudge"
}

publish_nudge() {
    require_nudge
    local args=(-h "$MQTT_HOST" -p "${MQTT_PORT:-1883}") cfg_dir="" rc=0
    [ -z "${MQTT_USER:-}" ] || args+=(-u "$MQTT_USER")
    if [ -n "${MQTT_PASS:-}" ]; then
        # The password goes into a private options file that mosquitto_pub reads from $XDG_CONFIG_HOME, never onto
        # argv; printf is a shell builtin, so it does not show in a process listing either. The file format splits
        # on spaces, so a password with whitespace is refused rather than mangled.
        [[ "$MQTT_PASS" =~ [[:space:]] ]] && die "MQTT_PASS with whitespace cannot be passed to mosquitto_pub safely"
        # The file lives under the repository's tmp/ and is removed on return, interrupt or termination.
        mkdir -p "$workspace_root/tmp"
        cfg_dir="$(mktemp -d "$workspace_root/tmp/ddi-nudge.XXXXXX")"
        chmod 700 "$cfg_dir"
        nudge_cfg_dir="$cfg_dir"
        trap 'rm -rf "$nudge_cfg_dir"' EXIT
        trap 'rm -rf "$nudge_cfg_dir"; exit 130' INT
        trap 'rm -rf "$nudge_cfg_dir"; exit 143' TERM
        printf -- '-P %s\n' "$MQTT_PASS" > "$cfg_dir/mosquitto_pub"
    fi
    # The explicit return matters: `set -e` is off inside a `||` list, where nudge_after calls this.
    XDG_CONFIG_HOME="${cfg_dir:-${XDG_CONFIG_HOME:-}}" \
        mosquitto_pub "${args[@]}" -t "rustyfarian/ota/$controller_id/nudge" -m 1 || rc=$?
    if [ -n "$cfg_dir" ]; then
        rm -rf "$cfg_dir"
        trap - EXIT INT TERM
    fi
    [ "$rc" -eq 0 ] || return 1
    echo "nudge published for $controller_id"
}

# nudge_after <what>: the nudge only shortens the wait after an operator action that already happened on the
# server, so a failed publish is a warning (the device polls on its own within 30 s), never a failed action.
nudge_after() {
    publish_nudge || echo "warning: the nudge after the $1 was not published; the device polls on its own within 30 s" >&2
}

cmd_tenant_init() {
    require_mgmt
    local key value
    for pair in "authentication.targettoken.enabled=true" "pollingTime=\"00:00:30\""; do
        key="${pair%%=*}"
        value="${pair#*=}"
        mgmt_json PUT "/system/configs/$key" "{\"value\":$value}"
        case "$http_status" in
            200|204) echo "tenant $key set to $value" ;;
            400)
                # A 400 is either a key the server does not offer or a value it rejects; only the first may pass.
                mgmt GET "/system/configs/$key"
                if [ "$http_status" = "200" ]; then
                    die "tenant config $key exists but the server rejected the value $value (HTTP 400)"
                fi
                echo "tenant key $key is not offered by this server (HTTP 400); left as is" >&2
                ;;
            *) expect_status '200|204' "tenant config $key" ;;
        esac
    done
}

token_json() {
    local token="${DDI_TOKEN:-}"
    [[ "$token" =~ ^[0-9a-f]{32,64}$ ]] || die "DDI_TOKEN must be 32 to 64 lowercase hex characters (set it in .env)"
    printf '%s' "$token" | jq -R '{securityToken: .}'
}

create_target() {
    local body
    body="$(token_json | jq -c --arg id "$controller_id" '[{controllerId: $id, name: $id} + .]')"
    mgmt_json POST "/targets" "$body"
}

cmd_ensure_target() {
    require_mgmt
    require_controller
    create_target
    case "$http_status" in
        200|201) echo "target $controller_id created" ;;
        409)
            mgmt_json PUT "/targets/$controller_id" "$(token_json | jq -c .)"
            expect_status '200' "token update of target $controller_id"
            echo "target $controller_id exists; token set"
            ;;
        *) expect_status '200|201' "target creation" ;;
    esac
}

cmd_target_reset() {
    require_mgmt
    require_controller
    # Checked before the delete, so a bad token never leaves the target deleted.
    token_json >/dev/null
    mgmt DELETE "/targets/$controller_id"
    case "$http_status" in
        200|204) echo "target $controller_id deleted" ;;
        404) ;;
        *) expect_status '200|204|404' "target deletion" ;;
    esac
    create_target
    expect_status '200|201' "target creation"
    echo "target $controller_id created with the token from DDI_TOKEN"
}

cmd_upload() {
    [ $# -ge 2 ] || die "upload needs <example> <version>"
    local example="$1" version="$2"
    shift 2
    parse_flags --tampered --downgrade-allowed -- "$@"
    require_mgmt
    require_controller
    check_example "$example"
    check_version "$version"
    local bundle bin sha256 chip name
    bundle="$(bundle_name "$example" "$version")"
    bin="$bundle_dir/$bundle.bin"
    for f in "$bin" "$bin.sha256" "$bin.version" "$bin.chip"; do
        [ -f "$f" ] || die "$f is missing; run: just ddi-bundle $example $version"
    done
    sha256="$(tr -d ' \n' < "$bin.sha256")"
    [[ "$sha256" =~ ^[0-9a-f]{64}$ ]] || die "$bin.sha256 is not 64 lowercase hex characters"
    [ "$(tr -d ' \n' < "$bin.version")" = "$version" ] || die "$bin.version does not say $version"
    chip="$(tr -d ' \n' < "$bin.chip")"
    [[ "$chip" =~ ^[a-z0-9]{1,32}$ ]] || die "$bin.chip is not a chip name"
    name="$(bundle_name "$example" "$version" "$(variant_of)")"

    local sm
    sm="$(find_module "$name" "$version")"
    if [ -n "$sm" ]; then
        complete_module "$sm" "$name" "$bin" "$sha256" "$chip"
        echo "module=$sm"
        return 0
    fi
    mgmt_json POST "/softwaremodules" "$(jq -nc --arg n "$name" --arg v "$version" '[{name: $n, version: $v, type: "os", vendor: "rustyfarian"}]')"
    expect_status '200|201' "module creation"
    sm="$(printf '%s' "$http_body" | jq -r '.[0].id')"
    upload_artifact "$sm" "$name" "$bin" "$sha256"
    add_metadata "$sm" "$chip" "${flag_downgrade:+allowed}"
    echo "uploaded $name.bin ($(wc -c < "$bin" | tr -d ' ') bytes, chip $chip${flag_downgrade:+, downgrade=allowed}${flag_tampered:+, tampered by the proxy})" >&2
    echo "module=$sm"
}

# upload_artifact <module-id> <name> <bin> <sha256>: the one artifact of the module, verified by the server against the sidecar digest.
upload_artifact() {
    local sm="$1" name="$2" bin="$3" sha256="$4"
    mgmt POST "/softwaremodules/$sm/artifacts" -F "file=@$bin;filename=$name.bin" -F "sha256sum=$sha256"
    expect_status '200|201' "artifact upload"
    local served
    served="$(printf '%s' "$http_body" | jq -r '.hashes.sha256 // empty')"
    [ -z "$served" ] || [ "$served" = "$sha256" ] || die "the server computed sha256 $served for $name.bin, the sidecar says $sha256"
}

# add_metadata <module-id> <chip> [downgrade-value]: the target-visible entries of the profile.
add_metadata() {
    local sm="$1" chip="$2" dg="${3:-}"
    local metadata
    metadata="$(jq -nc --arg chip "$chip" --arg dg "$dg" \
        '(if $chip == "" then [] else [{key: "chip", value: $chip, targetVisible: true}] end) + (if $dg == "" then [] else [{key: "downgrade", value: $dg, targetVisible: true}] end)')"
    [ "$metadata" != "[]" ] || return 0
    mgmt_json POST "/softwaremodules/$sm/metadata" "$metadata"
    expect_status '200|201' "module metadata"
}

# complete_module <module-id> <name> <bin> <sha256> <chip>: an existing module (an earlier run, maybe interrupted) must
# hold this bundle's artifact and its chip entry; a missing artifact or entry is added, a different one is refused,
# because a rebuilt image needs a new version.
complete_module() {
    local sm="$1" name="$2" bin="$3" sha256="$4" chip="$5"
    local items='(if type == "array" then . else (.content // []) end)'
    mgmt GET "/softwaremodules/$sm/artifacts"
    expect_status '200' "artifact list of module $name"
    local served
    served="$(printf '%s' "$http_body" | jq -r "$items | .[0].hashes.sha256 // empty")"
    if [ -z "$served" ]; then
        upload_artifact "$sm" "$name" "$bin" "$sha256"
        echo "module $name exists; its missing artifact was uploaded" >&2
    elif [ "$served" != "$sha256" ]; then
        die "module $name holds an artifact with sha256 $served, the bundle has $sha256; a rebuilt image needs a new version"
    fi
    mgmt GET "/softwaremodules/$sm/metadata"
    expect_status '200' "metadata of module $name"
    local have_chip have_dg
    have_chip="$(printf '%s' "$http_body" | jq -r "$items | map(select(.key == \"chip\")) | .[0].value // empty")"
    have_dg="$(printf '%s' "$http_body" | jq -r "$items | map(select(.key == \"downgrade\")) | .[0].value // empty")"
    if [ -n "$have_chip" ] && [ "$have_chip" != "$chip" ]; then
        die "module $name carries chip $have_chip, the bundle says $chip; a rebuilt image needs a new version"
    fi
    local want_chip="" want_dg=""
    [ -n "$have_chip" ] || want_chip="$chip"
    [ -z "$flag_downgrade" ] || [ -n "$have_dg" ] || want_dg="allowed"
    add_metadata "$sm" "$want_chip" "$want_dg"
    if [ -n "$have_dg" ] && [ -z "$flag_downgrade" ]; then
        echo "module $name already carries downgrade=$have_dg" >&2
    fi
    echo "module $name exists with this bundle's artifact; nothing else uploaded" >&2
}

cmd_set() {
    [ $# -ge 2 ] || die "set needs <example> <version>"
    local example="$1" version="$2"
    shift 2
    parse_flags --tampered -- "$@"
    require_mgmt
    require_controller
    check_example "$example"
    check_version "$version"
    local name ds sm
    name="$(bundle_name "$example" "$version" "$(variant_of)")"
    ds="$(find_set "$name" "$version")"
    if [ -n "$ds" ]; then
        echo "set $name $version exists" >&2
        echo "set=$ds"
        return 0
    fi
    sm="$(find_module "$name" "$version")"
    [ -n "$sm" ] || die "module $name $version is not on the server; run: just ddi-upload $example $version${flag_tampered:+ --tampered}"
    mgmt_json POST "/distributionsets" "$(jq -nc --arg n "$name" --arg v "$version" --argjson sm "$sm" \
        '[{name: $n, version: $v, type: "os", modules: [{id: $sm}], requiredMigrationStep: false}]')"
    expect_status '200|201' "set creation"
    ds="$(printf '%s' "$http_body" | jq -r '.[0].id')"
    echo "set=$ds"
}

cmd_publish() {
    [ $# -ge 2 ] || die "publish needs <example> <version>"
    local example="$1" version="$2"
    shift 2
    parse_flags --tampered --downgrade-allowed -- "$@"
    cmd_upload "$example" "$version" "$@"
    cmd_set "$example" "$version" ${flag_tampered:+--tampered}
}

cmd_assign() {
    [ $# -ge 2 ] || die "assign needs <example> <version>"
    local example="$1" version="$2"
    shift 2
    parse_flags --tampered --nudge -- "$@"
    require_mgmt
    require_controller
    [ -z "$flag_nudge" ] || require_nudge
    check_example "$example"
    check_version "$version"
    local name ds
    name="$(bundle_name "$example" "$version" "$(variant_of)")"
    ds="$(find_set "$name" "$version")"
    [ -n "$ds" ] || die "set $name $version is not on the server; run: just ddi-publish $example $version${flag_tampered:+ --tampered}"
    mgmt_json POST "/targets/$controller_id/assignedDS" "[{\"id\":$ds,\"type\":\"forced\"}]"
    expect_status '200|201' "assignment of $name to $controller_id"
    local aid
    aid="$(printf '%s' "$http_body" | jq -r '.assignedActions[0].id // empty')"
    if [ -z "$aid" ]; then
        die "the server created no action for $name (already assigned: $(printf '%s' "$http_body" | jq -r '.alreadyAssigned // "?"')); a set that is installed or running is never offered again: use another version or just ddi-target-reset"
    fi
    echo "assigned $name (set $ds) to $controller_id, forced" >&2
    echo "action=$aid"
    [ -z "$flag_nudge" ] || nudge_after assignment
}

# cancel_one <action-id> <force>: a plain cancel is refused (405) once an action is canceling, so that step is skipped
# then and only the force-quit remains.
cancel_one() {
    local aid="$1" force="$2" status
    mgmt GET "/targets/$controller_id/actions/$aid"
    expect_status '200' "action $aid"
    status="$(printf '%s' "$http_body" | jq -r '.detailStatus // .status // "none"')"
    if [ "$status" = "canceling" ]; then
        echo "action $aid: cancel already requested, the device has not confirmed it"
    else
        mgmt DELETE "/targets/$controller_id/actions/$aid"
        expect_status '200|204' "cancel of action $aid ($status)"
        echo "action $aid: cancel requested"
    fi
    if [ -n "$force" ]; then
        mgmt DELETE "/targets/$controller_id/actions/$aid?force=true"
        expect_status '200|204' "force quit of action $aid"
        echo "action $aid: force-quit"
    fi
}

cmd_cancel() {
    local aid=""
    if [ $# -ge 1 ] && [[ "$1" != --* ]]; then
        aid="$1"
        shift
    fi
    parse_flags --force --nudge -- "$@"
    require_mgmt
    require_controller
    [ -z "$flag_nudge" ] || require_nudge
    if [ -z "$aid" ]; then
        aid="$(newest_action open)"
        [ -n "$aid" ] || die "no open action on $controller_id"
    fi
    check_action_id "$aid"
    cancel_one "$aid" "$flag_force"
    [ -z "$flag_nudge" ] || nudge_after cancel
}

cmd_quiesce() {
    require_mgmt
    require_controller
    mgmt GET "/targets/$controller_id/actions?sort=id:DESC&limit=50"
    case "$http_status" in
        200) ;;
        404) echo "target $controller_id does not exist; nothing to quiesce"; return 0 ;;
        *) expect_status '200' "action list" ;;
    esac
    local open
    open="$(printf '%s' "$http_body" | jq -r ".content[]? | $open_filter | .id")"
    if [ -z "$open" ]; then
        echo "no open action on $controller_id"
        return 0
    fi
    for aid in $open; do
        cancel_one "$aid" 1
    done
}

cmd_action() {
    require_mgmt
    require_controller
    local aid="${1:-}"
    if [ -z "$aid" ]; then
        aid="$(newest_action)"
        [ -n "$aid" ] || die "no action on $controller_id"
    fi
    check_action_id "$aid"
    action_line "$aid" || exit 1
}

cmd_watch() {
    require_mgmt
    require_controller
    local aid="${1:-}" last="" line
    if [ -z "$aid" ]; then
        aid="$(newest_action)"
        [ -n "$aid" ] || die "no action on $controller_id"
    fi
    check_action_id "$aid"
    # A bench probe: it never exits on its own, prints only changes on stdout, flushes every line and keeps
    # a failed read (its reason goes to stderr) for the next round.
    while true; do
        if line="$(action_line "$aid")"; then
            if [ "$line" != "$last" ]; then
                printf '%s\n' "$line"
                last="$line"
            fi
        else
            printf 'action %s: read failed; retrying in 2 s\n' "$aid" >&2
        fi
        sleep 2
    done
}

cmd_proxy() {
    # The proxy needs the lab server's origin only, never the Management credentials.
    local port="${DDI_LAB_PROXY_PORT:-8090}" upstream="${DDI_LAB_UPSTREAM:-$mgmt_url}" rate="${DDI_LAB_PROXY_RATE:-20480}"
    [ -n "$upstream" ] || die "DDI_LAB_UPSTREAM or DDI_MGMT_URL must name the lab server (http://<lab-host>:<port>)"
    upstream="${upstream%/}"
    [[ "$upstream" =~ ^http://[^/]+$ ]] || die "the proxy upstream must be a plain http://<lab-host>:<port> origin (lab only)"
    [[ "$port" =~ ^[0-9]+$ ]] || die "DDI_LAB_PROXY_PORT must be a port number"
    [[ "$rate" =~ ^[0-9]+$ ]] || die "DDI_LAB_PROXY_RATE must be bytes per second (0 = full speed)"
    command -v python3 >/dev/null || die "python3 is required for the proxy"
    exec python3 -u "$workspace_root/scripts/ddi-lab-proxy.py" "$port" "$upstream" "$rate"
}

cmd_proxy_wait() {
    local port="${DDI_LAB_PROXY_PORT:-8090}" code
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        code="$(curl -s -o /dev/null --max-time 2 -w '%{http_code}' "http://127.0.0.1:$port/" || true)"
        if [ "$code" != "000" ] && [ -n "$code" ]; then
            echo "proxy answers on 127.0.0.1:$port (HTTP $code)"
            return 0
        fi
        sleep 1
    done
    die "the proxy does not answer on 127.0.0.1:$port after 10 s (see tmp/bench-ddi-proxy.log)"
}

case "$command" in
    tenant-init) cmd_tenant_init "$@" ;;
    ensure-target) cmd_ensure_target "$@" ;;
    target-reset) cmd_target_reset "$@" ;;
    upload) cmd_upload "$@" ;;
    set) cmd_set "$@" ;;
    publish) cmd_publish "$@" ;;
    assign) cmd_assign "$@" ;;
    cancel) cmd_cancel "$@" ;;
    quiesce) cmd_quiesce "$@" ;;
    action) cmd_action "$@" ;;
    watch) cmd_watch "$@" ;;
    nudge) require_controller; publish_nudge ;;
    proxy) cmd_proxy "$@" ;;
    proxy-wait) cmd_proxy_wait "$@" ;;
    -h|--help|help) usage ;;
    *)
        printf 'Error: unknown command "%s"\n' "$command" >&2
        usage
        ;;
esac
