#!/bin/sh
ck=./train.ckpt
e=$(cat "$ck" 2>/dev/null || echo 1)
[ "$e" -gt 1 ] && echo "resumed from checkpoint: epoch $e"
trap 'echo "$e" > "$ck"; echo "paused: checkpoint saved at epoch $e"; exit 0' TERM
while true; do doum progress "epoch $e/40" --eta 2h; sleep 3 & wait $!; e=$((e + 1)); done
