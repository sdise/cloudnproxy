#!/usr/bin/env bash
#
# Run one load scenario (Linux): mock T5 node -> t5d -> sampler -> load client.
#
# Usage:
#   run-scenario.sh <name> <conns> <client-mode> <mock-mode> <rateKb> <seconds>
#
# Environment overrides: T5D, OUT_DIR, PROXY_PORT, MOCK_PORT
#
# Example:
#   scripts/bench/run-scenario.sh A 500 idle idle 0 60
#   scripts/bench/run-scenario.sh C 300 bulk sink 500 60

set -u

NAME="${1:-A}"
CONNS="${2:-100}"
MODE="${3:-idle}"
MOCK_MODE="${4:-idle}"
RATE="${5:-0}"
DURATION="${6:-30}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="${OUT_DIR:-$HERE/out}"
PROXY_PORT="${PROXY_PORT:-18080}"
MOCK_PORT="${MOCK_PORT:-19990}"
T5D="${T5D:-}"

mkdir -p "$OUT_DIR"

if [ -z "$T5D" ]; then
    for c in "$HERE/../../target/release/t5d" "$HERE/../../target/debug/t5d"; do
        if [ -x "$c" ]; then
            T5D="$c"
            break
        fi
    done
fi
if [ -z "$T5D" ] || [ ! -x "$T5D" ]; then
    echo "!! t5d not found."
    echo "   Build it with: cargo build --release -p t5-daemon"
    echo "   or set T5D=/path/to/t5d"
    exit 1
fi

OUT="$OUT_DIR/result-$NAME"

cleanup() {
    pkill -x t5d 2>/dev/null || true
    pkill -f 'mock-t5.js' 2>/dev/null || true
    pkill -f 'bench.js' 2>/dev/null || true
}

cleanup
sleep 0.5

if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$PROXY_PORT "; then
    echo "!! port $PROXY_PORT is already in use - abort"
    exit 1
fi

# Config: tunnel pool and reconnect off so pre-built connections cannot skew the
# numbers; egress binding follows system routing because the target is on loopback.
CFG="$OUT_DIR/config-$NAME.toml"
cat > "$CFG" <<EOF
listen_host = "127.0.0.1"
listen_port = $PROXY_PORT
allow_lan = false
resolve_domain = ""
upstream = "127.0.0.1:$MOCK_PORT"
current_node = "127.0.0.1:$MOCK_PORT"
fake_host = "cloudnproxy.baidu.com"
t5_auth = "bench"
max_conns = 0
chain_enabled = false
egress_interface = "system"
connect_timeout_ms = 5000
tcp_nodelay = true
tunnel_pool = false
auto_reconnect = false
auto_switch = false
log_level = "warn"
log_file = ""
EOF

echo "=== scenario $NAME : conns=$CONNS client=$MODE mock=$MOCK_MODE rate=${RATE}KB/s secs=$DURATION ==="
echo "t5d: $T5D"

node "$HERE/mock-t5.js" "$MOCK_PORT" "$MOCK_MODE" "$RATE" > "$OUT.mock.log" 2>&1 &
sleep 1

"$T5D" -f "$CFG" -log warn > "$OUT.t5d.log" 2>&1 &
T5D_PID=$!
sleep 2

if ! kill -0 "$T5D_PID" 2>/dev/null; then
    echo '!! t5d failed to start'
    head -n 20 "$OUT.t5d.log"
    cleanup
    exit 1
fi
echo "t5d started pid=$T5D_PID"

bash "$HERE/sample.sh" t5d "$OUT.csv" "$DURATION" 500 > "$OUT.sample.log" 2>&1 &
SAMPLE_PID=$!
sleep 2

node "$HERE/bench.js" "$PROXY_PORT" "$CONNS" "$MODE" "$DURATION" 2>&1 |
    tee "$OUT.bench.log" | tail -n 3

wait "$SAMPLE_PID" 2>/dev/null || true
cleanup
sleep 0.5

echo "=== $NAME done -> $OUT.csv ==="
