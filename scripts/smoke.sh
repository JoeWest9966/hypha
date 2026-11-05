#!/bin/sh
# End-to-end smoke test: hub + one local satellite + exec + pty, no external deps.
# Exits non-zero on the first thing that doesn't work.
set -eu
cd "$(dirname "$0")/.."

# Not 5599: that is where a production hyphad sits on a loopback install, and a
# smoke run that silently talks to the real hub enrolls a junk satellite into the
# real fleet (it has happened).
PORT="${PORT:-5598}"
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
  "static_dir": ""
}
EOF
mkdir -p "$TMP/bin"

say "starting hub"
"$BIN/hyphad" --config "$TMP/hub.json" > "$TMP/hub.log" 2>&1 &
HUB_PID=$!
# If the hub could not bind, every later step would talk to whatever else owns
# the port. Fail loudly instead.
if ! kill -0 "$HUB_PID" 2>/dev/null; then
    echo "hub exited immediately:"; cat "$TMP/hub.log"; exit 1
fi
for _ in $(seq 1 50); do
    curl -sf "$HUB/api/meta" > /dev/null && break
    sleep 0.1
done

say "pointing cli at the hub"
"$BIN/hypha" use "$HUB" > /dev/null

ENROLL="$(curl -sf -X POST "$HUB/api/enroll-tokens" -H 'content-type: application/json' \
    -d '{"ttl_sec":600,"tags":["smoke"]}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')"

say "enrolling local satellite"
# The hub retires a host record when a new enrollment claims the same hostname, so
# a smoke run on a box that also runs a real agent would delete the real one.
# Claim a throwaway name instead.
"$BIN/hypha-sat" --config "$TMP/agent.json" enroll --hub "$HUB" --token "$ENROLL" \
    --name "smoke-$$" > /dev/null
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
for need in ("host.enroll", "exec", "enroll.token"):
    assert need in actions, f"missing audit action {need}: {actions}"
print("  audit ok:", sorted(actions))
PY

say "checking file push"
printf 'pushed-by-smoke\n' > "$TMP/payload.txt"
"$BIN/hypha" cp "$TMP/payload.txt" "$TMP/remote.txt" > "$TMP/cp.txt" 2>&1
grep -q "ok" "$TMP/cp.txt" || { cat "$TMP/cp.txt"; exit 1; }
grep -q "pushed-by-smoke" "$TMP/remote.txt" || { echo "file did not land"; exit 1; }

say "checking metrics"
for _ in $(seq 1 60); do
    curl -sf "$HUB/api/hosts" | grep -q '"mem_total":[1-9]' && break
    sleep 0.5
done
curl -sf "$HUB/api/hosts" | grep -q '"disk_total":[1-9]' || { echo "agent never reported metrics"; exit 1; }

say "checking cancel"
EXEC=$(curl -sf -X POST "$HUB/api/exec" -H 'content-type: application/json' \
    -d '{"command":"sleep 60","targets":{"all":true}}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["exec_id"])')
sleep 1
curl -sf -X POST "$HUB/api/exec/$EXEC/cancel" | grep -q '"cancelled":\["' || {
    echo "cancel did not reach any host"; exit 1; }
sleep 2
curl -sfN "$HUB/api/exec/$EXEC/events" | grep -q '"signal":15' || {
    echo "cancelled job did not report signal 15"; exit 1; }

say "checking pty session reattach"
HOST_ID=$(curl -sf "$HUB/api/hosts" | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["id"])')
PTY=$(curl -sf -X POST "$HUB/api/pty" -H 'content-type: application/json' \
    -d "{\"host_id\":\"$HOST_ID\"}" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["pty_id"])')
node scripts/pty-reattach.mjs "$HUB" "$PTY" || exit 1

say "checking exec output survives a hub restart"
EXEC2=$(curl -sf -X POST "$HUB/api/exec" -H 'content-type: application/json' \
    -d '{"command":"echo durable-marker","targets":{"all":true}}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["exec_id"])')
sleep 2
kill "$HUB_PID" 2>/dev/null || true
wait "$HUB_PID" 2>/dev/null || true
"$BIN/hyphad" --config "$TMP/hub.json" >> "$TMP/hub.log" 2>&1 &
HUB_PID=$!
for _ in $(seq 1 50); do
    curl -sf "$HUB/api/meta" > /dev/null && break
    sleep 0.1
done
# The in-memory channel is gone; the frames must come back from SQLite.
# Output frames are base64, so decode before looking for the marker.
curl -sfN "$HUB/api/exec/$EXEC2/events" | python3 -c '
import sys, json, base64
for line in sys.stdin:
    if not line.startswith("data:"):
        continue
    ev = json.loads(line[5:])
    if ev.get("kind") == "output" and b"durable-marker" in base64.b64decode(ev["data"]):
        sys.exit(0)
sys.exit(1)
' || { echo "exec output was not replayed from storage after restart"; exit 1; }
curl -sf "$HUB/api/audit?limit=200" | grep -q 'file.push' || { echo "file.push missing from audit"; exit 1; }
curl -sf "$HUB/api/audit?limit=200" | grep -q 'pty.open' || { echo "pty.open missing from audit"; exit 1; }

say "all good"
