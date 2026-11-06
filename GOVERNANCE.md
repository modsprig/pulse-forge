# Governance

PulseForge is a single-maintainer project moving toward shared ownership.
This document describes how decisions are made and how to get involved
(PF-1310).

## Roles

- **Maintainer**: merges PRs, cuts releases, owns the
  security policy and the roadmap gates. Contact: see SECURITY.md for the
  security address; GitHub issues for everything else.
- **Contributors**: anyone with a merged PR. Contributors may review PRs;
  two approvals from distinct contributors count as review consensus for
  non-breaking changes.
- **Committers** (future): contributors with a track record of merged,
  review-quality work. Committers can merge non-breaking PRs without the
  maintainer.

## Decision process

1. **Behavior/measurement changes** follow the roadmap contract
   (the private roadmap, `docs/.local/`): every number
   has a unit, source, scope and availability state; features are proven on
   the real platform (`NOT VERIFIED` is an acceptable, honest state).
2. **Schema/contract changes** (CLI output, MCP tools, disk format) require
   a version bump of the affected schema label and an ADR describing the
   migration path. See the pinned contract fixtures in `crates/pf-cli/tests/schemas/`.
3. **Security-relevant changes** (launch policy, IPC, redaction) require
   maintainer review and an ADR.
4. **Dependency additions** require an ADR (see ADR-011/015 for the
   precedent) and must pass the cargo-deny policy.

## Roadmap gates

Milestones close through evidence, not calendar time. The live ledger is
the private task ledger (`docs/.local/TASKS.md`): each row carries its
   status and evidence links.
A gate closes when every row it contains is `done` (with honest
`NOT VERIFIED` markers where the environment cannot validate).

## Good first issues

New contributors should look for issues labeled `good-first-issue`:
typically documentation improvements, fixture workloads, error-message
clarity, and headless view-model tests — the parts of the codebase with
the shortest feedback loops and no OS-privilege requirements.

## Recognition

Contributors are credited in release notes. Substantial contributors are
invited into `MAINTAINERS.md` as the project grows.
