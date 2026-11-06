# PulseForge

**A fast, local-first, Rust-native process profiler for Windows and Linux — built for humans and AI coding agents.**

![license](https://img.shields.io/badge/license-MIT-blue)
![rust](https://img.shields.io/badge/rust-2024%20edition%20%C2%B7%201.97%2B-orange)
![platform](https://img.shields.io/badge/platform-Windows%2011%20x64%20%F0%9F%9F%A2%20%7C%20Linux%20x64%20%E2%9C%A8-lightgrey)
![status](https://img.shields.io/badge/status-v0.7%20on--CPU%20stack%20profiler-green)

PulseForge records what a process actually consumes — CPU time, memory residency,
I/O — with **typed units, honest availability states and stable process identity**,
It is a native on-CPU stack profiler — `perf_event_open` per-thread sampling on
Linux (ETW probe on Windows pending an elevated prompt), DWARF symbolization,
bounded flamegraphs, speedscope/pprof exports, an `egui` desktop UI and an MCP
interface for coding agents.

> **No AI model runs inside PulseForge.** It produces measurements, not guesses:
> it will never label residency as a "leak" or a sample share as exact CPU time.

## Status

| Capability | Windows 11 x64 | Linux x64 | Milestone |
|---|---|---|---|
| Process identity + resource counters (CLI) | ✅ shipped | ✅ shipped (WSL2-verified) | v0.1/v0.5 |
| Per-user daemon + owner-only named-pipe IPC | ✅ shipped | ✅ shipped (UDS, WSL2-verified) | **v0.3** |
| Durable binary chunks + crash recovery | ✅ shipped | ✅ shipped (OS-neutral) | **v0.3** |
| MCP stdio interface for agents | ✅ shipped | ✅ shipped (UDS, WSL2-verified) | v0.4 |
| Native `egui`/WGPU desktop UI (`pulseforge-gui`) | ✅ shipped (Glow renderer) | ✅ shipped + flamegraph (WSL2-verified) | v0.4 |
| On-CPU stack sampling (ETW / perf) | 🔬 ETW needs elevated prompt | ✅ perf per-thread sampling + CPU attribution (WSL2-verified) | v0.6 |
| Symbolization + stack aggregation engine | ✅ self-stack scope | ✅ DWARF inline/line/build-id/debuglink/comp-dir (WSL2-verified) | v0.7 |
| Flamegraph + bottom-up + time/thread filters | ✅ GUI | ✅ GUI + CLI + function-shift compare (WSL2-verified) | v0.7 |
| Exports: speedscope + pprof | ✅ | ✅ both viewer-verified (WSL2-verified) | v0.9 |
| A/B observed-delta comparison (CLI) | ✅ shipped | ✅ (OS-neutral) | v0.9 |
| Linux .deb installer | — | ✅ built + installed + verified (WSL2) | v0.6 |
| 60-minute soak | ✅ SOAK VERIFICATION PASSED | — | P12 |

Capabilities are **feature-probed at runtime**, never inferred from the
OS name.

## Install

**Windows (per-user, no admin):** download `pulseforge-<version>-windows-x64.zip`
from a release, unzip, then:

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1
```

That installs to `%LOCALAPPDATA%\Programs\PulseForge`, adds it to your user
PATH (new terminals), and creates a Start-Menu shortcut for the GUI.
`-Uninstall` removes everything again. **Linux:** `pulseforge_<version>_amd64.deb`
(`dpkg -i`). Building from source needs Rust 1.97+ (MSVC on Windows); the
core crates are dependency-free and build fully offline — `cargo build --release`.

## Quick start

```bash
cargo build --release

# What can this machine do right now?
./target/release/pulseforge capabilities

# Record an attached process for 10 seconds (no admin rights needed)
./target/release/pulseforge record --pid 1234 --duration-ms 10000

# Launch and record a workload directly (no shell involved)
./target/release/pulseforge record --run ./target/release/pf-fixture-cpu -- --ms 3000

# Run the per-user daemon; recordings survive client disconnects
./target/release/pulseforge daemon &
./target/release/pulseforge agent start --pid 1234 --duration-ms 30000
./target/release/pulseforge agent status --session <id>   # from any client

# Or use the native GUI against the same daemon
./target/release/pulseforge-gui

# Re-render a recorded session as a report
./target/release/pulseforge report --session <session-dir>

# Compare two recorded sessions (observed deltas, honest caveats)
./target/release/pulseforge compare --a <dir1> --b <dir2> --markdown
```

Example report excerpt (full example in [`examples/`](examples/)):

```json
{
  "schema": "pulseforge.report/0",
  "session_id": "019a…",
  "mode": "counters",
  "target": { "pid": 4180, "os": "windows", "identity_recheck": "stable" },
  "metrics": {
    "process.cpu_core_pct": {
      "unit": "percent_of_one_logical_core",
      "availability": "available",
      "samples": 40,
      "mean": 99.2,
      "peak": 100.4,
      "first_sample_derivative": "unavailable"
    }
  },
  "warnings": []
}
```

## Design principles

1. **Counters always, stacks only when permitted.** Low-privilege process counters
   are the graceful baseline everywhere; CPU stack sampling is a separately
   negotiated capability with explicit privilege diagnostics — never a silent
   fallback or invented hotspots.
2. **No unwritten metric semantics.** Every number carries a unit, a source, a
   scope and an availability state. Windows working set is not called RSS;
   logical I/O is not called disk I/O.
3. **Identity is not a PID.** Process identity binds PID + creation time + boot
   context, rechecked during recording, so PID reuse can never contaminate a
   session.
4. **Local-first and least-privilege.** No admin elevation, no network listener,
   no symbol download, no telemetry. Recordings stay on your disk.
5. **Prove it, don't claim it.** Feasibility probes, fixtures, benchmarks and
   failure-path tests are part of each feature.

## Repository layout

```text
crates/
  pf-types/          typed IDs, units, metric taxonomy, errors, JSON writer
  pf-platform/       OS-neutral collector/query contracts and clocks
  pf-windows/        Windows counters, process identity, ETW feasibility probe
  pf-test-support/   deterministic fixture workloads used by tests and probes
  pf-cli/            the `pulseforge` binary (record / report / capabilities / probes)
docs/
  .local/                  internal working docs (architecture, roadmap, ADRs,
                           feasibility evidence) — private, not distributed
  (nothing else tracked here)
```

Internal working docs and the task ledger live in the private
`docs/.local/` working set.

## Development

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --target x86_64-unknown-linux-gnu   # Linux compile gate (runners execute it in CI)
```

Integration tests spawn real fixture processes and record them through the actual
Windows APIs — no mocks on the OS boundary. Failure paths (access denied, target
exit, identity mismatch) are asserted, not skipped.

## Contributing & support

- [Delivery record (v0.4.0)](DELIVERY.md) · [Contributing guide](CONTRIBUTING.md) · [Security policy](SECURITY.md) ·
  [Code of conduct](CODE_OF_CONDUCT.md) · [Changelog](CHANGELOG.md)
- Internal guides (permissions matrix, MCP integration, measurement
  methodology, security self-review) live in the private `docs/.local/` working set.
- [Open an issue](https://github.com//issues) — include the output of
  `pulseforge capabilities` and `pulseforge doctor` when reporting bugs.

## License

Licensed under the [MIT License](LICENSE). Revisit dual `MIT OR Apache-2.0`
before the 1.0 release (ADR-011, see the private decision records).
