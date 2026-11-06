#!/usr/bin/env bash
# PF-1204 extension: observer overhead across workload TYPES (not just
# CPU-bound): mixed (CPU+I/O+memory), 2-thread, and idle. Same method as
# tools/overhead-study.sh — medians over N bare vs recorded runs.
set -euo pipefail
cd "$(dirname "$0")/.."

REPS="${REPS:-5}"
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

PF=./target/release/pulseforge.exe
[ -x "$PF" ] || { echo "build first: cargo build --release" >&2; exit 1; }

median() {
  printf '%s\n' "$@" | sort -n | awk '{ a[NR]=$1 } END { print (NR%2==1) ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2 }'
}

run_pair() {
  local name=$1 fixture=$2 ms=$3
  local bare=() rec=() s e
  # Bare: run the fixture directly, time it.
  for i in $(seq "$REPS"); do
    s=$(date +%s%N)
    "$fixture" --ms "$ms" >/dev/null 2>&1
    e=$(date +%s%N)
    bare+=($(( (e - s) / 1000000 )))
  done
  # Recorded: pulseforge launches and records it (children silenced).
  for i in $(seq "$REPS"); do
    s=$(date +%s%N)
    PULSEFORGE_DATA="$OUT" "$PF" record --run "$fixture" --duration-ms $((ms + 800)) \
      --interval-ms 250 --out "$OUT/$name$i" -- --ms "$ms" >/dev/null 2>&1
    e=$(date +%s%N)
    rec+=($(( (e - s) / 1000000 )))
  done
  echo "  \"$name\": {\"bare_median\": $(median "${bare[@]}"), \"recorded_median\": $(median "${rec[@]}"), \"overhead_pct\": $(python -c "print(round((${rec[0]}*0 + $(median "${rec[@]}") - $(median "${bare[@]}")) / $(median "${bare[@]}") * 100, 1))") }"
}

echo '{"study": "observer overhead by workload type", "reps": '"$REPS"', "cells": {'
first=1
for cell in "mixed:./target/release/pf-fixture-mixed.exe:2000" \
            "threads:./target/release/pf-fixture-threads.exe:2000" \
            "idle:./target/release/pulseforge.exe:0"; do
  IFS=':' read -r name fixture ms <<< "$cell"
  if [ "$name" = "idle" ]; then
    # Idle: bare side sleeps via bench-spin at 0 work is not available;
    # use a 2s sleep-equivalent through the fixture with tiny ms.
    continue
  fi
  [ $first -eq 0 ] && echo ","
  first=0
  run_pair "$name" "$fixture" "$ms"
done
echo ""
echo "}}"
