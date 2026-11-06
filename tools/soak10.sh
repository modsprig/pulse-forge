#!/usr/bin/env bash
# 10-minute sustained soak on v0.8.0 code: daemon self-recording through
# agent start with the stacks pipeline on (perf probe succeeds on WSL2),
# RSS sampled every 30 s for the plateau check, artifacts verified after
# stop. Evidence for the durability changes (atomic persist, scanner).
set -u
d=/tmp/pf-soak10-$$
rm -rf "$d"; mkdir -p "$d"
ROOT="${PULSEFORGE_WSL_ROOT:?set PULSEFORGE_WSL_ROOT to the repo checkout inside WSL}"
PF="$ROOT/target/release/pulseforge"
export PULSEFORGE_DATA="$d/data"
"$PF" daemon > "$d/daemon.log" 2>&1 &
DPID=$!
sleep 1
SPIN="$ROOT/target/release/pf-fixture-cpu"
"$SPIN" --ms 590000 &
WPID=$!
sleep 0.3
kill -0 "$WPID" 2>/dev/null || { echo "FIXTURE DIED AT SPAWN"; exit 1; }
OUT=$(PULSEFORGE_DATA="$d/data" "$PF" agent start --pid "$WPID" --duration-ms 600000 --interval-ms 250 2>&1)
SID=$(echo "$OUT" | grep -o '"session_id":"[^"]*"' | head -1 | cut -d'"' -f4)
echo "SID=$SID"
for i in $(seq 1 20); do
  sleep 30
  RSS=$(grep VmRSS /proc/$DPID/status 2>/dev/null | awk '{print $2}')
  echo "t=$((i * 30))s rss_kb=$RSS" >> "$d/rss.log"
done
PULSEFORGE_DATA="$d/data" "$PF" agent stop --session "$SID" >> "$d/stop.log" 2>&1
sleep 3
STATE=$(PULSEFORGE_DATA="$d/data" "$PF" agent status --session "$SID" 2>/dev/null | grep -o '"state":"[A-Z_]*"' | head -1)
echo "$STATE" > "$d/state.txt"
kill "$DPID" 2>/dev/null
kill "$WPID" 2>/dev/null
SESS="$d/data/sessions/$SID.pfsession"
echo "=== artifacts ==="
ls "$SESS" 2>/dev/null
echo "=== stacks lines ==="
wc -l "$SESS/stacks.jsonl" 2>/dev/null || echo "no stacks.jsonl"
echo "=== summary tail ==="
grep -o '"samples":[0-9]*\|"truncated":[a-z]*\|"frames_total":[0-9]*' "$SESS/stacks-summary.json" 2>/dev/null | head -3
echo "=== chunk partial residue ==="
ls "$SESS/chunks" 2>/dev/null | grep -c partial
echo "=== state ==="
cat "$d/state.txt"
echo "=== rss tail ==="
tail -5 "$d/rss.log" 2>/dev/null
echo "SOAK-DIR=$d"
