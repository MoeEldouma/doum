#!/usr/bin/env bash
# Full cycle test for a local model behind llama-swap + doum:
#   request → doum job queued → (gate) → model server starts → answer → unload → server stops → shards freed
# Usage: tests/llm_cycle.sh <model-id> [llama-swap base, default http://127.0.0.1:8410]
set -euo pipefail
MODEL="$1"; BASE="${2:-http://127.0.0.1:8410}"
mem() { awk '/MemAvailable/ {printf "%.1f", $2/1048576}' /proc/meminfo; }
echo "== before: $(mem) GB free"; doum status | head -1

t0=$(date +%s.%N)
resp=$(curl -s --max-time 1800 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d "{
  \"model\": \"$MODEL\", \"max_tokens\": 200,
  \"messages\": [{\"role\": \"user\", \"content\": \"In two short sentences, what does a GPU job queue do?\"}]}")
t1=$(date +%s.%N)
echo "== answered in $(printf '%.1f' "$(echo "$t1 - $t0" | bc)") s (includes queue wait + model load)"
echo "$resp" | python3 -c "
import json,sys
r=json.load(sys.stdin); m=r['choices'][0]['message']; u=r.get('usage',{})
print('reply:', (m.get('content') or '').strip()[:300])
print('tokens:', u)"
echo "== while loaded: $(mem) GB free"; doum status | sed -n '1p;/RUNNING/,/WAITING/p'

# second request, model already loaded: pure generation speed
t0=$(date +%s.%N)
resp=$(curl -s --max-time 600 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d "{
  \"model\": \"$MODEL\", \"max_tokens\": 300,
  \"messages\": [{\"role\": \"user\", \"content\": \"Write 150 words about the doum palm of the Nile.\"}]}")
t1=$(date +%s.%N)
python3 - "$resp" "$t0" "$t1" <<'EOF'
import json, sys
r = json.loads(sys.argv[1]); dt = float(sys.argv[3]) - float(sys.argv[2]); n = r.get("usage", {}).get("completion_tokens", 0)
print(f"== warm request: {n} tokens in {dt:.1f} s = {n/dt:.1f} tok/s (end to end)")
EOF

echo "== unloading"
curl -s -X POST "$BASE/api/models/unload" -o /dev/null -w "unload HTTP %{http_code}\n" || true
for i in $(seq 1 60); do
  if ! pgrep -f "ollama serve" >/dev/null; then break; fi; sleep 1
done
sleep 3
echo "== after: $(mem) GB free · ollama running: $(pgrep -fc 'ollama serve' || true)"; doum status --all | sed -n '1p;/FINISHED/,$p' | tail -4
