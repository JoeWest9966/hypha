#!/bin/sh
# End-to-end smoke test: hub + one local satellite + exec + pty, no external deps.
# Exits non-zero on the first thing that doesn't work.
set -eu
cd "$(dirname "$0")/.."

PORT="${PORT:-5599}"
TMP="$(mktemp -d)"
HUB="http://127.0.0.1:${PORT}"
export HYPHA_CONFIG="$TMP/config.json"

cleanup() {
    kill "$HUB_PID" 2>/dev/null || true
    kill "$SAT_PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

say() { printf '\033[36m[smoke]\033[0m %s\n' "$*"; }

say "building"
cargo build -q --bin hyphad --bin hypha-sat --bin hypha
BIN=target/debug

cat > "$TMP/hub.json" <<EOF
{
  "listen": "127.0.0.1:${PORT}",
  "public_url": "${HUB}",
  "data_dir": "${TMP}/data",
  "bin_dir": "${TMP}/bin",
  "static_dir": "$(pwd)/crates/hub/static",
  "password": "smoke"
}
EOF
mkdir -p "$TMP/bin"

say "starting hub"
"$BIN/hyphad" --config "$TMP/hub.json" > "$TMP/hub.log" 2>&1 &
HUB_PID=$!
for _ in $(seq 1 50); do
    curl -sf "$HUB/api/meta" > /dev/null && break
    sleep 0.1
done

say "logging in"
"$BIN/hypha" login --hub "$HUB" --password smoke > /dev/null

TOKEN="$(python3 -c 'import json,os;print(json.load(open(os.environ["HYPHA_CONFIG"]))["token"])')"
ENROLL="$(curl -sf -X POST "$HUB/api/enroll-tokens" \
    -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d '{"ttl_sec":600,"tags":["smoke"]}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')"

say "enrolling local satellite"
"$BIN/hypha-sat" --config "$TMP/agent.json" enroll --hub "$HUB" --token "$ENROLL" > /dev/null
"$BIN/hypha-sat" --config "$TMP/agent.json" run > "$TMP/agent.log" 2>&1 &
SAT_PID=$!

for _ in $(seq 1 50); do
    "$BIN/hypha" hosts 2>/dev/null | grep -q "1 online" && break
    sleep 0.2
done

say "checking fleet view"
"$BIN/hypha" hosts | tee "$TMP/hosts.txt"
grep -q "1 online" "$TMP/hosts.txt" || { echo "satellite never came online"; exit 1; }

say "running a command"
"$BIN/hypha" run -- 'echo smoke-ok; echo to-stderr >&2' > "$TMP/run.txt" 2>&1
grep -q "smoke-ok" "$TMP/run.txt" || { cat "$TMP/run.txt"; exit 1; }
grep -q "to-stderr" "$TMP/run.txt" || { cat "$TMP/run.txt"; exit 1; }

say "checking exit code propagation"
if "$BIN/hypha" run -- 'exit 3' > /dev/null 2>&1; then
    echo "expected non-zero exit for 'exit 3'"; exit 1
fi

say "checking pty"
HOST=$("$BIN/hypha" --no-color hosts | awk 'NR==2{print $2}')
printf "echo pty-ok\nexit\n" | timeout 20 "$BIN/hypha" sh "$HOST" > "$TMP/pty.txt" 2>&1 || true
grep -q "pty-ok" "$TMP/pty.txt" || { cat "$TMP/pty.txt"; exit 1; }

say "checking audit trail"
python3 - "$TMP" <<'PY'
import sqlite3, sys, os
db = sqlite3.connect(os.path.join(sys.argv[1], "data", "hypha.db"))
rows = db.execute("select action from audit").fetchall()
actions = {r[0] for r in rows}
for need in ("host.enroll", "exec", "auth.login"):
    assert need in actions, f"missing audit action {need}: {actions}"
print("  audit ok:", sorted(actions))
PY

say "all good"
