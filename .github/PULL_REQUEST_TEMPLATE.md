## What

<!-- One-paragraph summary of the change. -->

## Task / roadmap item

<!-- PF-xxx id or "chore". Update the (private) ledger status for the affected row. -->

## Evidence

<!-- Commands run and observed results. For platform code: which OS/build,
normal user or elevated, integration test names. Mark NOT VERIFIED where you
could not validate. -->

```text

```

## Semantics check (metrics only)

- [ ] Unit, source, scope and availability documented per metric
- [ ] First-derivative unavailability, gaps and resets are explicit
- [ ] No cross-OS semantic conflation (working set ≠ RSS, logical ≠ storage I/O)

## Safety check

- [ ] No new unwrap/panic across file, OS, IPC or MCP boundaries
- [ ] New outputs/queues/captures are bounded
- [ ] No elevation, implicit shell, network fetch or injection added
- [ ] `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` green
