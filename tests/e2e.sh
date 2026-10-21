#!/usr/bin/env bash
# End-to-end tests: a throwaway Doum daemon running harmless CPU jobs (no GPU needed), checking what the unit and
# property tests can't: real processes, signals, restarts and the HTTP API.
#
#   bash tests/e2e.sh [path/to/doum]          # default: target/release/doum
#   DOUM_E2E_SLURM=1 bash tests/e2e.sh        # also test the Slurm backend (needs a working Slurm with GPU shards)
#
# Runs in CI (without Slurm). Takes about two minutes, most of it the overrun test's one-minute margin.
set -euo pipefail

BIN="$(realpath "${1:-target/release/doum}")"
T="$(mktemp -d)"
PORT=$((18000 + RANDOM % 2000))
export DOUM_CONFIG="$T/doum.toml" DOUM_STATE_DIR="$T/state"
PASS=0
DAEMON=""

cleanup() {
    [ -n "$DAEMON" ] && kill "$DAEMON" 2>/dev/null || true
    pkill -f "$T/" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

ok() { PASS=$((PASS + 1)); printf '  \033[32m✓\033[0m %s\n' "$*"; }
fail() {
    printf '  \033[31m✗\033[0m %s\n' "$*"
    echo "--- daemon log"; tail -30 "$T/daemon.log" || true
    exit 1
}
# wait_for "description" seconds command...
wait_for() {
    local what="$1" secs="$2"; shift 2
    for _ in $(seq 1 $((secs * 2))); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.5
    done
    fail "timed out after ${secs}s: $what"
}
doum() { "$BIN" "$@"; }
job() { # job <name> -> field <key> of the newest job with that name
    doum status --json | python3 -c "
import sys, json
js = [j for j in json.load(sys.stdin)['jobs'] if j['name'] == sys.argv[1]]
j = max(js, key=lambda j: j['id']) if js else {}
print(j.get(sys.argv[2], ''))" "$1" "$2"
}
state_is() { [ "$(job "$1" state)" = "$2" ]; }
reported() { # the job has sent a progress report with an estimate
    doum status --json | python3 -c "
import sys, json
js = [j for j in json.load(sys.stdin)['jobs'] if j['name'] == sys.argv[1]]
sys.exit(0 if js and max(js, key=lambda j: j['id'])['progress'].get('eta_s') is not None else 1)" "$1"
}
log_has() { doum logs "$(job "$1" id)" 2>/dev/null | grep -q "$2"; }

start_daemon() {
    "$BIN" daemon >>"$T/daemon.log" 2>&1 &
    DAEMON=$!
    wait_for "daemon answers" 10 doum status
}

cat >"$DOUM_CONFIG" <<EOF
listen = "127.0.0.1:$PORT"
default_mode = "smart"
smart_wait_s = 600
max_wait_s = 0
pause_grace_s = 3
unmanaged_min_gb = 100000
[memory]
check = false
[notify]
desktop = false
[comfyui]
url = ""
free_when_idle = false
EOF

if [ -n "${DOUM_E2E_BACKEND:-}" ]; then
    sed -i "1i backend = \"$DOUM_E2E_BACKEND\"" "$DOUM_CONFIG"
fi

# stand-in jobs
cat >"$T/train.sh" <<'EOF'
#!/bin/sh
ck="$1"; e=$(cat "$ck" 2>/dev/null || echo 1)
[ "$e" -gt 1 ] && echo "resumed from checkpoint: epoch $e"
trap 'echo "$e" > "$ck"; echo "paused: checkpoint saved at epoch $e"; exit 0' TERM
while true; do doum progress "epoch $e" --eta 2h; sleep 1 & wait $!; e=$((e + 1)); done
EOF
cat >"$T/work.sh" <<'EOF'
#!/bin/sh
n=$1; i=1; while [ "$i" -le "$n" ]; do doum progress "step $i/$n" --eta "$(( n - i + 1 ))s"; sleep 1; i=$((i + 1)); done
EOF
printf '#!/bin/sh\ntrap "" TERM\nsleep 300\n' >"$T/stubborn.sh"
printf '#!/bin/sh\ndoum progress "almost done" --eta 2s\nsleep 300\n' >"$T/liar.sh"
chmod +x "$T"/*.sh
mkdir -p "$T/bin" && ln -s "$BIN" "$T/bin/doum"
export PATH="$T/bin:$PATH"
cd "$T"

echo "doum end-to-end tests (${DOUM_E2E_BACKEND:-native} backend, $(doum --version))"
start_daemon

echo "pause and resume"
doum run -d -s 60 -n render -- "$T"/work.sh 8 >/dev/null
doum run -d -s 24 -n train -- "$T"/train.sh "$T/train.ckpt" >/dev/null
wait_for "train running" 10 state_is train running
sleep 2
doum run -d -i -s 110 -n writer -- "$T"/work.sh 3 >/dev/null
wait_for "train paused for the interactive job" 15 log_has train "checkpoint saved"
ok "smart mode paused the long batch job; it saved its checkpoint"
wait_for "writer done" 40 state_is writer done
ok "the interactive job ran once the render finished"
wait_for "train resumed" 20 log_has train "resumed from checkpoint"
ok "the paused job resumed from its checkpoint"

echo "daemon restart"
kill "$DAEMON"; wait "$DAEMON" 2>/dev/null || true; DAEMON=""
pgrep -f "$T/train.sh" >/dev/null || fail "the job died with the daemon"
start_daemon
wait_for "train still running after restart" 10 state_is train running
ok "a background job kept running through a daemon restart and was picked up again"
doum cancel "$(job train id)" >/dev/null
wait_for "train cancelled" 15 state_is train cancelled
ok "cancel"

echo "cancel escalates"
doum run -d -s 4 -n stubborn -- "$T"/stubborn.sh >/dev/null
wait_for "stubborn running" 10 state_is stubborn running
sleep 1
doum cancel "$(job stubborn id)" >/dev/null
wait_for "stubborn killed after the grace period" 15 state_is stubborn cancelled
pgrep -f "$T/stubborn.sh" >/dev/null && fail "the job that ignores SIGTERM is still running"
ok "a job that ignores SIGTERM is killed after the grace period"

echo "API"
TOKEN=$(cat "$DOUM_STATE_DIR/token")
SECRET_VALUE="sk-e2e-not-real" doum run -d -s 1 -n envjob -- sleep 30 >/dev/null
wait_for "envjob running" 10 state_is envjob running
curl -s "127.0.0.1:$PORT/api/status" | grep -q "sk-e2e-not-real" && fail "a job's environment leaked through /api/status"
ok "job environments are not returned by the API"
[ "$(stat -c %a "$DOUM_STATE_DIR")" = "700" ] || fail "state directory is not owner-only"
[ "$(stat -c %a "$DOUM_STATE_DIR/state.json")" = "600" ] || fail "state.json is not owner-only"
ok "state directory 0700, state.json 0600"
code=$(curl -s -o /dev/null -w "%{http_code}" -H "Host: evil.example:$PORT" "127.0.0.1:$PORT/api/status")
[ "$code" = "403" ] || fail "a foreign Host header got $code, not 403"
ok "requests addressed to another host are refused (DNS rebinding)"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "127.0.0.1:$PORT/api/jobs" -H "x-doum-token: $TOKEN" \
    -H 'content-type: application/json' -d '{"name":"x","cmd":["true"],"cwd":"/tmp","shards":200,"owner":"agent","detached":true}')
[ "$code" = "403" ] || fail "an agent job over the shard limit got $code, not 403"
ok "agent limits are enforced by the daemon"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "127.0.0.1:$PORT/api/jobs" -H 'content-type: application/json' \
    -d '{"name":"x","cmd":["true"],"cwd":"/tmp","shards":1}')
[ "$code" = "401" ] || fail "a submit without the token got $code, not 401"
ok "changes need the token"
curl -s "127.0.0.1:$PORT/metrics" | grep -q '^doum_up 1' || fail "/metrics"
ok "Prometheus /metrics"
doum cancel "$(job envjob id)" >/dev/null

echo "re-planning when a job overruns its estimate (about a minute)"
doum run -d -s 60 -n liar -- "$T"/liar.sh >/dev/null
wait_for "liar running" 10 state_is liar running
wait_for "liar reported its estimate" 15 reported liar
doum run -d -i -s 110 -n urgent -- "$T"/work.sh 2 >/dev/null
sleep 3
state_is liar running || fail "smart mode should first wait for a job that says it is almost done"
ok "smart mode waits for a job that says it is almost done"
wait_for "the overrunning job is paused" 90 state_is urgent done
grep -q "ran past its estimate" "$T/daemon.log" || fail "no 'ran past its estimate' in the daemon log"
ok "once its estimate went stale it was paused, and the urgent job ran"
doum cancel "$(job liar id)" >/dev/null

if [ "${DOUM_E2E_BACKEND:-}" = "slurm" ]; then
    echo "Slurm backend"
    squeue -h -o "%j" | grep -q "^doum-" && fail "leftover doum jobs in squeue before the test"
    doum run -d -s 2 -n slurmjob -- sleep 20 >/dev/null
    wait_for "slurmjob running in Slurm" 20 sh -c 'squeue -h -o "%j %T" | grep -q "slurmjob RUNNING"'
    ok "a background job runs as a Slurm job ($(squeue -h -o '%j' | grep slurmjob))"
    doum cancel "$(job slurmjob id)" >/dev/null
    wait_for "slurmjob gone from squeue" 20 sh -c '! squeue -h -o "%j" | grep -q slurmjob'
    ok "cancel goes through scancel"
fi

echo "$PASS checks passed"
if [ -n "${DOUM_E2E_SLURM:-}" ] && [ -z "${DOUM_E2E_BACKEND:-}" ]; then
    trap - EXIT; cleanup
    DOUM_E2E_BACKEND=slurm exec bash "$0" "$BIN"
fi
