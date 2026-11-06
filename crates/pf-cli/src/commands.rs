//! Subcommand implementations. Platform backends are selected at compile
//! time; without one, commands fail honestly with `UNSUPPORTED_PLATFORM`.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::time::Duration;

use pf_platform::contracts::PlatformBackend;
use pf_platform::paths;
use pf_platform::record::{Recorder, RecorderConfig};
use pf_platform::session_store::{SCHEMA_0, SessionStore, manifest_v0};
use pf_types::json::JsonValue;
use pf_types::{ErrorCode, Os, PfError, SessionState};

use crate::args::{Command, RecordArgs};
use crate::report;

fn backend() -> Result<Box<dyn PlatformBackend>, PfError> {
    #[cfg(windows)]
    {
        Ok(Box::new(pf_windows::WindowsPlatform::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(pf_linux::LinuxPlatform::new()))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "no collector backend is compiled for this OS (supported: Windows, Linux)",
        ))
    }
}

fn caps_json(b: &dyn PlatformBackend) -> JsonValue {
    use JsonValue as J;
    let caps = b.capabilities();
    let probe = |p: &pf_platform::contracts::ProbeStatus| {
        J::Object(vec![
            ("available".into(), J::Bool(p.available)),
            ("reason_code".into(), J::Str(p.reason_code.clone())),
            ("detail".into(), J::Str(p.detail.clone())),
        ])
    };
    J::Object(vec![
        ("tool".into(), J::Str("pulseforge".into())),
        ("version".into(), J::Str(env!("CARGO_PKG_VERSION").into())),
        ("os".into(), J::Str(caps.os.as_str().into())),
        ("os_version".into(), J::Str(caps.os_version.clone())),
        (
            "logical_cpus".into(),
            match caps.logical_cpus {
                Some(n) => J::UInt(u64::from(n)),
                None => J::Null,
            },
        ),
        ("counters".into(), probe(&caps.counters)),
        ("cpu_stack_sampling".into(), probe(&caps.cpu_stack_sampling)),
    ])
}

/// PF-708 deep link: `pulseforge ui <session>` opens the GUI directly on
/// the recorded session (offline chunk-store read; daemon not required).
fn dispatch_ui(session: &str) -> Result<(), PfError> {
    let gui = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pulseforge-gui.exe")));
    let Some(gui) = gui else {
        return Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "cannot locate pulseforge-gui next to the CLI binary",
        ));
    };
    if !gui.is_file() {
        return Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            format!(
                "pulseforge-gui not found at {} (build it with: cargo build --release -p pf-gui)",
                gui.display()
            ),
        ));
    }
    match std::process::Command::new(&gui)
        .arg("--open")
        .arg(session)
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(e) => Err(PfError::new(
            ErrorCode::BackendFailure,
            format!("failed to launch the GUI: {e}"),
        )),
    }
}

pub fn dispatch(cmd: Command) -> Result<(), PfError> {
    match &cmd {
        Command::Daemon => return crate::daemon_cmds::dispatch_daemon(),
        Command::Agent(agent) => return crate::daemon_cmds::dispatch_agent(agent.clone()),
        Command::Mcp => return crate::mcp_bridge::run(),
        _ => {}
    }
    match cmd {
        Command::Version => {
            println!("pulseforge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Daemon | Command::Agent(_) | Command::Mcp => unreachable!("handled above"),
        Command::Ui { session } => dispatch_ui(session.as_str()),
        Command::Bench {
            baseline,
            candidate,
            out_dir,
        } => crate::bench::run(&baseline, &candidate, out_dir),
        Command::Export {
            session,
            out,
            redact,
        } => {
            let policy = if redact {
                pf_platform::export::RedactionPolicy::Standard
            } else {
                pf_platform::export::RedactionPolicy::None
            };
            let redactions = pf_platform::export::export_session(
                std::path::Path::new(&session),
                std::path::Path::new(&out),
                policy,
            )?;
            println!(
                "exported: {} ({} redactions, policy {})",
                out,
                redactions,
                if redact { "standard" } else { "none" }
            );
            Ok(())
        }
        Command::Capabilities => {
            // Capabilities must work even without a backend (honest report).
            match backend() {
                Ok(b) => print_json(&caps_json(b.as_ref())),
                Err(e) if e.code == ErrorCode::UnsupportedPlatform => {
                    use JsonValue as J;
                    print_json(&J::Object(vec![
                        ("tool".into(), J::Str("pulseforge".into())),
                        ("version".into(), J::Str(env!("CARGO_PKG_VERSION").into())),
                        ("os".into(), J::Str(host_os_name().into())),
                        (
                            "counters".into(),
                            J::Object(vec![
                                ("available".into(), J::Bool(false)),
                                ("reason_code".into(), J::Str("NO_BACKEND".into())),
                                ("detail".into(), J::Str(e.message.clone())),
                            ]),
                        ),
                        (
                            "cpu_stack_sampling".into(),
                            J::Object(vec![
                                ("available".into(), J::Bool(false)),
                                ("reason_code".into(), J::Str("NO_BACKEND".into())),
                                ("detail".into(), J::Str(e.message.clone())),
                            ]),
                        ),
                    ]))
                }
                Err(e) => Err(e),
            }
        }
        Command::List { limit } => {
            let b = backend()?;
            let entries = b.enumerate(limit)?;
            use JsonValue as J;
            let rows: Vec<JsonValue> = entries
                .iter()
                .map(|e| {
                    J::Object(vec![
                        ("pid".into(), J::UInt(u64::from(e.pid))),
                        (
                            "parent_pid".into(),
                            match e.parent_pid {
                                Some(p) => J::UInt(u64::from(p)),
                                None => J::Null,
                            },
                        ),
                        ("image".into(), J::Str(e.image_name.clone())),
                    ])
                })
                .collect();
            print_json(&J::Array(rows))
        }
        Command::ProbeCounters => {
            let b = backend()?;
            probe_counters(b.as_ref())
        }
        Command::ProbeEtw => {
            #[cfg(windows)]
            {
                let result = pf_windows::probe_etw_session_creation();
                print_json(&result.to_json())
            }
            #[cfg(not(windows))]
            {
                Err(PfError::new(
                    ErrorCode::UnsupportedPlatform,
                    "ETW probe is Windows-only (Linux equivalent: perf probe, roadmap v0.2)",
                ))
            }
        }
        Command::Doctor => doctor(),
        Command::BenchSpin { ms } => {
            let checksum = pf_test_support::spin_ms(ms);
            println!("bench-spin {ms}ms checksum={checksum}");
            Ok(())
        }
        Command::Report { session } => render_session_report(&session),
        Command::Stacks {
            session,
            limit,
            compare_session,
            bottom_up,
        } => render_stacks_analysis(&session, limit, compare_session.as_deref(), bottom_up),
        Command::ExportStacks { session, out } => export_speedscope(&session, out.as_deref()),
        Command::ExportPprof { session, out } => export_pprof(&session, &out),
        Command::Compare {
            baseline,
            candidate,
            force,
            markdown,
        } => run_compare(&baseline, &candidate, force, markdown),
        Command::Record(args) => run_record(args),
    }
}

fn host_os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn print_json(v: &JsonValue) -> Result<(), PfError> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", v.to_json_string()).map_err(PfError::from)?;
    Ok(())
}

/// PF-003 probe: record *this* process for 2 s at the default interval and
/// emit the report plus a self-consistency check (cumulative counters must
/// never decrease; gauges must be present).
fn probe_counters(b: &dyn PlatformBackend) -> Result<(), PfError> {
    let caps = b.capabilities();
    let pid = std::process::id();
    let mut source = b.open(pid)?;
    let cfg = RecorderConfig {
        interval: Duration::from_millis(250),
        max_duration: Some(Duration::from_millis(2_000)),
        ..RecorderConfig::default()
    };
    let result = Recorder::new(source.as_mut(), cfg, None)?.run()?;
    let json = report::from_recorder(&result, caps.logical_cpus, None);
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", json.to_json_string()).map_err(PfError::from)
}

fn doctor() -> Result<(), PfError> {
    use JsonValue as J;
    let data_root = paths::data_root();
    let sessions = paths::sessions_root();
    let caps = backend().ok().map(|b| caps_json(b.as_ref()));
    let json = J::Object(vec![
        ("tool".into(), J::Str("pulseforge".into())),
        ("version".into(), J::Str(env!("CARGO_PKG_VERSION").into())),
        ("pid".into(), J::UInt(u64::from(std::process::id()))),
        (
            "data_root".into(),
            match &data_root {
                Ok(p) => J::Str(p.display().to_string()),
                Err(e) => J::Str(format!("ERROR: {e}")),
            },
        ),
        (
            "sessions_root".into(),
            match &sessions {
                Ok(p) => J::Str(p.display().to_string()),
                Err(e) => J::Str(format!("ERROR: {e}")),
            },
        ),
        (
            "data_root_writable".into(),
            J::Bool(
                data_root
                    .as_ref()
                    .map(|p| p.join(".write-test").tap_write())
                    .unwrap_or(false),
            ),
        ),
        ("backend".into(), caps.unwrap_or(J::Null)),
    ]);
    print_json(&json)
}

trait WriteTest {
    fn tap_write(&self) -> bool;
}
impl WriteTest for PathBuf {
    fn tap_write(&self) -> bool {
        std::fs::write(self, b"ok").is_ok() && std::fs::remove_file(self).is_ok()
    }
}

fn resolve_out_dir(args: &RecordArgs) -> Result<PathBuf, PfError> {
    match &args.out_dir {
        Some(d) => {
            let p = PathBuf::from(d);
            std::fs::create_dir_all(&p).map_err(PfError::from)?;
            Ok(p)
        }
        None => paths::sessions_root().map_err(|e| {
            PfError::new(
                ErrorCode::StorageIo,
                format!(
                    "cannot resolve sessions directory: {e} (set PULSEFORGE_DATA or pass --out)"
                ),
            )
        }),
    }
}

fn run_record(args: RecordArgs) -> Result<(), PfError> {
    let b = backend()?;
    let caps = b.capabilities();

    // Target acquisition: attach (never touched) or direct launch (no shell).
    let mut child: Option<std::process::Child> = None;
    let pid = match args.pid {
        Some(pid) => pid,
        None => {
            let exe = args.run.clone().ok_or_else(|| {
                PfError::new(ErrorCode::Internal, "--run requires an executable path")
            })?;
            let mut cmd = StdCommand::new(&exe);
            cmd.args(&args.run_args);
            // The child's stdio stays separate from pulseforge's own pipes.
            cmd.stdin(std::process::Stdio::null());
            let c = cmd.spawn().map_err(|e| {
                PfError::new(
                    ErrorCode::InvalidArgument,
                    format!("failed to launch `{exe}`: {e}"),
                )
                .with_remediation(
                    "pass an executable path directly; PulseForge never invokes a shell",
                )
            })?;
            let pid = c.id();
            child = Some(c);
            // Give the loader a beat so the process exists before we open it.
            std::thread::sleep(Duration::from_millis(150));
            pid
        }
    };

    let record_result = (|| -> Result<pf_platform::record::RecorderReport, PfError> {
        let mut source = b.open(pid)?;
        let cfg = RecorderConfig {
            interval: Duration::from_millis(args.interval_ms),
            max_duration: Some(Duration::from_millis(args.duration_ms)),
            ..RecorderConfig::default()
        };
        Recorder::new(source.as_mut(), cfg, None)?.run()
    })();

    // Child lifecycle AFTER the recording is finalized (stop-never-kills
    // applies to the recording itself; direct-parent cleanup is hygiene).
    let had_child = child.is_some();
    if let Some(mut c) = child {
        if !args.leave_running {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    let result = record_result?;
    let session_id = result.outcome.session_id;
    let out_root = resolve_out_dir(&args)?;
    let store = SessionStore::create(&out_root, session_id)?;
    store.append_samples(&result.snapshots)?;
    let identity = result.target_identity.clone();
    let mut warnings = result.outcome.warnings.clone();
    if args.leave_running {
        warnings.push(format!(
            "target pid {pid} was launched by this session and left running by request"
        ));
    } else if had_child {
        warnings.push(format!(
            "recording stopped at budget; child pid {pid} (launched by this session) was terminated as direct-parent cleanup"
        ));
    }
    let manifest = manifest_v0(
        session_id,
        result.outcome.terminal_state(),
        caps.os.as_str(),
        &caps.os_version,
        &identity,
        &result.image_name,
        caps.logical_cpus,
        args.interval_ms,
        result.outcome.reason.as_str(),
        &warnings,
        SCHEMA_0,
    );
    store.write_manifest(&manifest)?;
    let json = report::from_recorder(&result, caps.logical_cpus, None);
    let report_path = store.dir().join("report.json");
    std::fs::write(&report_path, format!("{}\n", json.to_json_string())).map_err(PfError::from)?;

    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "session: {}", store.dir().display());
    let _ = writeln!(out, "report:  {}", report_path.display());
    let _ = writeln!(out, "summary: {}", report::one_line_summary(&json));
    Ok(())
}

fn render_session_report(session_dir: &str) -> Result<(), PfError> {
    let store = SessionStore::open(std::path::Path::new(session_dir))?;
    let manifest = store.read_manifest()?;
    let session_id = manifest
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, "manifest lacks session_id"))?;
    let session_id = pf_types::SessionId::parse(session_id)
        .map_err(|e| PfError::new(ErrorCode::InvalidArgument, format!("bad session_id: {e}")))?;
    let state = manifest
        .get("state")
        .and_then(|v| v.as_str())
        .and_then(parse_state)
        .unwrap_or(SessionState::Failed);
    let stop_reason = manifest
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".into());
    let identity = read_identity(&manifest)?;
    let image = manifest
        .get("target")
        .and_then(|t| t.get("image"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let warnings: Vec<String> = manifest
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // Prefer the durable chunk store when present; fall back to schema-0
    // JSONL (older sessions) — both produce the same report shape.
    let (snapshots, partial) = match pf_storage::ChunkReader::open(store.dir()) {
        Ok(reader) if reader.total_records() > 0 => {
            let mut notes: Vec<String> = reader.salvage_notes().to_vec();
            if reader.corrupt_chunk_count() > 0 {
                notes.push(format!(
                    "{} corrupt chunk(s) excluded",
                    reader.corrupt_chunk_count()
                ));
            }
            let page = reader.range(0, u64::MAX, 1_000_000)?;
            if page.truncated {
                notes.push("query cap reached; report shows a sample".into());
            }
            (
                page.snapshots,
                (!notes.is_empty()).then(|| notes.join("; ")),
            )
        }
        _ => store.read_samples()?,
    };
    let logical = manifest
        .get("target")
        .and_then(|t| t.get("logical_cpus"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok());
    // Sampled-stack summary (PF-1001/1002): embedded verbatim when the
    // session dir carries one; absent means stacks were unavailable.
    let stacks_summary = std::fs::read_to_string(store.dir().join("stacks-summary.json"))
        .ok()
        .and_then(|text| pf_types::json::parse(&text).ok());
    let json = report::build_report(report::ReportInput {
        schema_label: "pulseforge.report/0",
        session_id: &session_id,
        state,
        stop_reason: stop_reason.as_str(),
        identity: &identity,
        image_name: &image,
        snapshots: &snapshots,
        logical_cpus: logical,
        warnings: &warnings,
        poll_p50_ms: 0.0,
        poll_max_ms: 0.0,
        partial_read: partial.as_deref(),
        stacks_summary: stacks_summary.as_ref(),
    });
    print_json(&json)
}

/// Stack aggregation over a recorded session's sampled stacks (PF-904):
/// reads the self-contained `stacks.jsonl` (resolved frames persisted at
/// finalize time), builds outermost-first sample stacks and renders the
/// shared aggregation JSON — top functions, call tree, unresolved buckets.
/// Bounded: bounded input file, bounded top lists, bounded tree.
fn render_stacks_analysis(
    session_dir: &str,
    limit: usize,
    compare_session: Option<&str>,
    bottom_up: bool,
) -> Result<(), PfError> {
    let store = SessionStore::open(std::path::Path::new(session_dir))?;
    let session_id = store
        .read_manifest()?
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(|s| pf_types::SessionId::parse(s).ok())
        .ok_or_else(|| {
            PfError::new(
                ErrorCode::InvalidArgument,
                "manifest lacks a valid session_id",
            )
        })?;
    let stacks_path = store.dir().join("stacks.jsonl");
    let text = std::fs::read_to_string(&stacks_path).map_err(|_| {
        PfError::new(
            ErrorCode::CapabilityUnavailable,
            "session has no sampled stacks (counters-only recording, or stacks              persistence failed); nothing to aggregate",
        )
    })?;
    let mut samples: Vec<pf_analysis::stacks::SampledStack> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v = pf_types::json::parse(line).map_err(|e| {
            PfError::new(
                ErrorCode::PartialData,
                format!("stacks.jsonl line unparseable: {e}"),
            )
        })?;
        let frames_json = match v.get("frames").and_then(|f| f.as_array()) {
            Some(f) => f,
            None => continue, // legacy line without resolved frames
        };
        let mut frames: Vec<pf_analysis::stacks::ResolvedInput> =
            Vec::with_capacity(frames_json.len());
        for fr in frames_json {
            let module = fr
                .get("module")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let symbol = fr
                .get("symbol")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let offset = fr.get("offset").and_then(|m| m.as_u64()).unwrap_or(0);
            let file = fr
                .get("file")
                .and_then(|m| m.as_str())
                .filter(|f| !f.is_empty())
                .map(str::to_string);
            let line = fr
                .get("line")
                .and_then(|m| m.as_u64())
                .and_then(|l| u32::try_from(l).ok())
                .filter(|l| *l > 0);
            frames.push((module, symbol, offset, file, line));
        }
        // Persisted frames are callee-first (innermost IP first); the trie
        // aggregates outermost-first.
        let mut st = pf_analysis::stacks::SampledStack::from_resolved(&frames, 1);
        st = st.flip();
        samples.push(st);
    }
    if samples.is_empty() {
        return Err(PfError::new(
            ErrorCode::PartialData,
            "stacks.jsonl carries no aggregatable samples",
        ));
    }
    let mut trie = pf_analysis::stacks::StackTrie::new();
    if bottom_up {
        // PF-907: callee-first aggregation — exclusive weight lands on the
        // outermost frame, and the tree walks callees-first.
        trie.aggregate_bottom_up(&samples);
    } else {
        trie.aggregate(&samples);
    }
    // PF-1105 function-shift compare: diff this session"s top functions
    // against another session"s (bounded to the same top-list limit).
    let compare_shifts: Option<Vec<pf_analysis::stacks::FunctionShift>> =
        compare_session.and_then(|other| {
            let other_store = SessionStore::open(std::path::Path::new(other)).ok()?;
            let other_samples =
                pf_analysis::stacks_io::load_session_stacks(other_store.dir()).ok()?;
            let mut other_trie = pf_analysis::stacks::StackTrie::new();
            other_trie.aggregate(&other_samples);
            Some(pf_analysis::stacks::compare_top_functions(
                &other_trie,
                &trie,
                limit,
            ))
        });
    let out = pf_types::json::JsonValue::Object(vec![
        (
            "schema".to_string(),
            pf_types::json::JsonValue::Str("pulseforge.stacks-analysis/0".into()),
        ),
        (
            "session".to_string(),
            pf_types::json::JsonValue::Str(session_id.as_hyphenated()),
        ),
        (
            "direction".to_string(),
            pf_types::json::JsonValue::Str(
                if bottom_up { "bottom-up" } else { "top-down" }.to_string(),
            ),
        ),
        (
            "compared_against".to_string(),
            match compare_session {
                Some(other) => pf_types::json::JsonValue::Str(other.to_string()),
                None => pf_types::json::JsonValue::Null,
            },
        ),
        (
            "function_shifts".to_string(),
            match &compare_shifts {
                Some(shifts) => pf_analysis::stacks::shifts_to_json(shifts),
                None => pf_types::json::JsonValue::Null,
            },
        ),
        (
            "aggregation".to_string(),
            trie.to_json_with_tree(limit, 2_000, 48),
        ),
    ]);
    print_json(&out)
}

/// PF-909: write the session's sampled stacks as a speedscope JSON
/// document (bounded; unit "none", weight 1 per sample — sample counts are
/// never relabeled as elapsed time). `--out` writes a file; without it the
/// document goes to stdout (protocol purity: JSON only).
fn export_speedscope(session_dir: &str, out: Option<&str>) -> Result<(), PfError> {
    use std::io::Write;
    let store = SessionStore::open(std::path::Path::new(session_dir))?;
    let session_id = store
        .read_manifest()?
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(|s| pf_types::SessionId::parse(s).ok())
        .ok_or_else(|| {
            PfError::new(
                ErrorCode::InvalidArgument,
                "manifest lacks a valid session_id",
            )
        })?;
    let image = store
        .read_manifest()
        .ok()
        .and_then(|m| {
            m.get("target")
                .and_then(|t| t.get("image"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let profile_name = if image.is_empty() {
        format!("pulseforge session {}", session_id.as_hyphenated())
    } else {
        format!(
            "{} (pulseforge session {})",
            image,
            session_id.as_hyphenated()
        )
    };
    let report = pf_analysis::export::speedscope_export(
        store.dir(),
        &profile_name,
        &format!("pulseforge@{}", env!("CARGO_PKG_VERSION")),
    )?;
    if report.truncated {
        eprintln!(
            "EXPORT-TRUNCATED: {}/{} samples exported (cap)",
            report.samples_included, report.samples_total
        );
    }
    let body = format!(
        "{}
",
        report.doc.to_json_string()
    );
    match out {
        Some(path) => {
            let mut f = std::fs::File::create(path)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            eprintln!(
                "EXPORTED: {} samples, {} frames -> {}",
                report.samples_included, report.frames, path
            );
            Ok(())
        }
        None => {
            std::io::stdout().write_all(body.as_bytes())?;
            Ok(())
        }
    }
}

/// PF-909: write the session's sampled stacks as a pprof (profile.proto)
/// protobuf document. Binary output goes to a file (stdout stays
/// text/JSON); the emitted structure is then re-validated with the
/// spec-conformance decoder before the command reports success — we never
/// hand out a document our own decoder cannot read back.
fn export_pprof(session_dir: &str, out: &str) -> Result<(), PfError> {
    use std::io::Write;
    let store = SessionStore::open(std::path::Path::new(session_dir))?;
    let report = pf_analysis::export::pprof_export(store.dir())?;
    if report.truncated {
        eprintln!(
            "EXPORT-TRUNCATED: {}/{} samples exported (cap)",
            report.samples_included, report.samples_total
        );
    }
    // Self-validation before write: the document must survive our own
    // spec-conformance decoder.
    let validation = pf_analysis::export::validate_pprof_document(&report.doc)?;
    let mut f = std::fs::File::create(out)?;
    f.write_all(&report.doc)?;
    f.sync_all()?;
    eprintln!(
        "EXPORTED: {} samples, {} locations, {} functions -> {} (validated: {} samples, top leaf {:?})",
        report.samples_included,
        report.locations,
        report.functions,
        out,
        validation.samples,
        validation
            .by_leaf
            .iter()
            .max_by_key(|(_, w)| **w)
            .map(|(k, _)| k.clone())
    );
    Ok(())
}

fn parse_state(s: &str) -> Option<SessionState> {
    use SessionState::*;
    Some(match s {
        "CREATED" => Created,
        "STARTING" => Starting,
        "RECORDING" => Recording,
        "STOPPING" => Stopping,
        "FINALIZING" => Finalizing,
        "COMPLETED" => Completed,
        "FAILED" => Failed,
        "CANCELLED" => Cancelled,
        "RECOVERED_PARTIAL" => RecoveredPartial,
        _ => return None,
    })
}

fn read_identity(manifest: &JsonValue) -> Result<pf_types::ProcessIdentity, PfError> {
    let t = manifest
        .get("target")
        .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, "manifest lacks target"))?;
    let os = match t.get("os").and_then(|v| v.as_str()) {
        Some("windows") => Os::Windows,
        Some("linux") => Os::Linux,
        _ => {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "manifest target lacks os",
            ));
        }
    };
    Ok(pf_types::ProcessIdentity::new(
        os,
        t.get("pid")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        t.get("creation_identity")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        t.get("boot_identity")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    ))
}

/// Session facts load result for comparisons.
struct LoadedSession {
    snapshots: Vec<pf_types::CounterSnapshot>,
    os: pf_types::Os,
    interval_ms: u64,
    logical_cpus: Option<u32>,
    label: String,
}

/// `pulseforge compare --a DIR --b DIR`: observed-delta comparison of two
/// recorded sessions (PF-1107). Reads chunks when present, falls back to
/// schema-0 JSONL; missing counters compare as *unavailable*, never zero.
fn run_compare(a_dir: &str, b_dir: &str, force: bool, markdown: bool) -> Result<(), PfError> {
    let load = |dir: &str| -> Result<LoadedSession, PfError> {
        let store = pf_platform::session_store::SessionStore::open(std::path::Path::new(dir))?;
        let manifest = store.read_manifest()?;
        let target = manifest.get("target").cloned().unwrap_or(JsonValue::Null);
        let os = match target.get("os").and_then(|v| v.as_str()) {
            Some("linux") => pf_types::Os::Linux,
            _ => pf_types::Os::Windows,
        };
        let interval = target
            .get("interval_ms")
            .and_then(|v| v.as_u64())
            .or_else(|| manifest.get("interval_ms").and_then(|v| v.as_u64()))
            .unwrap_or(250);
        let cpus = target
            .get("logical_cpus")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        let label = manifest
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or(dir)
            .to_string();
        let snapshots = match pf_storage::ChunkReader::open(std::path::Path::new(dir)) {
            Ok(r) if r.total_records() > 0 => r.range(0, u64::MAX, 1_000_000)?.snapshots,
            _ => store.read_samples()?.0,
        };
        Ok(LoadedSession {
            snapshots,
            os,
            interval_ms: interval,
            logical_cpus: cpus,
            label,
        })
    };
    let a_loaded = load(a_dir)?;
    let b_loaded = load(b_dir)?;
    let a = pf_analysis::SessionFacts::from_snapshots(
        &a_loaded.label,
        a_loaded.os,
        a_loaded.interval_ms,
        a_loaded.logical_cpus,
        &a_loaded.snapshots,
        vec![],
    );
    let b = pf_analysis::SessionFacts::from_snapshots(
        &b_loaded.label,
        b_loaded.os,
        b_loaded.interval_ms,
        b_loaded.logical_cpus,
        &b_loaded.snapshots,
        vec![],
    );
    let cmp = pf_analysis::compare_sessions(&pf_analysis::ComparisonInput {
        baseline: a,
        candidate: b,
        force_incompatible: force,
    });
    if markdown {
        println!("{}", cmp.to_markdown(&a_loaded.label, &b_loaded.label));
    } else {
        print_json(&cmp.to_json())?;
    }
    Ok(())
}
