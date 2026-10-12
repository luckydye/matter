#!/usr/bin/env bash
# Integration test: discover and commission rs-matter's `onoff_light` example device,
# then check the commissioned node survives a server restart.
#
# Note: wipes `$TMPDIR/rs-matter`, where the example device persists its fabrics.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT=5599
API="http://127.0.0.1:$PORT"
# rs-matter's test device: passcode 20202021, discriminator 3840.
CODE="34970112332"
WORK=$(mktemp -d)
DEVICE_PID=""
SERVER_PID=""

cleanup() {
    kill $DEVICE_PID $SERVER_PID 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1"
    echo "--- server log"; tail -n 40 "$WORK/server.log" || true
    echo "--- device log"; tail -n 40 "$WORK/device.log" || true
    exit 1
}

# Poll `$1` (a URL) until the python expression `$2` over the JSON response `r` is true.
wait_for() {
    for _ in $(seq 60); do
        if curl -sf "$1" | python3 -c "import json,sys; r=json.load(sys.stdin); sys.exit(0 if ($2) else 1)" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

start_server() {
    ./target/debug/matter-server --listen "127.0.0.1:$PORT" --storage "$WORK/storage" >>"$WORK/server.log" 2>&1 &
    SERVER_PID=$!
}

RS_MATTER=$(dirname "$(dirname "$(cargo metadata --format-version 1 | python3 -c 'import json,sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"]=="rs-matter"))')")")
DEVICE_FEATURES=""
[ "$(uname)" = "Darwin" ] && DEVICE_FEATURES="--features astro-dnssd"

cargo build
cargo build --manifest-path "$RS_MATTER/Cargo.toml" -p rs-matter-examples --bin onoff_light $DEVICE_FEATURES --target-dir "$PWD/target/rs-matter"

rm -rf "${TMPDIR:-/tmp}/rs-matter"
./target/rs-matter/debug/onoff_light >"$WORK/device.log" 2>&1 &
DEVICE_PID=$!

start_server

wait_for "$API/devices" "any(d['kind'] == 'commissionable' and d['txt'].get('D') == '3840' for d in r)" \
    || fail "device was not discovered"
echo "ok: device discovered"

STATUS=$(curl -s -o "$WORK/commission.json" -w '%{http_code}' -X POST "$API/commission" \
    -H 'Content-Type: application/json' -d "{\"code\": \"$CODE\"}")
[ "$STATUS" = "200" ] || fail "commissioning returned $STATUS: $(cat "$WORK/commission.json")"
NODE_ID=$(python3 -c "import json; print(json.load(open('$WORK/commission.json'))['node_id'])")
echo "ok: commissioned as node $NODE_ID"

wait_for "$API/nodes" "any(n['node_id'] == $NODE_ID and n['operational'] for n in r)" \
    || fail "commissioned node is not discovered as operational"
echo "ok: node $NODE_ID is operational"

kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true
start_server
wait_for "$API/nodes" "[n['node_id'] for n in r] == [$NODE_ID]" \
    || fail "commissioned node was not persisted across restart"
echo "ok: node $NODE_ID persisted across restart"

echo "PASS"
