#!/usr/bin/env bash
# Smoke-test the CLI surface end-to-end.
#
# Boots the proxy in the background pointed at a non-existent upstream (we
# never actually proxy a request — just exercise the control plane), then
# walks each subcommand and asserts the binary answers.
#
# Usage: scripts/cli-smoke.sh <path-to-gostly-agent-binary>
#   e.g. scripts/cli-smoke.sh target/release/gostly-agent
#
# Exits 0 on success, 1 on first failure. The trap kills the proxy on exit
# (success or failure) so a hung CI run never leaks a process.

set -euo pipefail

BIN="${1:-}"
[ -n "$BIN" ] || { echo "usage: $0 <path-to-gostly-agent-binary>" >&2; exit 2; }
[ -x "$BIN" ] || { echo "binary not found or not executable: $BIN" >&2; exit 1; }

PORT="${GOSTLY_SMOKE_PORT:-18080}"
DATA_DIR="$(mktemp -d -t gostly-smoke.XXXXXX)"
PID=""

cleanup() {
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        # Give it a beat to flush, then SIGKILL if still around.
        sleep 1
        kill -9 "$PID" 2>/dev/null || true
    fi
    rm -rf "$DATA_DIR"
}
trap cleanup EXIT

step() { printf '\n→ %s\n' "$*"; }
fail() { printf '✗ %s\n' "$*" >&2; exit 1; }

step "gostly --version"
VER_OUT="$("$BIN" --version)"
echo "  $VER_OUT"
echo "$VER_OUT" | grep -q "gostly v" || fail "version output missing 'gostly v': $VER_OUT"

step "gostly -V (short form)"
"$BIN" -V | grep -q "gostly v" || fail "-V output wrong"

step "gostly --help"
"$BIN" --help | grep -q "^Usage:" || fail "--help output missing 'Usage:'"
"$BIN" --help | grep -q "start" || fail "--help missing 'start' subcommand"

step "gostly start --help"
"$BIN" start --help | grep -q "upstream" || fail "start --help missing --upstream"

# Boot the proxy. We point --upstream at a port nothing's listening on; the
# proxy itself comes up regardless because forwarding only happens on a
# real client request.
step "gostly start (background, port $PORT, data $DATA_DIR)"
"$BIN" start \
    --upstream http://127.0.0.1:9999 \
    --port "$PORT" \
    --data-dir "$DATA_DIR" \
    > "$DATA_DIR/proxy.log" 2>&1 &
PID=$!
echo "  pid=$PID"

# Wait for the listener — poll instead of sleeping so a slow CI runner
# doesn't false-fail on the first check.
for i in 1 2 3 4 5 6 7 8 9 10; do
    if "$BIN" status --port "$PORT" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

step "gostly status"
STATUS_OUT="$("$BIN" status --port "$PORT")"
echo "$STATUS_OUT"
echo "$STATUS_OUT" | grep -q '"status": "ok"' || fail "status did not return ok"

step "gostly mode MOCK"
MODE_OUT="$("$BIN" mode mock --port "$PORT")"
echo "  $MODE_OUT"
echo "$MODE_OUT" | grep -q "MOCK" || fail "mode change response missing MOCK"

step "gostly status (verify mode flipped)"
"$BIN" status --port "$PORT" | grep -q '"mode": "Mock"' \
    || fail "status does not reflect Mock mode after switch"

step "gostly export --format openapi"
EXPORT_OUT="$("$BIN" export --format openapi --port "$PORT")"
echo "$EXPORT_OUT" | head -3
echo "$EXPORT_OUT" | grep -q '"format": "openapi"' || fail "export missing format tag"

step "gostly stop"
"$BIN" stop --pidfile "$DATA_DIR/gostly.pid"

# Give the proxy a moment to wind down. Don't hard-fail if it lingers — the
# trap will reap it; but report it.
sleep 1
if kill -0 "$PID" 2>/dev/null; then
    echo "  warning: proxy still alive 1s after SIGTERM (will be cleaned up)"
fi

echo
echo "✓ CLI smoke passed"
