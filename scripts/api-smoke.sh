#!/usr/bin/env bash
# WP-52 manual smoke test (UI §7): import → precheck → create → live logs →
# status → delete, driven entirely through the HTTP API with curl.
#
# This needs a real tier1 cluster (the create/delete steps touch the nodes),
# so it is a maintainer-run script, not part of `just check`.
#
# Usage:
#   scripts/api-smoke.sh <tier1-config.toml>
#
# Environment:
#   KURIN_BIN        binary to run (default: target/debug/kurin; build with
#                    `cargo build -p kurin --features ui`)
#   KURIN_ADDR       listen address (default: 127.0.0.1:7080)
#   KURIN_DATA_DIR   server data dir (default: a fresh temp dir)
#   KURIN_KEEP=1     keep the data dir and server log on exit
set -euo pipefail

CONFIG="${1:?usage: $0 <tier1-config.toml>}"
KURIN_BIN="${KURIN_BIN:-target/debug/kurin}"
ADDR="${KURIN_ADDR:-127.0.0.1:7080}"
DATA_DIR="${KURIN_DATA_DIR:-$(mktemp -d)}"
BASE="http://${ADDR}"
COOKIE_JAR="$(mktemp)"
SERVER_LOG="$(mktemp)"
SERVER_PID=""

cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ "${KURIN_KEEP:-0}" != "1" ]]; then
        rm -rf "$DATA_DIR" "$COOKIE_JAR" "$SERVER_LOG"
    else
        echo "[smoke] data dir: $DATA_DIR (server log: $SERVER_LOG)"
    fi
}
trap cleanup EXIT

json_str() {
    python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
}

json_field() {
    python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$1"
}

cluster_name() {
    python3 - "$CONFIG" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle:
    print(tomllib.load(handle)["name"])
PY
}

api() {
    local method="$1" path="$2"
    shift 2
    curl -sS -b "$COOKIE_JAR" -c "$COOKIE_JAR" -X "$method" \
        -H 'Content-Type: application/json' "$BASE$path" "$@"
}

wait_operation() {
    local id="$1" timeout="${2:-3600}" elapsed=0 state
    while (( elapsed < timeout )); do
        state="$(api GET "/api/v1/operations/$id" | json_field state)"
        case "$state" in
            succeeded) echo "[smoke] operation $id succeeded"; return 0 ;;
            failed|cancelled|interrupted)
                echo "[smoke] operation $id ended: $state" >&2
                api GET "/api/v1/operations/$id" >&2
                return 1 ;;
        esac
        sleep 2
        elapsed=$((elapsed + 2))
    done
    echo "[smoke] operation $id timed out" >&2
    return 1
}

submit() {
    local name="$1" body="$2"
    api POST "/api/v1/clusters/$name/operations" -d "$body" | json_field id
}

echo "[smoke] starting $KURIN_BIN serve on $ADDR (data: $DATA_DIR)"
"$KURIN_BIN" serve --listen "$ADDR" --data-dir "$DATA_DIR" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 100); do
    if curl -sS -o /dev/null "$BASE/healthz"; then break; fi
    sleep 0.1
done
curl -sS -o /dev/null "$BASE/healthz" || {
    echo "[smoke] server did not start; see $SERVER_LOG" >&2
    cat "$SERVER_LOG" >&2
    exit 1
}

TOKEN="$(tr -d '\n' <"$DATA_DIR/token")"
api POST /api/v1/session -d "{\"token\": $(printf '%s' "$TOKEN" | json_str)}" >/dev/null

NAME="$(cluster_name)"
echo "[smoke] importing $CONFIG as cluster $NAME"
api POST /api/v1/clusters -d "{\"toml\": $(json_str <"$CONFIG")}" >/dev/null

echo "[smoke] precheck"
PRECHECK_ID="$(submit "$NAME" '{"kind":"precheck","params":{}}')"
wait_operation "$PRECHECK_ID"

echo "[smoke] create"
CREATE_ID="$(submit "$NAME" '{"kind":"create","params":{}}')"
# Follow the live event stream in the background; it closes on OpFinished.
curl -sS -N -b "$COOKIE_JAR" "$BASE/api/v1/operations/$CREATE_ID/events" \
    >"$DATA_DIR/events.jsonl" &
SSE_PID=$!
wait_operation "$CREATE_ID"
wait "$SSE_PID" 2>/dev/null || true

echo "[smoke] first log page:"
api GET "/api/v1/operations/$CREATE_ID/logs?after=0&limit=20" | python3 -m json.tool

echo "[smoke] status:"
api GET "/api/v1/operations/$CREATE_ID" | python3 -m json.tool

echo "[smoke] delete"
DELETE_ID="$(submit "$NAME" "{\"kind\":\"delete-cluster\",\"params\":{},\"confirm\":\"$NAME\"}")"
wait_operation "$DELETE_ID"

echo "[smoke] done: import → precheck → create → logs → status → delete"