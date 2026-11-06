# Changelog

All notable changes to PulseForge are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
with a `0.x` pre-stable period aligned to the milestone table in the README.

## [0.9.0] — Unreleased

### Added — delete recordings from the GUI (session management)
- Recordings accumulated forever: there was no way to remove a session
  from PulseForge. The session browser now has a 🗑 button on every
  non-live card that opens a confirmation modal ("Permanently delete
  *name*? This removes the session record and its stored samples from
  disk. There is no undo."), and the confirmed delete runs off the UI
  thread with its typed outcome landing through a per-frame poll —
  success toasts the bytes freed, failure surfaces as an error toast and
  the card honestly stays. Esc cancels; live recordings refuse deletion
  daemon-side (`InvalidState` — stop first) even if a stale client asked.
- Layers: `pf_storage::delete_session_dir` (`.pfsession`-marker guard +
  typed `SessionNotFound`/`InvalidArgument`, returns bytes freed),
  `SessionSupervisor::delete` (terminal-only, deletes the directory
  outside the supervisor lock, falls back to the default sessions root
  for directories an older daemon run left un-adopted, idempotent at the
  record level when the disk copy is already gone), and the daemon
  `delete_session` op (`{"deleted":true,"bytes_freed":N}`).
- Locked by real-recording integration tests (delete removes record +
  store and reports bytes; a live recording refuses with `InvalidState`),
  daemon wire tests (shape + typed `SessionNotFound` for unknown ids),
  and GUI tests (arm → confirm → typed outcome lands; Esc disarms
  without dispatching).

### Added — drag-and-drop opens sessions offline
- Dropping a session directory (or any file inside one) onto the window
  opens its dashboard offline — no daemon, chunk store only — mirroring
  `pulseforge-gui --open`. Non-session drops surface an honest error
  toast ("no manifest.json") and never navigate. Locked by a headless
  test feeding a real `dropped_files` frame through `update()`.

### Added — Windows per-user installer
- The release zip now ships `install.ps1`: per-user install to
  `%LOCALAPPDATA%\Programs\PulseForge` (no admin), user-PATH update
  (idempotent, new terminals only), Start-Menu shortcut for the GUI,
  installed-binary verification, and `-Uninstall` that removes every
  trace it created. Verified in a sandbox: install → `pulseforge
  version` runs from the install dir → uninstall leaves nothing.
  `tools/package-release.sh` stages it and the GETTING-STARTED note
  points at it; README gained an Install section (zip+installer on
  Windows, .deb on Linux).

### Added — Live Processes shows which process is actually hot (CPU% + RAM)
- The process picker answered "what is running" but not "what is busy" —
  the #1 question when choosing a profiling target. Every daemon
  `list_processes` row now carries `cpu_pct` (percent of one logical core
  since the previous enumeration — Task-Manager semantics, backend-computed
  delta) and `rss_bytes`. IPC stays additive per the versioning policy:
  absent values are JSON `null`, and an older GUI simply ignores the keys.
- Windows: per-pid `GetProcessTimes` deltas against a process-global
  per-pid baseline plus `K32GetProcessMemoryInfo` working set on the same
  least-privilege `PROCESS_QUERY_LIMITED_INFORMATION` handle. The baseline
  is **merged per-pid** so a bounded enumeration can never clobber the
  baseline of rows it did not fetch, and negative deltas (PID reuse) stay
  honestly absent. Verified on real processes: the own-pid row carries a
  sane delta and positive working set within a few cycles.
- Linux: utime+stime from `/proc/<pid>/stat` (USER_HZ is a fixed 100)
  against the same merge-per-pid baseline; RSS from stat field 24.
  **WSL2-verified** — the first implementation read `/proc/<pid>` (a
  directory) instead of `/proc/<pid>/stat`; the Windows-side suite cannot
  catch that, and the real-Linux test did.
- Picker UI: hot-first sort is the new default (`CPU (hot first)` cycles
  with PID/name orders; rows without a delta sort last — absence never
  masquerades as idle), a CPU% column colored by load (<50% muted,
  ≥50% amber, ≥85% red), a compact RAM column, and CPU/RAM lines in the
  row tooltips. Locked by `cpu_sort_puts_hot_processes_first_and_absent_last`,
  `process_sort_cycle_covers_cpu_first`, the daemon wire-shape test
  `list_processes_carries_live_resource_fields`, and the platform tests on
  both OSes; pixels verified in the refreshed 01-home/01b-tree snapshots.

### Added — pprof export from the GUI
- The GUI could export Speedscope JSON while the validated pprof
  (profile.proto) writer existed only in the CLI. The dashboard header now
  has `📤 Export pprof`: the same `pf_analysis::export::pprof_export`
  engine, self-validated with the spec-conformance decoder before the file
  is written (the CLI's honesty contract), written as `profile.pb` into
  the session folder, toast reports the sample count and truncation if the
  cap engaged.

### Added — theme preference persists across launches; System follows the OS
- `Theme::System` used to paint identical dark visuals — a silent no-op.
  It now resolves against the OS: the Windows personalization toggle
  (`AppsUseLightTheme`, read through a new `pf-windows::theme` registry
  query) and GNOME's `color-scheme` on Linux (best-effort; anything
  unreadable resolves to the dark default — never a guess), cached for 5 s
  so per-frame calls cost nothing.
- The chosen preference persists in `<data_root>/gui-settings.json`
  (schema `pulseforge.gui-settings/0`); missing or malformed files fall
  back to dark and never block startup. Headless tests stay hermetic via
  `pf_gui::testing::hermetic_data_root()` (throwaway `PULSEFORGE_DATA`).

### Added — session folder + window title polish
- The dashboard header gained a `📂 Folder` button that reveals the
  session's directory in the file manager (Explorer / xdg-open, spawned
  detached with no console flash).
- The window title now reflects live state: `PulseForge v0.9.0 — ●
  Recording (session …)` while recording, the session state otherwise,
  plain title on Home. The title change is sent only when the string
  differs, so idle frames stay free.

### Fixed — single-key shortcuts no longer fire while typing (P0)
- Typing "cargo" into the run/exe field toggled Compare on the `c` and
  refreshed on the `r`: the `TEXT_FOCUS_OK` guard was initialized `true`
  and **never stored** — `ui_has_no_text_focus()` was a constant. The
  guard is now re-derived every frame from `ctx.wants_keyboard_input()`.
  Locked by an end-to-end headless test: a focused TextEdit suppresses
  the `D` shortcut, and a background click plus `D` opens diagnostics.

### Fixed — error toasts no longer look like success
- Every toast rendered a green pulsing dot, including "Export error: …".
  Toasts now carry a severity: errors get a solid red dot and danger
  stroke (`ToastKind::Error`), successes keep the green pulse. Color is
  never the only channel — the message states the outcome either way.

### Changed — zoomed dashboard refresh fetches samples once
- A zoomed refresh fetched the full sample list **twice** per cycle
  (once for the series, once re-fetched only to slice the CPU window).
  The CPU slice now cuts the already-fetched vector in memory — one pipe
  round-trip and one JSON decode per refresh, a visible stall removed at
  100k-sample scale.

### Changed — session cards surface why a recording stopped
- `stop_reason` was parsed from the daemon and never shown. Session
  cards now append `· stopped: <reason>` after the sample count when the
  daemon reports one — the browser tells you a session ended for
  `duration` vs `target_exited` without opening it.

### Removed — dead view-model code (audit findings)
- `PulseForgeApp::backend_error`, `HomeModel::process_filter`,
  `ActionOutcome::Stopped` (never constructed),
  `DiagnosticsModel::refresh_if_due_manual` + `now_duration`,
  `ProcessRow::has_window`, and `CompareModel::metrics_text` (the
  production UI renders `metrics_detailed`); the compare test now
  asserts the production path instead.

### Fixed — flaky CPU-accumulation test under load
- `cpu_counters_monotonic_and_plausible_when_accounted` relied on
  sibling tests burning CPU between its two snapshots; under a lighter
  or heavier parallel schedule the strict-increase assertion flaked.
  The test now spins 100 ms on its own thread between the snapshots —
  deterministic on both ends of the load spectrum.

## [0.8.1] — Unreleased

### Fixed — GUI no longer freezes while the daemon is unreachable ("Not Responding")
- **User-reported P0**: with the daemon not running, the window title went
  "(Not Responding)" and the UI hung for seconds at a time. Root cause: the
  named-pipe/UDS `connect` retry loop budgeted 50×100 ms (~5 s) per call and
  the Home view made two blocking daemon calls per refresh cycle — all on
  the UI thread.
- `pipe::connect` (Windows) and `uds::connect` (Linux) absence budgets cut
  to 12×50 ms (~0.6 s worst case); `ERROR_PIPE_BUSY` still waits properly
  on `WaitNamedPipeW`, since the server is provably alive there.
- All three view models now refresh off the UI thread: a new `AsyncSlot<T>`
  runs the computation on a worker thread (the in-flight flag prevents
  stacking duplicate workers), the frame never blocks, and the payload
  lands through a non-blocking `apply_refresh` poll. Home, Dashboard, and
  Diagnostics are all converted; action-driven immediate refreshes keep the
  synchronous path.
- Regression test: a `SlowDaemon` (~120 ms per call) asserts every UI frame
  stays < 100 ms while a refresh is in flight and that the payload still
  lands (both platforms).
- Behavioral note: a failed refresh now yields empty series instead of
  stale ones — honest "no data" beats silently-old data.

### Fixed — offline dashboards no longer wipe themselves after ~0.7 s
- `PulseForgeApp::offline` stored a `FakeDaemon` as its client, so the
  periodic cadence refresh clobbered the offline-loaded series with empty
  data one 700 ms cycle after opening a session. Offline apps now keep the
  real `OfflineClient` (hoisted to module scope), whose periodic refreshes
  re-read the immutable chunk store idempotently. Locked by
  `offline_dashboard_survives_periodic_refresh` (frames spanning two
  cadences, series + sample count must survive).

### Changed — no synchronous daemon IPC left on interactive paths
- Opening the diagnostics drawer or compare panel, clicking zoom/pan, and
  the "↻ refresh" button all called a blocking refresh on the UI thread
  (~1.2 s with a dead daemon; a full re-fetch even with a live one). All
  four now use a forced async `request_refresh_now` (in-flight-guarded);
  the payload lands through the per-frame apply poll.
- Cadence-stamp fix: a refresh request blocked by the in-flight guard no
  longer advances the cadence stamp, which used to delay the next refresh
  by a full cycle after the worker finished.
- Locked by `zoom_request_refresh_now_never_blocks` (SlowDaemon ~120 ms
  per call: the zoom path stays < 100 ms and the zoomed payload lands).

### Changed — the Start/Stop/Compare buttons no longer block the UI thread
- The last synchronous daemon round-trips on the UI thread are gone:
  `submit_attach`/`submit_run` (create + start ≈ 1.2 s with a dead
  daemon), `stop`, and compare runs now dispatch off-thread through
  dedicated slots. Validation stays synchronous (instant, no IPC) so a
  bad form still fails immediately; the daemon round-trip lands through
  a per-frame apply poll, and a successful start navigates to its
  dashboard exactly when the outcome arrives.
- Honest in-flight markers: the status line shows "starting…" while a
  start is in flight, the Stop button shows "stopping…", and a compare
  re-run clears the stale table instead of leaving old numbers up.
- Locked by `start_click_never_blocks_and_failure_lands` (dispatch
  < 100 ms with a dead daemon, typed TIMEOUT failure lands) and
  `successful_start_navigates_when_outcome_lands` (Home → Dashboard on
  the Started outcome, not before).

### Added — time axis on the dashboard charts
- The three dashboard charts answered "how much" but never "when":
  vertical time gridlines now mark the 25/50/75% window positions with
  relative timestamps (compact ns→ms/s/m formatting), the bottom-left
  unit annotation carries the window span (`unit bytes · window 30.0s`),
  and the hover tooltip includes the sample's relative time
  (`t+7.5s · cpu_core_pct · 42 percent`). Verified in pixels.

### Added — the daemon now starts automatically with the GUI (on demand)
- User-reported expectation: the GUI used to require a manually started
  daemon and rendered a wall of duplicated TIMEOUT text when it was not
  running. When a refresh finds the daemon unreachable, the Home view now
  launches the sibling `pulseforge` binary's `daemon` subcommand
  DETACHED (own process group / CREATE_NO_WINDOW — it outlives the GUI
  and flashes no console) entirely off the UI thread, waits for the pipe
  through the real client (~6 s bounded), and states each phase honestly
  in the status line: "auto-starting…" → "daemon auto-started —
  connected" or "auto-start failed: …" with the manual `pulseforge
  daemon` instruction kept visible. A 30 s cooldown prevents respawn
  loops; a missing sibling binary fails fast with the manual instruction
  (no PATH fallback — spawning an unrelated `pulseforge` would be
  surprising).
- The duplicated error wall is gone: when both list calls fail with the
  identical error, the text appears once.
- Onboarding step 1 updated: the daemon launches automatically.
- Locked by `daemon_down_triggers_autostart_attempt_with_honest_failure`
  and `daemon_down_error_is_not_printed_twice`; pixels verified in the
  07-daemon-down snapshot.
- Test-budget fix (removes the documented WSL timing flake class): the
  headless paint budgets now measure STEADY-STATE frames — frame 1 is
  the one-time egui warm-up (font atlas + cold page-ins, 30-40 ms on
  WSL/drvfs) and said nothing about paint cost. Steady-state frames
  measure 0.4-0.6 ms; isolated reruns were previously required.

### Fixed — the GUI no longer opens a console window alongside the app
- User-reported: launching `pulseforge-gui` also popped up a console —
  the binary was built as a CONSOLE-subsystem executable. It is now
  `windows_subsystem = "windows"` on Windows (PE Subsystem verified:
  WINDOWS_GUI). Console output from the binary is gone by design — the
  diagnostic surfaces are the UI and the typed exit codes remain for
  automation; the auto-started daemon still runs with no visible console
  (CREATE_NO_WINDOW).

### Fixed — the process picker no longer stops at 100 rows
- User-reported: the picker showed at most 100 processes. The GUI
  requested only 100 rows although the daemon protocol allows 1024
  (`op_list_processes` min(1024)); both platform backends stop the
  snapshot at the requested limit, and the worst-case JSON frame
  (~150 KB) is far under the 4 MiB cap. The picker now requests the
  protocol maximum (1024); rows render virtualized as before. This also
  repairs search: the filter runs client-side over the fetched rows, so
  a process beyond the first 100 was previously unsearchable. Locked by
  `process_list_beyond_100_is_reachable` (250 fake rows, tail searchable).
- Raised further to 4096 (user request): the daemon clamp and the GUI
  request both move to 4096 — worst-case frame ~0.6-1.2 MB, still far
  under the 4 MiB cap. Known per-frame filter/sort cost with the picker
  open (~1-3 ms at 4096 rows, release) is noted in the ledger.

## [0.8.0] — 2026-11-04

### Thread-count sweep for the sampler delta: inconclusive on this machine
- `sampler_overhead.rs` gained a thread-count sweep (1/4/16 full-length
  concurrent spinners, bare vs observed): deltas read 323/48/109 ms —
  non-monotonic and variance-dominated on this shared machine. Neither
  per-event nor per-session scaling resolves here; both timing-sweep
  threads are closed as requiring a quiet benchmark machine or
  kernel-side attribution. The numbers are recorded as raw evidence.
- Gates: WSL 270/0, Windows 277/0 (captured), clippy 0/0.

### strx resolution: a real spec bug found by inspection, fixed + fuzz-seeded
- `resolve_strx` (the DWARF5 indexed-string resolver) scanned the
  `.debug_str_offsets` section ITSELF for the string — but per spec
  those entries are OFFSETS into `.debug_str`. Any producer emitting
  indexed strings (strx/strx1-4, all 5 form sites) resolved names to
  garbage. Fixed to two-section resolution; a new strx seed (v5 CU
  with DW_AT_str_offsets_base + strx names + the two string sections)
  proves the arithmetic live (idx 0/1 resolve correctly) and puts the
  path under mutation (500 mutants x 6 seeds).
- Gates: WSL 270/0, Windows 277/0 (captured), clippy 0/0.

### 10-minute sustained soak on the durability changes
- Daemon recording a live fixture for 600 s at 250 ms with the stacks
  pipeline on: COMPLETED after stop; 20,000 stack samples persisted
  (the recorder cap hit with the honest `truncated: true` flag;
  frames_total 21,173); 0 `.partial` residue in the chunks dir (the
  atomic chunk writes held); all 7 session artifacts present; cadence
  ~4.2 samples/s throughout; daemon RSS plateau ~6 MB (growth then
  flatten, bounded).
- Evidence: `.local/soak10.sh` captured verification output. Caveat
  recorded honestly: the WSL VM cycled after completion, so the
  post-soak stacks CLI re-load of the tmpfs session copy could not be
  re-run — that load path is covered by pf-bench-stacks and the daemon
  stacks e2e.
- Gates: WSL 270/0, Windows 277/0 (captured), clippy 0/0.

### UI/UX iteration pass: pixel-reviewed themes, chart-fill fix, Light mode fixed
- A headless snapshot harness (`ui_snapshot.rs`) renders the REAL app
  into BMP images (software rasterizer over egui's tessellated meshes
  with font-atlas coverage sampling) — pixel-exact UI review of every
  view and theme without a window.
- REAL BUG: chart area fills used `Shape::convex_polygon` on concave
  series (any spiky/sawtooth line) — convex triangulation corrupted
  them into white overdraw. Replaced with a per-segment quad strip.
- REAL UX BUG: the Light theme was unreadable — `Theme::apply`
  clobbered every theme with the dark palette. Now per-theme
  (GitHub-light: white panels, #24292f text, #0969da accent), and the
  hardcoded dark fills in home/dashboard replaced with a theme-aware
  `Palette` (card_fill/card_stroke/text_strong/text_muted/accent).
- Chart gridlines at 25/50/75%, brighter metric-card labels, shortened
  top-bar session id, diagnostics drawer as a bordered panel, compare
  combos showing session image names, flame panel verified pixel-wise.
- Gates: WSL 271/0, Windows 278/0 (captured), clippy 0/0.

### Iteration 12: chart hover tooltips
- Hovering a chart shows a dark tooltip box (series name + value +
  unit) with a blue marker dot at the nearest sample — driven by
  pointer position, verified in pixels via the harness's virtual
  pointer (Event::PointerMoved).

### Iteration 9: session cards show when each session ran
- `SessionId::unix_ms()` extracts the 48-bit UUIDv7 millisecond
  timestamp embedded in the identity itself (unit-tested against the
  wall clock, ±1 s) — and the Home session cards render relative ages
  ('just now / N min ago / N h ago / N d ago') in the meta line. Test
  fixtures with zero-timestamp ids honestly render as decades old.

### Iterations 10-11: quantitative charts + recording indicator
- Chart gridlines now carry VALUE LABELS (the data value at each
  25/50/75% gridline, small muted type at the left edge) — intermediate
  chart values are readable, not just min/max.
- The dashboard State card shows a '●' recording-indicator prefix when
  the session is live.
- Gates: WSL 271/0, Windows 278/0 (captured), clippy 0/0.

### Iteration 7: first-run onboarding empty state
- The no-sessions Home now renders an onboarding card: 'No sessions
  yet' + three numbered steps (start the daemon / pick a process and
  press Start / open the finished session) + keyboard-shortcut hints —
  replacing the one-line dead end a first-time user used to see.
- Gates: Windows 277/0 (captured; cpu_counters parallel-load flake
  passes isolated), WSL 271/0, clippy 0/0.

### Iteration 8: GUI launch-and-record (CLI `record --run` parity)
- REAL PARITY GAP: the GUI could only attach to an already-running pid,
  while the CLI could launch-and-record a NEW workload. The daemon's
  create_session already accepted `run` — the GUI just never used it.
- `DaemonClient::start_run` (exe + args + duration/interval/label),
  implemented on the pipe client (create_session with `run` →
  start_recording), the FakeDaemon (call recording for test asserts),
  and the OfflineClient (typed UnsupportedPlatform error).
- The attach bar gains an attach/run mode toggle; run mode shows
  executable-path and args inputs; Start dispatches by mode. Verified
  in pixels.
- Gates: WSL 271/0, Windows 278/0 (captured), clippy 0/0.

### Multi-CU .debug_info: cross-CU inline refs under mutation
- The .debug_info seed now carries TWO compile units — CU2 with its own
  abbrev table (same codes, different shapes) and a CROSS-CU inline
  whose abstract_origin points at CU1's subprogram by absolute offset.
  Asserted: both CUs contribute inline ranges and CU2's chain resolves
  to CU1's symbol — multi-CU walking, per-CU abbrev selection, and
  absolute-ref name resolution verified together, for v4 and v5.
- Building it surfaced a seed bug (a malformed abbrev entry missing the
  has_children byte); the parser degraded honestly at the unknown code
  that followed — no panic, no invented data — and the fixed seed
  verifies both CUs in both versions.

### Seed-coverage completion: implicit_const + the v2 line table
- The abbrev seed gained an implicit_const attribute (form 0x21): the
  attribute's value rides in the abbrev table and the DIE body omits
  it — `read_form`'s implicit_const branch was previously un-exercised.
- A third hand-built seed adds DWARF v2 line tables — the oldest
  version the parser accepts, with a different header (no
  max_ops_per_inst, no address_size/segment fields) and v2-era file
  entries, exercising the version-conditional paths the v4/v5 seeds
  skip. The line-parser version matrix is now v2/v4/v5
  mutation-covered; both new surfaces held clean.
- A duration sweep of the stack-sampler delta (does it amortize down
  for longer sessions?) is INCONCLUSIVE on this shared machine — raw
  numbers recorded as evidence, not a curve.
- Gates: WSL 269/0, Windows 277/0 (captured), clippy 0/0.

### Continuous optimization + bughunt: IPC/session fuzz, symbolization index
- PF-1202 grows two more hostile-input suites: the daemon IPC frame
  codec + handshake gate (`fuzz_ipc.rs` — ~66k mutated frames with
  hostile length prefixes, ~30k mutated handshakes) and the session
  stacks loader (`fuzz_stacks_io.rs` — ~3k corrupted `stacks.jsonl`
  files plus 12 structural traps). Both surfaces held clean: typed
  errors everywhere, no panics, allocations capped. The `/proc/<pid>`
  stat parser was audited against hostile process names — already
  correct (last-`)` rule).
- Symbolization's last quadratic is gone: the per-IP linear scan over
  ALL of a module's DWARF inline ranges (tens of thousands in a
  dev-profile library) is now a bucketed `InlineIndex` (64 KiB buckets
  built at load; >1 GiB ranges to a linear overflow list; >64 GiB
  claimed spans skip the bucket table — bounded against hostile
  headers). Result semantics are exactly the old scan's, locked by the
  inline_expansion suite. Measured: **54.43 -> 3.45 ms (-94%)** for
  20k ranges x 5k unique IPs (`inline_bench`).
- Gates: WSL 255/0, Windows 266/0, clippy 0/0.

### 1M-sample icicle stress: the last PF-910 remainder closes
- `pf-gui/tests/flame_stress.rs`: 1,000,000 stack samples (the stress_1m
  shape mix) built in memory and driven through the REAL offline app's
  stacks panel. Asserted: the flame tree stays bounded (≤ 2000 nodes,
  honest omission counts), painted rects stay within the cap, half-window
  re-aggregation filters exactly the right samples, the bottom-up
  direction is exact at scale. Timings are printed for the ledger and
  never asserted against (release: 1M aggregate 268 ms WSL / 352 ms
  Windows; paint p95 2–15 ms).
- The panel's re-aggregation now streams: `StackTrie::aggregate_iter` /
  `aggregate_bottom_up_iter` consume borrowed samples directly, so the
  GUI no longer clone-collects the selected sub-batch (a deep copy of
  every selected stack on every pan/zoom at recorder scale). Measured
  −40% at 1M (debug); the release aggregate now sits at the pure-trie
  floor from stress_1m.
- The PF-910 remainder "a 1M-frame icicle render test is P9 proper" was
  gated on the GUI flamegraph panel and is now closed — no remainder
  remains on PF-910.
- Gates: WSL 250/0, Windows 261/0, clippy 0/0. (Known flake class, not
  data-related: daemon-stop e2es occasionally miss polling deadlines
  under full-workspace parallel load; isolated reruns pass.)

### Optimization marathon, third pass: scale and self-overhead
- 1M-sample aggregation re-measured after the FxHasher/hoisted-path-set
  work: **449 → 249 ms (−45%)** (`stress_1m`, release, WSL2) — the
  trie gains hold at scale, now evidenced.
- Observer-overhead study re-run with current code
  (`tools/overhead-study.sh`, now platform-portable): Linux/WSL2
  **0–3.1%** on a 2 s workload (was 3.2–8.3% at the first study; the
  250 ms cell shows zero measurable overhead) — the persist streaming
  and record-path work landed where it counts, in the profiler's own
  footprint. Windows native stays at 3.1–7.9%, unchanged, and is
  durability-bound: the median is identical at 250 ms and 1000 ms
  intervals, i.e. per-session fsyncs for crash recovery, not per-poll
  cost — deliberately not traded away for the number.
- `pf-bench-stacks`' write section now mirrors the REAL streaming
  persist shape (reused scratch + 1 MiB BufWriter, fsync excluded)
  instead of the retired one-giant-String pattern.

### Tool pass: the interactive surface measured, the parse floor cut
- Measured the user-facing surface on real recorded sessions (release
  builds): every CLI command is dominated by process spawn — `<10 ms`
  from a native filesystem (~30 ms under WSL drvfs is binary-load
  overhead, not the tool), `stacks`/`report`/exports 60–70 ms for a real
  20k-sample session. The one cost that scales with session size is the
  shared JSON parse floor.
- `pf_types::json` parse floor −19% (145.3→118.4 ms per 37.8 MB):
  per-segment UTF-8 re-validation dropped — provably redundant because
  `parse` takes `&str` (the whole buffer is already valid UTF-8) and
  string/number segment delimiters are ASCII bytes, which never occur
  inside a multi-byte sequence, so every segment is codepoint-aligned
  valid UTF-8. The invariant is documented at the `unsafe` site, locked
  by a new multi-byte-across-segments unit test, and remains
  fuzz-covered (fuzz inputs are `&str`-filtered). Object/array vectors
  are pre-sized (was a 4→8 growth per parsed object).
- `stacks_io` load path: each frame object is walked ONCE (five `.get()`
  calls were five linear scans per frame) and the top-level object once.
  Full load −22% (214→167.7 ms; −37% vs the 265.5 ms baseline at the
  start of the optimization work).
- Gates: WSL 249/0, Windows 259/0, clippy 0/0 both platforms.

### PF-1202 extension: the binary parsers face the fuzzer
- The seeded-mutation fuzz method (deterministic xorshift PRNG; byte
  soups, bit flips, truncations, duplications, splices; property =
  panic-freedom + bounded termination + honest empty results) now covers
  the parsers that read arbitrary files from disk:
  - `pf-linux/tests/fuzz_parsers.rs` (7 suites, ~85k cases): ELF symtab,
    DWARF `.debug_abbrev`/`.debug_info`/`.debug_line`, perf ring regions,
    `/proc`-maps text, and `resolve_ip_chains` against overlapping,
    unsorted, zero-length and backwards map sets. The ELF/DWARF seed is
    hand-built and structurally valid (symtab + inline chain + line
    program) — seed validity is asserted in the suite, so mutations start
    from a parse that provably works.
  - `pf-windows/tests/fuzz_pe.rs` (4 suites, ~12k cases): PE32+ identity
    through the debug directory + RSDS CodeView blob, MSF 7.00 PDB
    matching (GUID+age), and `resolve_pdb` against mutated references
    with hostile path bytes — a non-matching file is never accepted.
- One real bug found and fixed: the perf ring parser **panicked** on a
  record whose header size lied about its body (byte-indexing past the
  buffer end); the slice-based parser now keeps-what-parsed, with a
  regression test. The ELF/DWARF/line/PE/MSF parsers survived clean.
- Suites are target-gated so mirrored stub compiles stay clean
  (`#![cfg(target_os = "linux")]` / `#![cfg(windows)]`).
- Gates: WSL 248 tests / Windows 259 tests, 0 failures; clippy 0/0.

### Hot-path optimization pass: measured, then optimized
- New `pf-bench-stacks` microbenchmark (pf-test-support, both platforms):
  median-of-5 timings for the four paths every stacks session touches —
  persist write shape, the real `load_session_stacks_timed` loader, a
  JSON-parse-only floor section, and top-down/bottom-up aggregation.
  Baseline (WSL release, 18k samples × ~15.5 depth, 37.8 MB): write
  58.6 ms, load 265.5 ms, aggregate 19.3 ms, bottom-up 38.7 ms.
- `StackTrie`: the per-sample path `HashSet` is allocated once per batch
  (was one set per sample), trie-internal maps hash with a zero-dep
  FxHasher (rotate-xor-multiply; `HashMap`/`HashSet` semantics and all
  honesty counters unchanged), and `aggregate_bottom_up` walks frames in
  reverse instead of cloning every sample through `flip()`. Measured:
  aggregate −23%, bottom-up −55% (WSL), −23%/−59% class gains confirmed
  on Windows.
- Loader (`stacks_io`): disk order (callee-first) is reversed in place
  and consumed via the new `SampledStack::from_resolved_owned` — the
  per-frame string re-clone and the per-line `flip()` clone are gone.
  Load path −13..−21% (run-noisy); the remaining floor is the shared
  `pf_types::json` parser (~140 ms per 37.8 MB), now visible via the
  parse-only bench section.
- perf ring parse extracted into a pure, unit-tested `parse_ring`:
  exact sample round-trip decode, and truncated/malformed records now
  keep-what-parsed instead of panicking (the old byte-indexing parser
  would panic on a record whose header size lied about its body). The
  drain scratch buffer and the sample vec are reused across ticks, and
  `PF_PERF_DEBUG` is read once per process (was a lock-taking env read
  on every 100 ms tick). Throughput: 0.52 ms per 18k-sample ring
  (~35M samples/s) via the `#[ignore]`d timing bench.
- `resolve_ip_chains` binary-searches the maps snapshot on a verified
  sorted+disjoint fast path; unsorted/overlapping synthetic maps keep
  the exact first-match linear scan.
- Persist: `stacks.jsonl` streams through a 1 MiB `BufWriter` from one
  reused line scratch — a session-sized string (~38 MB for an 18k-sample
  session) no longer accumulates in memory — and leaf attribution reuses
  the already-resolved per-IP chains (the second full resolution pass is
  gone).
- Gates after: WSL 241 tests / Windows 255 tests, 0 failures; clippy 0
  findings on both platforms; the daemon stacks e2e passes unchanged on
  real recorded data. (Note for future gate runs: the e2e's file:line
  assertion requires the dev-profile workload — running the suite with
  `--release` strips `.debug_line` from the fixture and fails that one
  assertion by construction.)

## [0.7.0] — 2026-11-01

### Release artifacts: zip, .deb, SBOM — verified for 0.7.0
- `dist/pulseforge-0.7.0-windows-x64.zip` (5.0 MB) packaged and
  SHA-256-verified; the EXTRACTED binary smoke-tested end-to-end:
  `version` -> `capabilities` (counters available, ETW honestly
  NOT_IMPLEMENTED without elevation) -> records a real fixture to
  COMPLETED -> `report` parses.
- The Linux .deb recipe is now REPEATABLE: `tools/build-deb.sh` (FHS
  layout, control + postinst, `--root-owner-group`). It stages on the
  native filesystem because drvfs reports mode 777 and chmod does not
  stick there - dpkg-deb refuses such a control directory. Built
  `pulseforge_0.7.0_amd64.deb` (5.9 MB); installed via `dpkg -i`, the
  installed `/usr/bin/pulseforge` records a real workload to COMPLETED,
  and `dpkg -r` removes cleanly.
- `dist/SBOM.json` regenerated for 0.7.0 (423 components, all within
  license policy).

### Continuous bughunt pass 3: recovery + metrics fuzz — one real bug fixed
- `fuzz_recovery.rs` (pf-daemon): 4k mutated manifests through the
  daemon-start recovery scan plus a clean-state contract test. Contract
  asserted: recovery never destroys data files, never rewrites terminal
  sessions, ignores non-session dirs, is idempotent, and answers typed
  errors on hostile manifests.
- **Real bug found and fixed**: a torn multi-byte manifest — a daemon
  killed mid-write splits a UTF-8 sequence, which is the NORMAL crash
  leftover — hit the `read_to_string` error path and was silently
  skipped: no recovery note, indistinguishable from a missing manifest.
  Recovery now marks non-UTF8 manifests exactly like unparseable ones.
- `fuzz_metrics.rs` (pf-types): 30k mutated snapshot documents through
  `CounterSnapshot::from_json` — Ok or typed error, and everything
  accepted is canonically stable (re-serialize -> re-parse -> equal);
  absent counters stay honestly absent.
- Gates: WSL 262/0 (one captured run; timing-bound tests flake under
  full-workspace parallel load and pass isolated), Windows 273/0,
  clippy 0/0.

### Continuous bughunt pass 4: service dispatch under fire
- `fuzz_service.rs` (pf-test-support): 6k mutated request frames through
  `DaemonService::dispatch` against a REAL supervisor and platform
  backend. Contract asserted: every response is well-formed (echoed id —
  absent ids echo as 0 — with exactly one of result/error), hostile
  start/create attempts leave no recording running, session dirs on disk
  are bounded by create attempts, and unknown or path-shaped op names
  answer typed INVALID_ARGUMENT errors. The dispatch layer held clean.
- The recovery fuzz's disk churn was trimmed (1.5k mutated manifests,
  71 s -> 4.5 s per run, same seeds), which removed the load contention
  behind the parallel-run e2e timing flakes — the full workspace now
  passes in one run on both platforms.
- Gates: WSL 264/0, Windows 275/0 (captured full runs), clippy 0/0.

### Continuous bughunt pass 5: campaign-scale fuzz + GUI zoom/search stress
- Every fuzz suite now takes a `PF_FUZZ_SCALE` env multiplier (default 1,
  clamped 1..=64) — deep campaigns expand the corpus without recompiling;
  gate runs stay at scale 1.
- An 8x campaign (~1.5M hostile cases) found + fixed a test-harness bug:
  fuzz_chunks' "invented data" bound was hardcoded for the 1x corpus
  while the corpus scales with the multiplier, so real records were
  flagged as inventions. The bound now tracks the written count; at 8x
  the chunk mutation suite ran 24k mutated chunk files clean (683 s).
- The GUI stress gained the two un-stressed interactions at 1M scale —
  zoom (re-rooting a subtree clone) and active search (per-rect highlight
  scan): both keep the painted-rect cap, paint p95 9–10 ms.
- One parallel-load flake (pf-windows cpu-counters monotonicity) passes
  isolated — same timing family as the documented e2e flakes.
- Gates: WSL 264/0, pf-windows 21/0 + 4/0 + 6/0 isolated, clippy 0/0.

### Stack-sampler marginal cost: measured, and the shape is surprising
- `sampler_overhead.rs` (pf-linux, #[ignore]d study): a deterministic
  spinner's wall time with the per-thread sampler attached (production
  100 ms drain cadence) vs bare. WSL2 release: bare 540 ms, observed
  603 ms (+63.5 ms, 11.8%, ~3.3k samples/s).
- The surprise: a 10x coarser period shows the SAME delta (+61 ms,
  11.3%, ~0.4k samples/s) — the overhead does NOT scale with sample
  volume. It is fixed per-session cost (event setup, ring mmap, kernel
  accounting, drain-thread scheduling interference), not per-sample
  work. Deeper attribution needs kernel-side profiling — out of scope;
  the finding itself is the ledger value: stack sampling on this WSL2
  class costs ~12% wall to a fully-loaded 2-thread target regardless
  of period (the counters-only path is 0-3.1%).
- Gates: WSL 268/0, Windows 277/0 (captured), clippy 0/0.

### The v5 .debug_info CU faces the fuzzer too
- The seed builder is version-parameterized: v4 and v5 CU headers (v5's
  unit_type/addr_size/abbrev_offset field order), both sanity-asserted
  (inline chain + line table resolve per version), both swept by the
  mutation and truncation suites, and the DIE body gained a `udata`-form
  attribute for deeper `read_form` coverage.
- Building the v5 CU surfaced the version-dependent DIE-offset
  subtlety: the subprogram DIE sits at 12+8=20 in v5 (12-byte CU
  header) vs 19 in v4 — the seed's abstract_origin ref_addr is now
  computed per version, exactly as real compilers emit it.
- Gates: WSL 268/0, Windows 277/0 (captured), clippy 0/0.

### DWARF v5 line tables face the fuzzer — and it caught a real bug
- `fuzz_parsers.rs` gained a second hand-built seed: a structurally
  valid DWARF v5 `.debug_line` unit with format-described directory/
  file entries (`line_strp` + `udata` forms) — the v5 path real rustc
  binaries use, previously exercised only un-mutated.
- Building the seed found + fixed a REAL parser bug: the v5
  file-register DEFAULT was the v4 convention (`file_base + 1`) where
  v5 is 0-based — a spec-legal v5 program that never emits `set_file`
  silently resolved to the SECOND declared file. Initial and reset
  values are now version-aware (v4: 1, v5: 0); v4 behavior is
  byte-identical, and `inline_expansion`'s real rustc v5 binaries
  re-verified.
- The v5 seed's mutations now hammer `parse_v5_entries` and
  `read_line_form` (500 mutants × 6 seeds).
- Gates: WSL 268/0 (captured), pf-linux 40/0, pf-windows 21/0 isolated
  (one cpu-counters flake under parallel load passes isolated — the
  documented timing family), clippy 0/0.

### Chaos sweep: SIGKILL at 7 points, the durability story held
- `crash_safety.rs` (pf-cli): SIGKILLs the daemon at 7 swept points
  (2.2-4.0 s) spanning the recording AND finalize windows, restarts so
  recovery runs, and asserts what a crash victim relies on: the manifest
  always parses and lands terminal-or-RECOVERED_PARTIAL, `stacks.jsonl`
  is complete if present (every line parses — the atomic rename held
  under the actual kill conditions), summary/maps parse when present,
  final chunks parse their headers, and `pulseforge report` renders the
  post-crash session without a daemon. All 7 kills recovered.
- Write-surface audit: chunks and manifests were already atomic
  (`.partial` -> rename) and samples.jsonl is append-only with a lenient
  reader — after the stacks-artifact fix, everything is either atomic or
  lenient.
- Gates: WSL 267/0, Windows 277/0 (captured), clippy 0/0.

### Crash-safety fix: stacks artifacts are written atomically
- A bughunt pass over the crash story found `persist_stacks` writing
  its three artifacts (`stacks.jsonl`, `maps.json`,
  `stacks-summary.json`) non-atomically — a daemon killed mid-write
  left a torn `stacks.jsonl`, and the loader honestly rejects any
  unparseable line: one torn tail line took the whole session's stacks
  analysis down.
- All three artifacts now go through tmp + fsync + rename (the
  recovery manifest's pattern): a crash leaves only a `.partial` tmp
  file as evidence, never a torn final artifact. Contract locked by
  the persist unit test (no `.partial` residue on success, all three
  finals present) and the daemon stacks e2e on real recordings.
- Gates: WSL 266/0, Windows 277/0 (captured), clippy 0/0.

### Property-tested aggregation + hybrid children: 1M aggregate -27%
- `property_stacks.rs` (pf-analysis): deterministic random corpora (6
  seeds x 50k stacks, top-down AND bottom-up) against the aggregation
  engine's exactness contracts — exact weights, exact sample counts,
  inclusive >= exclusive on every node, root sums == total, recursion
  cuts EXACTLY equal to samples with a repeated key (on a tier sized to
  stay under the node cap), honest truncation on over-cap tiers
  (totals exact, flag reported, no invented cuts), and constructor
  equivalence between `from_resolved` and `from_resolved_owned`.
- With that net in place: `Node.children` is now hybrid — a linear-scan
  Vec for <= 8 children (deep-chain nodes hold 1-3) that promotes to the
  FxHash map for wide fan-outs. Every renderer sorts by inclusive weight
  afterward, so iteration order stays independent. Measured: **1M-sample
  aggregate 249 -> 181 ms (-27%)**, 18k aggregate 14.9 -> 14.1 ms.
- Gates: WSL 266/0, Windows 277/0, clippy 0/0.

### Continuous optimization pass 2: chunk bench + reject-only fast scanner
- The chunk store was measured (new `chunk_bench`): write 1.28M records/s
  (10.7 MB per 100k), full-range read 8.4M records/s, envelope ~12 ms per
  100k — three-plus orders of magnitude above any real cadence, so the
  recording and read-back paths need no optimization (numbers recorded).
- The session loader gained a reject-only fast path: a strict byte
  scanner for the persist writer's exact line shape (fixed key order,
  plain u64s, escape-free strings) that skips the JSON tree entirely —
  ANY deviation falls back to the full parser, so the fast path can only
  reject, never misparse. Field semantics mirror the fallback exactly
  and are locked by a 30k-case differential fuzz against the full
  parser — which caught and fixed three real scanner bugs en route
  (comma-comma ips lists, leading-zero numbers, the 20-digit u64::MAX
  cap). Escaped strings and malformed lines still take the full parser
  (typed errors unchanged).
- Load path: **167.7 -> 79.0 ms (-53%)**; **-70% vs the 265.5 ms
  baseline** where the optimization work started.
- Gates: WSL 258/0, Windows 269/0, clippy 0/0.

## [0.5.x/0.6.x] — consolidated additions that shipped in those releases

### PF-1003 (short-thread slice): 100 ms thread rescans and coverage accounting
- The stack sampler's drain thread now rescans `/proc/<pid>/task` on
  EVERY tick (100 ms; previously ~1 s) — a bounded read_dir per tick —
  shrinking the blind window for born-and-died threads tenfold. The
  cadence change is invisible to the sampler API; new threads are opened
  and started as before.
- `stacks-summary.json` gains `threads_with_samples`: the number of
  distinct tids with persisted samples — honest per-thread coverage,
  reported rather than implied.
- Verified in WSL2: three SEQUENTIAL worker threads, each spinning only
  400 ms (under the old 1 s cadence, well over the 100 ms tick), are ALL
  sampled — four distinct tids including the leader. The daemon e2e
  asserts the new coverage field for the single-threaded bench-spin
  target.
- Remainder: threads shorter than one 100 ms interval may still be
  missed — a /proc scan cannot observe them at all (perf_event inherit is
  the kernel-level alternative and carries its own ring-buffer
  tradeoffs); CPU-migration/NUMA effects unexamined.

### [0.5.x/0.6.x] — PF-910 (aggregation stress slice): 1M stacks, measured
- `pf-analysis/tests/stress_1m.rs`: 1,000,000 synthetic stacks across the
  roadmap's named stress shapes — deep recursion, wide fan-out, no-symbol
  leaves, 8-module chains, broken references — through the StackTrie
  engine: **aggregate 449 ms** (release, WSL2). Honesty invariants hold
  exactly at scale: total weight 1,000,000, recursion cuts exactly
  n/5 = 200,000, node cap un-tripped, unresolved weight tracked.
- Query path: a generated 617 MB `stacks.jsonl` (1M lines) through the
  real `pulseforge stacks` CLI — 20.06 s end-to-end (dev build: JSON
  parse of 617 MB + full aggregation + bounded JSON render), with
  recursion cuts and node-truncation flags exact.
- Remainder: GUI render stress against the flamegraph panel is P9 proper
  (the panel itself is the PF-906 slice).

### [0.5.x/0.6.x] — PF-1203: 60-minute soak VERIFIED
- The full sustained-capture soak completed and passed verification
  (tools/verify-soak.py: SOAK VERIFICATION PASSED): the daemon
  self-recorded at 250 ms for 3600 s; 13,954 samples recorded (cadence
  floor 12,960 exceeded); memory plateau confirmed (first-half 5.1 MB →
  last-half 8.7 MB — within the runtime-cache constant, no linear
  growth; final reading 4.3 MB); 3,033 sealed chunks with 0 .partial
  residue; terminal state COMPLETED; report size bounded.
- PF-1203's bounded slice is closed.

### [0.5.x/0.6.x] — PF-1301: Linux .deb installer (WSL2-verified)
- `pulseforge_0.5.0_amd64.deb` (575 KB): Debian package with control file
  (Package/Version/Architecture/Depends), the release binary at
  `/usr/bin/pulseforge`, built with dpkg-deb on the ext4 filesystem (the
  drvfs mount maps everything 777 — dpkg-deb refuses that, so the
  package tree is copied to ext4 for correct permissions).
- Verified end-to-end in WSL2: `dpkg -i` installs; the installed binary
  reports version, runs a real daemon recording (8,913 samples from the
  fixture), the stacks CLI reports correct hot-loop attribution
  (spec_next 3515, forward_unchecked 1397), and `dpkg -r` removes it
  cleanly.
- The MSIX (Windows) installer needs a signing certificate — P13 proper.

### [0.5.x/0.6.x] — PF-907 (bounded slice): bottom-up stack view
- `pf_analysis::stacks::StackTrie::aggregate_bottom_up`: frames reversed
  before insertion, so the trie's root is the LEAF — exclusive weight
  lands on the outermost frame (entry points) and the tree walks
  callees-first. Cycle cuts and node caps behave identically on reversed
  stacks (reversed recursion is still recursion — counted).
- CLI: `pulseforge stacks --bottom-up`; the output records
  `direction: bottom-up|top-down`. Round-trip verified: top-down
  exclusive = the hot leaf, bottom-up exclusive = the entry point, same
  total weight and inclusive totals in both directions.
- Engine test covers the leaf/root role swap, inclusive totals, and
  reversed-recursion cut counting.
- WSL note: the GUI render-budget tests on the shared drvfs target dir
  are warm-cache-sensitive; from ext4 they pass 8/8 and on Windows
  252/0. This is an environment measurement artifact, not a code
  regression.

### [0.5.x/0.6.x] — PF-909: speedscope independent-viewer round-trip VERIFIED
- The speedscope CLI (v1.21.0 — the reference implementation behind
  speedscope.app) was installed on the verification host and imported the
  exported speedscope document through its full stdin pipeline (parse +
  conversion to speedscope's internal profile model) without error, from
  a real daemon-recorded session (6,684 samples, 24 frames).
- Combined with the go tool pprof round-trip for the pprof export, BOTH
  independent formats now have viewer-level verification.

### [0.5.x/0.6.x] — PF-1105: function-shift compare over stack sessions
- `pf_analysis::stacks::compare_top_functions`: diffs two aggregated
  stack profiles by exclusive weight. Every function in either top-set
  is reported with baseline/candidate weights and a signed delta —
  gained functions carry 0 on the baseline side, lost functions carry 0
  on the candidate side, so gains and losses are both honest (never
  averaged away). Sorted by |delta| descending.
- New CLI surface: `pulseforge stacks --session A --compare-session B`
  renders the shift table (`function_shifts`, `compared_against`) in the
  stacks-analysis JSON.
- Verified in WSL2 with a two-session e2e: a cold_fn-dominated baseline
  vs a new_hot-dominated candidate reports gained (+80), lost (−50), and
  the stable function's −30 shift exactly.

### [0.5.x/0.6.x] — PF-1210: binary reproducibility measured
- Same-commit relinks of `pulseforge.exe` (release, 1,167,360 B) were
  byte-compared: **exactly 5 differing regions / 21 bytes (0.0018%)** —
  the COFF TimeDateStamp (1 byte), the three debug-directory entries'
  TimeDateStamps (3 bytes), and the RSDS GUID (15 of 16 bytes). All other
  1,167,339 bytes are identical across relinks, and content-identical
  no-relink states are byte-identical.
- Diagnosis: the MSVC linker injects fresh build stamps and a fresh PDB
  GUID per link. Fix path recorded for P13: `/Brepro` +
  `SOURCE_DATE_EPOCH` in the release config yields a fully deterministic
  binary (the 21-byte delta is the entire gap).

### [0.5.x/0.6.x] — PF-1009 (cross-check slice): Linux perf agreement on the same fixture
- `perf` (linux-tools 6.8.0-147) runs in WSL2 and cross-checks our
  sampler on the identical fixture: `perf record -F 999 -g` captured
  3,012 samples with 0 lost; our sampler recorded 14,304 on the same
  target.
- Dominant-function agreement: `spec_next` is #1 in BOTH (perf 45.95%,
  ours 39.77%), and the same RangeIterator-loop function set
  (forward_unchecked, Iterator::next, spin_ms) dominates in both.
- Attribution-method differences recorded honestly: per-boundary
  percentages differ because our sampler attributes through
  inline-expanded DWARF chains (surfacing inlinees like wrapping_mul and
  overflowing_add that perf's nearest-symtab-symbol report folds into
  enclosing symbols), and the effective rates differ (~5 kHz vs 999 Hz).
- PF-1009's cross-check sub-gate is closed **on WSL2**; the bare-metal
  confirmation of the same cross-check remains the open part of the
  gate.

### [0.5.x/0.6.x] — PF-1003 (CPU-attribution slice): per-sample CPU + migration examination
- `pf-linux/src/perf.rs`: `PERF_SAMPLE_CPU` added to the sampling type
  (sample_type 103). The kernel's `perf_output_sample` field order was
  verified against the kernel source before touching the parser: cpu+res
  is output AFTER time and BEFORE callchain — the parser reads it there,
  so a wrong order would break every callchain and be caught immediately.
- `StackSample.cpu` flows through the recorder into `stacks.jsonl` (a
  `cpu` field per sample line) and `stacks-summary.json` reports
  `cpus_observed` (distinct CPUs across the session).
- Migration examination (the named PF-1003 remainder): a WSL2 e2e runs a
  three-spinner fixture and reports per-tid CPU sets — this run: 44,453
  samples, all three threads resident on CPU 0, zero migrating tids (an
  honest scheduling fact of the WSL2 scheduler, printed per-tid; CPU ids
  sanity-checked).
- Remainder: threads shorter than one 100 ms interval may still be
  missed (a /proc scan cannot see them at all — perf_event inherit is the
  kernel-level alternative and carries its own ring-buffer tradeoffs).

### [0.5.x/0.6.x] — PF-909: independent-viewer round-trip VERIFIED (go tool pprof)
- The Go toolchain (1.27.3) was fetched onto the verification host and
  `go tool pprof` — google/pprof's own implementation, fully independent
  of our encoder — was run against a real exported document from a
  daemon-recorded session.
- Parse level: `-raw` decodes every table (13,433 samples with full
  location chains; string/function/mapping tables intact).
- Semantic level: `-top` reproduces the known-hotspot ordering from our
  own aggregation (spec_next 38.7% flat, forward_unchecked 29.4%,
  Iterator::next 9.2%, spin_ms 8.3%…).
- The export was hardened in the process: mappings now carry real
  memory_start/memory_limit from the session's maps.json snapshot and
  locations carry ABSOLUTE addresses (load base + module-relative
  offset) — so go pprof's address-based re-symbolization cross-checks our
  DWARF attribution (the default `-top` shows the same inlinee leaves our
  frames attribute, e.g. std's `cold_path` intrinsic for the fixture's
  clock-read loop at /rustc/…/intrinsics/mod.rs:419).
- The PF-909 gate is CLOSED: format-level export + independent round-trip.

### [0.5.x/0.6.x] — PF-901 (search-path slice): PDB recovery through symbol directories
- `pf-windows/src/modules.rs`: `resolve_pdb` resolves a module's RSDS PDB
  reference through three tiers — the reference's own path, the module's
  sibling directory, and `_NT_ALT_SYMBOL_PATH`/`_NT_SYMBOL_PATH`
  environment directories (semicolon-separated, bounded to 64 candidates;
  `srv*`/`cache*`/`symsrv*` store syntax is skipped honestly: PulseForge
  never performs network access). Every candidate is verified by the
  GUID+age match before being returned — a file whose name fits is never
  accepted on name alone.
- `ModuleInfo.checksum`: the optional-header image CheckSum, the Windows
  build-fingerprint analog (the RSDS GUID is the build identity itself),
  parsed from both the on-disk and the loaded image; dev builds may
  legitimately carry 0 (only /RELEASE computes it) — the contract is
  parse-and-stability.
- Verified on Windows with a real split workflow: a private copy of the
  test binary lives in a directory with no PDB while its PDB sits only in
  a symbol-path directory; resolution recovers it through the search path
  with GUID+age verification (E2E-PDB-SEARCH), and without the search
  path it honestly resolves to nothing. This completes the PF-901
  bounded slices (Toolhelp identity, loaded-image identity, PDB search
  paths).

### [0.5.x/0.6.x] — PF-901 (ReadProcessMemory slice): identity from the loaded image
- `pf-windows/src/modules.rs`: `RemoteProcess` opens a target for bounded
  remote reads (OpenProcess with VM_READ + QUERY_INFORMATION, RAII close,
  byte-exact reads) and `remote_module_identity` parses the LOADED PE
  header and debug directory straight from the target's address space.
- In-memory layout differs from the file: RVA = address − base, section
  extents use VirtualAddress/VirtualSize, and the debug directory locates
  its CodeView record via `AddressOfRawData` (the file parse uses
  `PointerToRawData` — the two locators are handled explicitly, each in
  its own parser).
- Verified on Windows against a running fixture: the remote identity
  (timestamp + GUID + age) agrees with the on-disk parse for a normally
  loaded image, and — the slice's purpose — still resolves after the
  fixture's on-disk executable is RENAMED AWAY while running
  (E2E-REMOTE-RENAME ok). A fourth real bug was caught by the first
  remote read: the data-directory index was off (DataDirectory[0] instead
  of [6]), invisible until a genuine remote parse ran.
- Remainder: `_NT_ALT_SYMBOL_PATH`-style PDB search paths and the Windows
  build-id note equivalent.

### [0.5.x/0.6.x] — PF-901 (bounded slice): same-user PE module identity
- `pf-windows/src/modules.rs`: `modules_of(pid)` enumerates a process's
  modules via the Toolhelp snapshot (same-user without elevation;
  other-user targets surface as typed PermissionDenied — never a silent
  empty list), and each module's on-disk PE image is parsed for its
  identity: TimeDateStamp plus the RSDS CodeView reference from the debug
  directory (PDB GUID + age + path, with the RVA-vs-file-pointer fallback
  mapped through the section table).
- `pdb_matches` reads the PDB's own MSF superblock and info stream
  (stream 1) and requires BOTH GUID and age to equal the PE's reference —
  a wrong-version PDB never matches, and malformed PDBs answer "no
  match" rather than guessing.
- Windows verification: the test binary's own module carries a real RSDS
  reference whose PDB matches (wrong-age negative included); the snapshot
  base equals `GetModuleHandleW(NULL)` ground truth; the private fixture
  loads in the ASLR region (proving the snapshot reports the true
  relocated base — non-relocated x64 images load at 0x140000000);
  LoadLibrary/FreeLibrary is observable in consecutive snapshots.
- Three parser bugs were found and fixed against python ground truth
  during development: the optional header starts at PE+24 (the signature
  is separate from the COFF header), the debug entry's `PointerToRawData`
  is at +24 (+20 is `AddressOfRawData`, an RVA), and the MSF
  `DirectorySize` lives at superblock +0x2C (+0x30 is `Unknown0`).
- Remainder: `_NT_ALT_SYMBOL_PATH`-style PDB search paths, the Windows
  build-id note equivalent, and remote image parsing via
  ReadProcessMemory.

### [0.5.x/0.6.x] — PF-909 (pprof slice, format-level): profile.proto export
- `pf-analysis/src/export.rs`: a hand-rolled bounded protobuf encoder
  (varint/tag/length-delimited; 200 000-location cap with a typed
  exhaustion error) emitting the pprof `profile.proto` document —
  string/function/mapping/location/sample tables built from the
  self-contained session stacks, leaf-first sample location ids, lines
  attached with the PF-1005 source attribution.
- Honesty contract mirrors the speedscope slice: `sample_type` and
  `period_type` are `samples`/`count` with period 1 — sample counts are
  never relabeled as elapsed time; unresolved frames keep their
  `module+0xoffset` identity; the raw `address` is unclaimed (0) and
  attribution rides on mapping/function/line.
- Spec conformance is executable: `validate_pprof_document` decodes the
  wire format field-by-field (tags, wire types, referential integrity
  across all four tables, the `string_table[0] == ""` contract, sample
  value arity) and the new `pulseforge export-pprof --session <dir>
  --out <file>` command self-validates every document before writing it
  (binary to file only — stdout stays text/JSON).
- Verified in WSL2: a real daemon-recorded session exports, decodes back
  through the conformance decoder, and reports the known hotspot as the
  top leaf (5350 samples). **Independent-viewer round-trip (go tool
  pprof) remains honestly NOT VERIFIED** until a host provides the tool.

### [0.5.x/0.6.x] — PF-1005 (comp-dir slice): absolute source paths
- `dwarf::comp_dirs`: a root-DIE-only walk over `.debug_info` collecting
  each compile unit's `(DW_AT_stmt_list, DW_AT_comp_dir)` pair — cheap
  and bounded (one abbrev + one attribute list per unit).
- `line.rs` binds every `.debug_line` unit to its comp dir through that
  map and resolves relative file paths absolutely: absolute names stay
  untouched, `.` and empty directories collapse, and relative names join
  their include directory then the comp dir (v4 implicit comp-dir base
  and v5 explicit directory entries both covered).
- Verified in WSL2: the rustc -O -g fixture's hot-loop IP now attributes
  to the ABSOLUTE source path (`…/inline_fixture.rs:13` — the actual
  compile location) both when parsing the binary directly and through
  the build-id split-debug recovery flow. This completes the PF-1005
  bounded slices: inline chains, line attribution, build-id and
  debuglink recovery, and comp-dir resolution.

### [0.5.x/0.6.x] — PF-1005 (build-id slice): build-id debug-file recovery
- `pf-linux/src/elfio.rs`: `.note.gnu.build-id` parsing (GNU owner,
  NT_GNU_BUILD_ID type, padded-note layout) plus the standard gdb layout
  helpers — first two hex digits as the directory, the rest as the
  `.debug` file name. Strip keeps the allocated note, so fully stripped
  binaries remain matchable.
- `symbols.rs` recovery order is now: direct parse → build-id (sibling
  `.build-id/…` layout, then `/usr/lib/debug/.build-id/…`; a candidate
  carrying its own build-id must equal the binary's) → `.gnu_debuglink`
  (CRC-verified) → whatever the binary itself offered, honestly.
- Verified in WSL2: the objcopy split + strip workflow recovers symbols,
  inline chains AND line info through the build-id layout, with
  `symbols_file` provenance pointing into `.build-id/`; a binary with
  the note removed (`objcopy -R`) parses as None; binaries without any
  recovery path still degrade to honest single-entry chains.
- PF-1005 remainder: absolute comp-dir resolution for split paths.

### [0.5.x/0.6.x] — PF-1005 (line + split-debug slice): file:line attribution and debuglink recovery
- `pf-linux/src/line.rs`: a minimal `.debug_line` state machine (v2–v5
  headers; special/standard/extended opcodes; VLIW units skipped
  honestly) producing a bounded address→file:line matrix — consecutive
  same-line runs collapsed, 1M-row cap with an honest `truncated` flag,
  last-row-at-or-below lookup semantics.
- `pf-linux/src/elfio.rs`: the shared zero-dep ELF layer (section-by-name
  extraction, the DWARF cursor, `.gnu_debuglink` parsing, CRC-32);
  `dwarf.rs` was refactored onto it.
- `ResolvedFrame` carries `file`/`line` (the sampled PC's source
  location — for inlined code, the inlinee's line); `stacks.jsonl`
  frames persist them and `stacks-summary.json` reports
  `line_resolved`.
- Split-debug recovery: a stripped binary follows `.gnu_debuglink`
  (sibling or `.debug/` companion) with the stored CRC32 verified against
  the candidate file — wrong or missing debug data degrades to
  module+offset honestly; the cache records `symbols_file` provenance.
- Verified in WSL2: the rustc -O -g fixture's hot-loop IP attributes to
  the fixture's real source line (inline_fixture.rs:13 — matching the
  embedded source); the standard `objcopy --only-keep-debug` + `strip` +
  `--add-gnu-debuglink` workflow recovers symbols, inline chains AND
  line info through the CRC-checked link; a stripped binary without a
  link still degrades to honest single-entry chains.
- Remainder: build-id-based debug-file search and absolute comp-dir
  resolution.

### [0.5.x/0.6.x] — PF-1003 (thread slice): whole-process sampling
- `pf-linux/src/perf.rs`: `ProcessSampler` — whole-process sampling with
  one `perf_event_open` per discovered thread (bounded at 32;
  `perf_event_open(pid=tgid)` measures only the group-leader thread, the
  limitation the thread-filter e2e surfaced last increment). All thread
  rings merge in `collect()`; LOST and kernel-frame-skip counters fold
  per-child deltas exactly once (also fixing a latent double-count in
  the drain loop's re-add of cumulative counters).
- Thread lifecycle: the stack-sampler drain thread re-scans
  `/proc/<pid>/task` at ~1 s cadence — threads born after recording start
  are opened and started, exited threads are drained and dropped, and
  threads that could not be opened (or exceed the bound) are counted in
  `threads_unsampled` rather than silently dropped.
- A third real bug fixed on the way: the sample parser read the PID half
  of `PERF_SAMPLE_TID` (the 64-bit field packs pid low, tid high on
  little-endian), misattributing every sample to the process instead of
  its thread — invisible while only the leader was sampled, and exactly
  what per-thread sampling exposed.
- Verified in WSL2: a two-thread fixture records samples from BOTH tids
  through the daemon (7,539/7,539 across the pair); a thread spawned
  600 ms after open is discovered by the rescan and its samples carry
  the new tid; the GUI thread filter now isolates a strict subset of a
  real multi-thread session and Reset restores the full view.
- Remainder: threads shorter than one rescan interval may be missed;
  CPU-migration/NUMA effects unexamined.

### [0.5.x/0.6.x] — PF-906: thread-filtered flame view
- The GUI stacks panel gained a bounded per-tid filter mirroring the
  time window: a thread dropdown driven by the samples' own tid values
  (top 64 threads by sample count, hidden threads counted honestly),
  model-level clamping so `reaggregate` combines window ∩ thread safely
  without the UI, honest "N of M samples" counts, and an All-threads
  reset.
- The real-data e2e surfaced two genuine findings, both handled:
  (1) the default full-range window truncated the tail below one
  millisecond and silently excluded the last samples of a recording —
  fixed so the full range includes the true end; (2) the perf sampler
  measures the target's group-leader thread only
  (`perf_event_open(pid=tgid)` semantics), so even a two-thread fixture
  yields a single tid — documented as the PF-1003 sampler remainder, with
  the e2e verifying the filter's honest behavior against the real
  single-tid session (keep-all, select-none, reset) while the synthetic
  suite covers the multi-tid path.

### [0.5.x/0.6.x] — PF-1005 (bounded slice): DWARF inline-frame expansion
- `pf-linux/src/dwarf.rs`: a minimal zero-dependency DWARF parser —
  `.debug_abbrev` + `.debug_info` only (v4 and v5, 32-bit format) — that
  extracts inlined-subroutine chains per code range: `DW_TAG_inlined_
  subroutine` ranges (low_pc/high_pc in both address and offset forms),
  names via `DW_AT_abstract_origin` (ref forms made absolute; strp/
  string/line_strp/strx with `DW_AT_str_offsets_base`), nested chains
  reconstructed from the DIE parent stack. Every output is capped
  (ranges, chain depth, DIEs, CUs); unknown forms or 64-bit DWARF make
  the unit degrade to symtab-only names — never to a wrong name.
- `pf-linux/src/symbols.rs`: the per-module cache now carries the inline
  table, and `resolve_ip_chains` resolves a sampled IP to its full chain
  `[inlinees innermost-first …, enclosing symbol]`; `resolve_ips` keeps
  its enclosing-symbol semantics. Stripped binaries degrade to honest
  `module+offset` single-entry chains.
- The recorder pipeline persists FLATTENED chains: `stacks.jsonl` frames
  entries gain `inlined: true` markers and `stacks-summary.json` an
  `inlined_frames` counter; leaf attribution now names the innermost
  inlinee. Aggregation, the flamegraph, the MCP stacks tools and the
  speedscope export all consume inline-expanded frames without changes.
- Verified in WSL2 against a REAL toolchain-built binary: a fixture
  compiled `rustc -O -g` with `#[inline(always)] inline_inner` inside
  `#[inline(never)] outer` resolves chains to `[inline_inner → outer]`
  deterministically (statically over the symbol range — no sampling
  noise); a stripped copy yields an empty inline table. The WSL2 daemon
  e2e stays green with the flattened frame schema.
- Three parser bugs were found and fixed against readelf/python ground
  truth during development: `sh_name` lives at Elf64_Shdr offset 0 (the
  first cut read sh_link), the form-code table was off by one for the
  data/string/run (data1/2/4/8 = 0x04..0x07, string = 0x08; ref_addr =
  0x10 and 4 bytes in 32-bit DWARF), and `DW_AT_abstract_origin` is 0x31
  (0x49 is DW_AT_type).
- Open PF-1005 remainder: `.debug_line` source locations and split-debug
  (debuglink) recovery.

### [0.5.x/0.6.x] — PF-906: time-filtered flamegraph
- The GUI stacks panel gained a bounded time-window filter driven by the
  samples' own `time_ns` values: two range-clamped drag fields in ms
  relative to the recording's first sample, with the window ordered and
  clamped inside the model (`reaggregate` is safe without the UI — the
  e2e caught an ordering bug there before it could ship).
- Honest counters throughout: "N of M samples in window", an exclusion
  note while filtering is active, and a Reset that restores the full
  view. Re-aggregation reuses the same node/depth/paint caps; absent
  timestamps (legacy lines) are skipped, never invented.
- `pf_analysis::stacks_io::load_session_stacks_timed` is the shared
  timed loader; the untimed wrapper keeps PF-905 ops unchanged.
- Verified in WSL2 against a real daemon-recorded session: the first
  half of the recording keeps the known hotspot in the tree with
  0 < windowed samples < total, and Reset restores the full count.
  Synthetic tests cover window math (relative-to-first-sample),
  zero-width windows, out-of-order ends, and reset.

### [0.5.x/0.6.x] — PF-909 (speedscope slice): stacks export for independent viewers
- `pf-analysis/src/export.rs` + new CLI command
  `pulseforge export-stacks --session <dir> [--out file]`: emits the
  session's sampled stacks as the speedscope file format (sampled
  profile). Honesty choices: `unit: "none"` with weight 1 per sample —
  sample counts are never relabeled as elapsed time; stacks are
  bottom-first per the format; unresolved frames keep their
  `module+0xoff`/`<unresolved>` identity; the input cap reports
  truncation (`EXPORT-TRUNCATED`) instead of hiding it.
- Spec conformance is encoded as a shared validator used by unit tests
  (synthetic session: 5 samples, 4 unique frames, endValue == summed
  weights, unresolved identity preserved) and by the WSL2 e2e (real
  daemon-recorded session; the top LEAF frame is the known hotspot,
  6657 samples — root frames legitimately dominate inclusive sums).
- Scope note: format-level validation only; an independent-viewer
  round-trip (speedscope.app) needs a host with the viewer installed.
  pprof export is a separate open slice (needs `go tool pprof` for the
  round-trip gate).

### [0.5.x/0.6.x] — PF-906 (bounded slice): flamegraph in the GUI
- `pf-gui/src/flame.rs`: a CPU-stacks panel on the session dashboard —
  a bounded icicle/flamegraph over the PF-904 engine's new `flame_tree`
  accessor (owned-value tree with the same node/depth caps and counted
  omissions as the JSON render).
- Interactions in this slice: hover tooltips (inclusive/exclusive/percent),
  click-to-zoom with Reset, capped search highlight (non-matches dimmed),
  per-frame paint budget with honest "frames hidden" notes. No time
  filtering or thread view yet — absent, not faked. Absent stacks render
  "CPU stacks unavailable (counters-only recording)", never an empty
  chart.
- `pf_analysis::stacks_io` (moved from pf-daemon) is now the one loader
  shared by daemon query ops and the GUI.
- e2e verified in WSL2: a real daemon-recorded `bench-spin` session
  drives the panel; the known hotspot appears in the rendered tree;
  synthetic sessions prove recursion collapse, coverage math, the search
  cap, zoom/reset, and counters-only honesty.

### [0.5.x/0.6.x] — PF-905: stacks through MCP (and the bridge on Linux)
- New daemon query ops `query_top_functions` and `get_stack`: bounded
  top-function lists (≤ 64 rows) and sample callchains containing a
  function (≤ 16 stacks, ≤ 64 frames, outermost-first), both carrying
  symbol-coverage quality fields (`frame_identity_coverage`,
  `frames_resolved`/`frames_total` — null when unknown, never zero) and
  typed errors for counters-only or unknown sessions.
- New MCP tools `profile_top_functions` and `profile_get_stack`
  (JSON-Schema contracts in tools/list) exposed to agent hosts.
- `pulseforge mcp` now connects to the Linux daemon over UDS —
  previously it answered UNSUPPORTED_PLATFORM on Linux; the MCP surface
  is verified end-to-end in WSL2 (`mcp_stacks_e2e`): a real daemon
  records bench-spin, and the top function reported through MCP is the
  known hotspot (6057 samples).

### [0.5.x/0.6.x] — PF-904 completed over real sampled stacks
- `pf-analysis/src/stacks.rs`: the aggregation engine now aggregates real
  sampled callchains — cycle-cut recursion safety (cuts counted; the cut
  node carries the exclusive weight), symbol-keyed interned frames,
  unresolved-frame buckets per module, a node cap (`nodes_truncated`
  surfaced), and a bounded call-tree JSON render whose per-node
  `omitted_children` makes every truncation visible. Output schema:
  `pulseforge.stacks-analysis/0`.
- Sessions are now self-contained for stack analysis: finalize resolves
  every frame of every sample against the start-of-session maps snapshot
  and persists the result in `stacks.jsonl` (`frames` beside raw `ips`,
  `symbol: null` when unresolved), writes a `maps.json` module snapshot
  for future re-resolution, and reports `frames_total`/`frames_resolved`
  in `stacks-summary.json`. Regression test: persisted lines re-parse.
- New CLI surface: `pulseforge stacks --session <dir> [--limit N]`
  aggregates a session's sampled stacks into top inclusive/exclusive
  functions, unresolved buckets and a bounded call tree.
- e2e verified in WSL2: a daemon-recorded 3 s `bench-spin` session
  aggregates to 6590 samples with the known hotspot
  (`pf_test_support::…RangeIteratorImpl::spec_next`) as the top exclusive
  function (6590) — never libc or `lang_start`; the persisted lines,
  summary counters and CLI render all agree.

### [0.5.x/0.6.x] — PF-1001/1002 completion: perf stacks in the daemon pipeline
- `pf-daemon/src/stacks.rs`: StackRecorder wraps `pf-linux`'s PerfSampler in
  a bounded drain thread (20 000-sample cap; over-cap samples are dropped
  and the truncation is surfaced — never unbounded memory). LOST records
  and kernel-frame skips are accumulated and reported.
- Session finalize (Linux) now persists sampled stacks into the durable
  session dir: `stacks.jsonl` (raw per-sample callchains, user-space only)
  and `stacks-summary.json` (`pulseforge.stacks/0`: sample/lost/truncated
  counters, kernel-frame accounting, leaf-symbol attribution against the
  maps snapshot taken at recording start). When the sampler produced no
  evidence, no stack files are written — unavailable is never rendered as
  zero.
- The stacks summary is embedded in the session's `report.json` and
  rendered by `pulseforge report --session …` as a `stacks` section;
  failures persist as warnings, not as fake absence.
- e2e verified in WSL2 (`pf-cli/tests/daemon_stacks_e2e.rs`): a real daemon
  records 3 s of `bench-spin`; 6080 leaf samples persist; the top leaf
  symbol resolves into `pf_test_support` (the known hotspot) — never
  `__libc_start_main` or `lang_start`; the daemon-written report, the
  standalone summary file, and the CLI render agree.

### [0.5.x/0.6.x] — PF-404 Linux UDS daemon transport
- `pf-linux::uds`: Unix-domain-socket transport (0600 socket in
  `$XDG_RUNTIME_DIR`, per-user + data-root name, stale-socket removal,
  concurrency-capped accept) behind the same `Stream` trait as the Windows
  pipe; daemon + agent wired on Linux; **e2e verified in WSL2 Ubuntu**
  (daemon serving → agent start → RECORDING → COMPLETED over UDS).

### [0.5.x/0.6.x] — P10 symbolization bounded slice
- `pf-linux/src/symbols.rs`: /proc/<pid>/maps parsing (file-backed
  ranges), ELF64 symtab/dynsym reader (single-pass section scan,
  sh_link strtab resolution, STT_FUNC extraction, bounded readers), IP →
  module → symbol+offset resolution with per-module LOAD-BASE correction
  (PIE: st_value is load-base-relative — the per-segment subtraction was
  the root-cause bug, found by the known-hotspot test).
- Verified in WSL2: sampled leaf IPs from the fixture child resolve to
  the fixture's known hot path (spec_next/forward_unchecked/next/
  spin_ms — 2231 fixture leaf frames); libc/startup frames never
  dominate (self-time attribution); stripped binaries resolve to
  module+offset honestly.

## [0.5.1] — P10 bounded slice: perf_event_open sampling
- `pf-linux/src/perf.rs`: perf_event_open FFI (kernel attr layout
  type@0/size@4), mmap ring buffer (data_head@1024/data_tail@1032/
  data@4096), PERF_RECORD_SAMPLE parsing, PERF_RECORD_LOST accounting,
  exclude_kernel + kernel-frame skip accounting, HW-probe + SW-fallback.
- Verified in WSL2 Ubuntu against a known-hotspot fixture: 675+ samples
  from 300 ms of hot work, user-space callchains only (683 kernel frames
  filtered with accounting), hot window out-samples idle window.
- Three bugs found and fixed via strace + ring diagnostics: swapped
  type/size fields, data_head at offset 0 (actually 1024), header size at
  pos+4 (actually pos+6).
- P10 remaining: symbolization (PF-90x Linux twin), recorder-pipeline
  integration, non-WSL bare-metal confirmation.

## [0.5.0] — Linux parity (P3 counters gate)

### Summary

The Linux counters gate (v0.2 in the original roadmap, delivered as the
0.5.0 milestone) is closed with native evidence from a real Ubuntu 24.04 /
kernel 5.15 environment (WSL2): `/proc` collector with the robust
`(comm)`-safe parser, pid+starttime+boot_id identity, logical-vs-storage
I/O semantics, record CLI end-to-end, and the capability probe reporting
`perf_event_paranoid` policy honestly. 7 unit tests + 4 e2e tests run
natively in the VM.

## [0.4.0] — 2026-10-27 (v0.4 core: native GUI + agent surfaces)

### Summary

Milestone state: v0.1 (counters), v0.3 (daemon + durable store), and the
P6/P7 cores (MCP bridge + native GUI) are closed with evidence on Windows.
Linux parity (P3/P10), ETW kernel-provider sampling (P8), and P12/P13
release gates remain open with documented unblock conditions in the private ledger (`docs/.local/TASKS.md`).

## [Unreleased]

### Added — PF-708 deep link (GUI)
- `pulseforge ui <session-dir>`: launches `pulseforge-gui --open <dir>`;
  the offline dashboard reads the durable chunk store directly (daemon not
  required). Windowed smoke-verified in both offline and daemon-connected
  modes.

### Added — dependency governance (PF-108)
- `deny.toml` + CI cargo-deny job: license policy (16 allow-listed
  permissive licenses), crate-source restrictions, duplicate-version
  warnings; `tools/license-scan.py` local mirror — verified all 415 locked
  packages in policy after ADR-015.

### Added — Phase P7 core (native GUI)
- `pf-gui` crate + `pulseforge-gui` binary: native egui/eframe (wgpu)
  application over the daemon — session browser (virtualized, searchable),
  attach form with typed validation, recording dashboard (CPU/resident/IO
  charts from the shared derivation engine, warnings verbatim, stop),
  live refresh cadence.
- Architecture: pure headless-testable view models behind a `DaemonClient`
  trait; the real named-pipe client and a scripted fake both drive them.
- Tests: full-UI headless passes (navigation, 100k-sample dashboard within
  budget, daemon-down surfaces typed errors) + a real-daemon e2e driving a
  live recording through the actual pipe client.
- Daemon fix: `session_dir` is now published at recording start (clients
  read durable chunks during a live recording, not only after finalize).
- ADR-015: eframe is the workspace's first deliberate product dependency.

### Added — hardening + scale (P12/P5/P6 subsets)
- Seeded-mutation fuzz suites for the JSON parser (~160k cases: byte soups,
  bit flips, truncations, splices, depth bombing) and chunk decoder
  (mutations, file soups, mixed clean/corrupt) — no panics, no invented
  data; **found and fixed** a real Int/UInt round-trip asymmetry (integers
  now decode canonically, serde_json convention).
- 1M-record store scale test (release): bounded queries stay exact and
  capped; 1.66 s end-to-end (PF-509 done).
- `docs/mcp-integration.md`: host configurations, tool table, agent loop,
  privacy notes (PF-609 done).
- `tools/overhead-study.sh` + measured results (PF-210 done for this
  machine class).

### Added — PF-008 GUI spike (verified, headless scope)
- `tools/gui-spike` (dev-only, outside the zero-dep workspace): egui
  headless pass/tessellation timing (100k-point chart + 10k-row virtualized
  table: p50 3.8 ms / p95 5.4 ms; 315k vertices peak) and offscreen wgpu
  device (example discrete GPU / Vulkan, verified readback). Converts PF-008 from
  NOT VERIFIED to measured evidence.

### Added — Phase P6 core (MCP agent interface)
- `pulseforge mcp`: stdio MCP bridge onto the daemon. Tools:
  capabilities, list_processes, list_sessions, attach, status, stop,
  summary, diagnostics — with JSON-Schema inputs, typed tool errors,
  64 KiB response budget, and **no launch tool** (ADR-008).
- End-to-end MCP tests against a real bridge+daemon: handshake, tool
  schemas, attach→status→stop workflow, idempotent stop, error paths,
  stdout purity with the daemon down.
- Named-pipe listener: live-instance accounting (sequential clients no
  longer exhaust the instance cap — real bug found by the MCP tests).
- ADR-014 (bridge on the validated JSON core; rmcp deferred).

### Added — Phases P5 + P11/P9 subsets
- `pf-storage`: versioned binary chunks (CRC-32 per chunk, atomic publish),
  crash-safe reader (`.partial` evidence, corruption surfaced as
  PARTIAL_DATA), bounded ranged queries with stride sampling,
  spike-preserving envelope rollups; daemon streams samples into chunks with
  a 1 s flush so a killed daemon loses at most ~1 s of data (kill-test
  proven).
- `pf-analysis`: observed-delta A/B comparison (scope-compatibility gates,
  `--force` opt-in, share-vs-time caveats), median/IQR/MAD distributions
  with outlier fence (no p-values), stack trie with inclusive/exclusive
  weights and coverage.
- `pulseforge compare --a --b [--force] [--markdown]`.
- `pf-windows` symbols: self-stack capture + serialized DbgHelp resolution
  with local PDBs only; tests resolve the full current stack.
- Zombie-handle fix: killed processes with lingering handles now read as
  TARGET_EXITED (single-instance lock race-free).

### Added — Phase P4 (per-user daemon + local IPC)
- `pf-daemon` crate: SessionSupervisor (FSM, idempotent stop, cancellation,
  live sample progress), single-instance lock with stale/PID-reuse
  recovery, orphaned-session recovery, framed-JSON IPC service.
- Windows named-pipe transport with an owner-only DACL (real user SID);
  endpoint scoped per user and data root; `pulseforge daemon` +
  `pulseforge agent …` CLI surface.
- End-to-end proof over real processes: recordings survive client
  disconnect; a second daemon refuses to start; a killed daemon's
  mid-recording session restarts as `RECOVERED_PARTIAL`.
- Recorder: live sample sink + externally-owned session ids.
- ADR-013 (framed JSON IPC, in-progress manifests, FIRST_INSTANCE finding).

### Added — repository
- MIT-licensed repository scaffold: CI (fmt / clippy / test / cross-check on
  Windows + Linux), issue templates, PR template, Dependabot, contributor
  guide, security policy, code of conduct, pinned toolchain (`1.97.1`).
- `docs/handoff/` — architecture brief and roadmap that form the implementation
  contract.

### Added — phase P0 (feasibility)
- `pf-test-support` fixture workloads: CPU-bound, CPU+I/O+memory, recursive
  stack, two-thread (PF-002).
- Windows counters feasibility probe validated against an independent
  measurement (PF-003) — see `docs/feasibility-results.md`.
- ETW session-creation probe with exact privilege outcomes, no foreign session
  touched (PF-004/PF-005) — see `docs/feasibility-results.md`.
- MCP stdio wire-format handshake spike (protocol-level, PF-009).
- Storage encoding microbenchmark fixture for the chunk-format decision
  (PF-010).
- Platform support matrix (`docs/support.md`, PF-001), security model
  (`docs/security-model.md`, PF-011), ADR log (`docs/decisions/`, PF-012).

### Added — v0.1 vertical slice (Windows counters-only)
- `pf-types`: typed process identity, metric taxonomy with units and
  availability, session state machine, error codes, bounded JSON writer.
- `pf-platform`: OS-neutral `CounterCollector` / `ProcessEnumerator` contracts.
- `pf-windows`: least-privilege Win32 counter collector (CPU, working set,
  private commit, I/O, handles, threads), stable process identity with PID
  reuse protection.
- `pf-cli`: `pulseforge capabilities`, `record --pid | --run`, `report`,
  `probe counters`, `probe etw`, `doctor`.
- Integration tests recording real fixture processes: happy path, access
  denied, target exit, identity mismatch, stop-without-kill.

### Known limitations (honest inventory)
- Linux counters (PF-006/007) are **NOT VERIFIED** — no Linux host was
  available; per the engineering contract they stay open, not skipped.
- CPU stack sampling is not implemented; ETW feasibility outcome is documented
  in `docs/feasibility-results.md` and gates milestone v0.6.
- Session storage is a provisional schema-0 directory (manifest + JSONL
  samples); the durable chunked format (PF-501…) lands in milestone v0.3.
- GUI (PF-008 render spike on real hardware) NOT VERIFIED in this environment.
