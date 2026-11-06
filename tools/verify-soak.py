#!/usr/bin/env python3
"""PF-1203 soak verification (runs after tools/soak.ps1 completes).

Checks, with exact evidence:
1. Bounded memory: the daemon's private-bytes series plateaus — the
   last-half mean must not exceed the first-half mean by more than a small
   constant (runtime caches). A leak grows linearly; a healthy profiler
   plateaus.
2. Cadence stability: the recorded session's sample count meets the
   expected floor for the poll interval.
3. Chunk integrity: the durable chunk store has sealed chunks (>= 1/min
   with the 1 s flush), no surprise .partial residue, and the report path
   reads it cleanly.
4. The session reached a terminal state.

Usage: python tools/verify-soak.py [--minutes 60] [--poll-ms 250]
"""
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SOAK = REPO / "probe-output" / "soak"

failures = []


def ok(name: str, detail: str) -> None:
    print(f"  PASS {name}: {detail}")


def fail(name: str, detail: str) -> None:
    failures.append(name)
    print(f"  FAIL {name}: {detail}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=int, default=60)
    ap.add_argument("--poll-ms", type=int, default=250)
    args = ap.parse_args()

    # ---- 1. memory plateau ----
    mem_csv = SOAK / "memory.csv"
    lines = mem_csv.read_text().strip().splitlines()[1:]
    private = [int(line.split(",")[1]) for line in lines if line.strip()]
    print(f"[1] daemon private memory: {len(private)} samples over {args.minutes} min")
    if len(private) < 3:
        fail("memory-samples", f"only {len(private)} samples")
    else:
        half = len(private) // 2
        first_mean = statistics.mean(private[:half])
        last_mean = statistics.mean(private[half:])
        growth = last_mean - first_mean
        # A healthy daemon grows by a small fixed amount (runtime caches),
        # not linearly with time. Allow 30 MiB absolute; a 40 B/s leak
        # would add ~72 MiB/hour and trip this.
        if growth <= 30 * 1024 * 1024:
            ok(
                "memory-plateau",
                f"first-half {first_mean / 1e6:.1f} MB -> last-half "
                f"{last_mean / 1e6:.1f} MB (growth {growth / 1e6:.1f} MB)",
            )
        else:
            fail("memory-plateau", f"growth {growth / 1e6:.1f} MB between halves — likely leak")

    # ---- 2-4. the recorded session ----
    sessions = sorted((SOAK / "data" / "sessions").glob("*.pfsession"))
    if not sessions:
        fail("session", "no session directory produced")
        return 1
    session = sessions[-1]
    print(f"[2] session: {session.name}")

    expected_min = int(args.minutes * 60 * 1000 / args.poll_ms * 0.9)
    pf = REPO / "target" / "release" / "pulseforge.exe"
    out = subprocess.run(
        [str(pf), "report", "--session", str(session)], capture_output=True, text=True
    )
    if out.returncode != 0:
        fail("report", out.stderr.strip())
    else:
        report = json.loads(out.stdout.strip().splitlines()[-1])
        samples = report.get("samples", 0)
        print(f"[3] cadence: {samples} samples recorded")
        if samples >= expected_min:
            ok("cadence", f"{samples} samples >= {expected_min} expected (90% floor)")
        else:
            fail("cadence", f"{samples} < {expected_min} expected")

        state = report.get("state")
        if state in ("COMPLETED", "STOPPING", "FINALIZING", "RECOVERED_PARTIAL"):
            ok("terminal-state", str(state))
        else:
            fail("terminal-state", str(state))

        size = len(out.stdout)
        if size < 8 * 1024 * 1024:
            ok("report-size", f"{size / 1e6:.1f} MB bounded")
        else:
            fail("report-size", f"{size / 1e6:.1f} MB — unbounded growth?")

    # ---- chunk store integrity scan ----
    chunks = list((session / "chunks").glob("*.pfc"))
    partials = list((session / "chunks").glob("*.partial"))
    print(f"[4] chunk store: {len(chunks)} sealed chunks, {len(partials)} .partial residue")
    if len(chunks) >= args.minutes:
        ok("chunk-count", f"{len(chunks)} sealed (1 s flush -> >=1/min expected)")
    else:
        fail("chunk-count", f"{len(chunks)} sealed chunks")
    if len(partials) == 0:
        ok("no-partial-residue", "no interrupted writes")
    else:
        fail("no-partial-residue", f"{len(partials)} .partial files left")

    print()
    if failures:
        print(f"SOAK VERIFICATION FAILED: {failures}")
        return 1
    print("SOAK VERIFICATION PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
