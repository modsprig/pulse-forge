#!/usr/bin/env bash
# PF-210 observer-overhead study on this machine.
#
# Method: run the same CPU-bound workload (pulseforge bench-spin) N times
# bare, then N times while attached-recording at each poll interval; report
# per-side medians and the observed delta. Workload duration is measured by
# the workload itself (wall clock of the spin), so scheduling noise affects
# both sides symmetrically.
#
# Caveats (honest): this sandbox zeroes CPU-time accounting for spawned
# processes (feasibility A.3), so the study compares WALL time of the
# workload under observation vs not — the observer-overhead question in its
# user-visible form. Medians over N=7 per cell.

set -u
cd "$(dirname "$0")/.."

REPS="${REPS:-7}"
WORKLOAD_MS=2000
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

# Binary name differs per platform build (Windows .exe, Linux bare).
PF=./target/release/pulseforge.exe
[ -x "$PF" ] || PF=./target/release/pulseforge
[ -x "$PF" ] || { echo "build first: cargo build --release" >&2; exit 1; }

run_bare() {
  # Wall duration of a bare spin, measured by timing the process.
  local s e
  s=$(date +%s%N)
  "$PF" bench-spin --ms "$WORKLOAD_MS" >/dev/null 2>&1
  e=$(date +%s%N)
  echo $(( (e - s) / 1000000 ))
}

run_recorded() {
  local interval=$1
  local s e
  s=$(date +%s%N)
  PULSEFORGE_DATA="$OUT" "$PF" record --run "$PF" --duration-ms $((WORKLOAD_MS + 800)) \
      --interval-ms "$interval" --out "$OUT/s$interval" -- bench-spin --ms "$WORKLOAD_MS" >/dev/null 2>&1
  e=$(date +%s%N)
  echo $(( (e - s) / 1000000 ))
}

median() {
  printf '%s\n' "$@" | sort -n | awk '{ a[NR]=$1 } END { print (NR%2==1) ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2 }'
}

echo "{\"study\": \"observer overhead (wall time, ms)\", \"workload_ms\": $WORKLOAD_MS, \"reps\": $REPS, \"cells\": {"

bare=()
for i in $(seq "$REPS"); do bare+=("$(run_bare)"); done
bare_med=$(median "${bare[@]}")
echo "  \"bare\": {\"median\": $bare_med, \"runs\": [$(IFS=,; echo "${bare[*]}")]},"

first=1
for interval in 100 250 1000; do
  rec=()
  for i in $(seq "$REPS"); do rec+=("$(run_recorded "$interval")"); done
  rec_med=$(median "${rec[@]}")
  [ $first -eq 0 ] && echo ","
  first=0
  printf '  "recorded_%sms": {"median": %s, "runs": [%s]}' "$interval" "$rec_med" "$(IFS=,; echo "${rec[*]}")"
done
echo ""
echo "}}"
