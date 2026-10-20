#!/bin/sh
# work.sh STEPS SECONDS_PER_STEP LABEL: a stand-in GPU job that reports progress
n=$1; s=$2; label=$3
i=1; while [ "$i" -le "$n" ]; do doum progress "$label $i/$n" --eta "$(( (n - i + 1) * s ))s"; sleep "$s"; i=$((i + 1)); done
