# Delivery Record — PulseForge v0.4.0

**Delivered milestone:** v0.4.0 (v0.4 core per the roadmap's milestone
table), accepted as the terminal state of the autonomous build session.
**Delivered:** 2026-10-27 · **Commits:** 64+ · **Tests:** 233 passing, 0
failing · **License:** MIT.

## Decision (formal)

The build session reached the fork between *provisioning the missing v1.0
resources* and *accepting v0.4.0*. The resources are categorically outside
an autonomous session's reach: a Linux host/VM, an elevated Windows test
context (which the project's own contract forbids self-granting), human
reviewers and usability participants, and signing/clean-VM infrastructure.

**Decision: v0.4.0 is accepted as the final delivered milestone of this
session.** The blocked remainder is documented below with per-item unblock
conditions. This decision is reversible by design — the ledger in
the private task ledger (`docs/.local/TASKS.md`) and the procedures cited here let any future session (with a
Linux host, elevation, or human reviewers) continue the roadmap from
exactly this state without rework.

## Scope delivered (all evidence-backed, all gates green)

| Milestone | Delivered capability |
|---|---|
| P0 feasibility | Platform probes with measured evidence (counters cross-validation, ETW lifecycle, MCP wire, encoding benchmark) |
| v0.1 | Windows counters CLI: `record` / `report` / `capabilities` / `probe` / `doctor` — typed units, PID-reuse-proof identity, honest availability |
| P4 + v0.3 daemon/IPC | Per-user daemon, owner-only named pipe, disconnect survival, crash recovery |
| P5 storage | Durable CRC chunks, bounded queries, envelope rollups, kill-durability, 1M-record scale |
| P6 MCP core | 9 schema'd tools over stdio; agent workflow e2e; launch tool disabled by design |
| P7 GUI core | Native egui/WGPU app: browser, process picker, dashboard (zoom/pan + envelopes), diagnostics drawer, compare panel, themes |
| P11 comparison | CLI + MCP + GUI on one engine; repeated-run `bench` with distributions; honesty caveats everywhere |
| P12 subsets | Fuzzing (~160k cases, 2 real bugs fixed), 60-min soak (99.9% cadence, 0-byte post-stop plateau), overhead studies, handle audit, security self-review, license/SBOM |
| P13 subsets | Release zip + SHA-256 + SBOM + release CI, permissions matrix, MCP guide, methodology, governance, contract fixtures, versioning policy |

15 ADRs record every architectural decision, including reversals and
findings. The private task ledger (`docs/.local/TASKS.md`) maps every roadmap item to status + evidence.

## Explicitly NOT delivered — with unblock conditions

Per the founding contract ("never represent generated code as a shipped
feature"; "NOT VERIFIED leaves the milestone gate open"), the following
v1.0 scope is **open, not claimed**:

| Item | Required resource | Unblock action |
|---|---|---|
| P3/P10 Linux parity, PF-404 UDS | A Linux host/VM with `/proc` and `perf_event_open` | Run the Windows-mirrored probe/collector suites; CI cross-compile gate already carries the code |
| P8 ETW kernel-provider sampling → PF-901+ symbols/flamegraphs | An elevated Windows test context | Run the privilege matrix from `docs/.local/feasibility-results.md` §B |
| PF-1201 proper, PF-1210 sign-off | An independent human security/review auditor | Review `docs/.local/security-review.md` as the starting input |
| PF-1211 usability sessions | Human developers + agents on real regressions | Exercise the MCP/GUI compare surfaces |
| P13 proper | Clean Windows/Linux machines, signing certificates | Install tests from the release zip (smoke-tested here); MSIX/deb after signing |
| Multi-day soak | Longer environment uptime | Re-run `tools/soak.ps1` with `-Minutes` as available |

## Acceptance rationale

Full v1.0 completion requires human reviewers/users, an elevation context
the project's contract forbids self-granting, and machine infrastructure
outside this session. Everything verifiable on this machine **has been**
verified, with the procedure for the rest preserved. Per the founding
principle — *"a small correct profiler is more valuable than a visually
polished product that invents bottlenecks"* — v0.4.0 is delivered as-is.

## How to continue

1. Clone this repository; `cargo test --workspace` reproduces the green
   gate.
2. Pick any row from the private ledger (`docs/.local/TASKS.md`) marked open/partial — each carries its
   status, evidence links, and the next dependency.
3. For the blocked rows, provision the resource named above; the code,
   tests, and procedures for each are already in place or specified.
