# Contributing to PulseForge

Thanks for helping build a profiler that tells the truth.

## Ground rules (non-negotiable)

These come from the project's engineering contract
the private architecture brief (`docs/.local/handoff/`, not distributed) and apply
to every contribution:

1. **Never counterfeit profiling data.** If a metric or stack is unavailable,
   represent it as unavailable with a structured reason. No zero-filling, no
   interpolation through gaps, no invented hotspots.
2. **Every number has a unit, source, scope and availability state.** If you add
   a metric, document its exact OS semantics in `pf-types` and the docs.
3. **No unwraps across trust boundaries.** Files, IPC, MCP input and OS APIs
   return typed errors; panics are bugs.
4. **Everything bounded.** Queues, responses, logs, captures and queries need
   explicit limits before the feature ships.
5. **Prove features on the real platform.** Windows-specific code is done when it
   runs on Windows with tests — cross-compiling is not validation. If you cannot
   validate on the required OS, say `NOT VERIFIED` in the PR.
6. **No silent elevation, no implicit shells, no network fetch, no injection.**

## Development workflow

```bash
git clone <your-fork>
cd pulse-forge
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same gates on Windows and Linux. Keep the build green at every
commit; the project follows small, reviewable, vertical-slice PRs.

### Task ledger

Work items use the `PF-xxx` identifiers from the private task ledger
(`docs/.local/TASKS.md`, not distributed). When you pick one up:

1. Add a row to the ledger with status `in-progress`.
2. Implement with tests (unit tests for semantics; integration tests that touch
   real OS APIs must spawn real processes).
3. Record evidence: commands run, observed results, known limitations.
4. Move the row to `done` and update `CHANGELOG.md`.

### Commit style

Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `chore:`, `perf:`),
imperative mood, one logical change per commit.

## Adding a metric

Metrics are a contract, not a convenience. Checklist:

- [ ] Canonical name `process.<thing>_<unit>` added to `pf-types::MetricId`.
- [ ] Exact semantic rule and caveats documented (see
      the private architecture brief §5.3 for the model).
- [ ] Availability is per-metric: permission failure degrades that metric only.
- [ ] Unit test for the derivation (deltas, rollover, first-sample
      unavailability, gap handling).
- [ ] Integration test asserting plausible ranges against a fixture workload.

## Reporting bugs

Open an issue with: OS build (`pulseforge capabilities` output), the exact
command, what you expected, what happened, and `pulseforge doctor` output if a
recording was involved. Never paste other users' process command lines or paths
without redaction — they can contain secrets.
