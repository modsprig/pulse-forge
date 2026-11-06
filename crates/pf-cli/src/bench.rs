//! `pulseforge bench` (roadmap PF-1101/1102/1104): execute a WorkloadSpec
//! twice (baseline/candidate specs) with recorded repetitions, then emit a
//! repeated-run comparison. Direct argv launch only; the child is managed
//! exactly like `record --run` (direct parent, never a shell).

use std::process::ExitCode;

use pf_platform::contracts::PlatformBackend;
use pf_platform::record::{Recorder, RecorderConfig};
use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId};

use pf_analysis::{RunResult, WorkloadSpec, compare_repeated};

pub fn run(
    baseline_path: &str,
    candidate_path: &str,
    out_dir: Option<String>,
) -> Result<(), PfError> {
    let read_spec = |p: &str| -> Result<WorkloadSpec, PfError> {
        let text = std::fs::read_to_string(p).map_err(|e| {
            PfError::new(
                ErrorCode::InvalidArgument,
                format!("cannot read spec {p}: {e}"),
            )
        })?;
        WorkloadSpec::parse(&text)
            .map_err(|e| PfError::new(ErrorCode::InvalidArgument, format!("invalid spec {p}: {e}")))
    };
    let baseline = read_spec(baseline_path)?;
    let candidate = read_spec(candidate_path)?;

    let backend = backend_factory()?;
    let caps = backend.capabilities();
    let logical_cpus = caps.logical_cpus;

    let store_root = match out_dir {
        Some(d) => {
            std::fs::create_dir_all(&d).map_err(PfError::from)?;
            std::path::PathBuf::from(d)
        }
        None => pf_platform::paths::sessions_root()
            .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("sessions root: {e}")))?,
    };

    eprintln!(
        "bench: baseline spec `{}` ({} reps)",
        baseline.name, baseline.repetitions
    );
    let baseline_runs = run_spec(&baseline, store_root.as_path(), logical_cpus)?;
    eprintln!(
        "bench: candidate spec `{}` ({} reps)",
        candidate.name, candidate.repetitions
    );
    let candidate_runs = run_spec(&candidate, store_root.as_path(), logical_cpus)?;

    let report = compare_repeated(&baseline_runs, &candidate_runs)
        .map_err(|e| PfError::new(ErrorCode::InvalidArgument, e))?;
    // Attach provenance for both sides (PF-1102).
    let report = attach_provenance(
        report,
        &baseline,
        &candidate,
        caps.os.as_str(),
        logical_cpus,
    );
    let mut out = std::io::stdout().lock();
    use std::io::Write;
    writeln!(out, "{}", report.to_json_string()).map_err(PfError::from)?;
    Ok(())
}

fn backend_factory() -> Result<Box<dyn PlatformBackend>, PfError> {
    #[cfg(windows)]
    {
        Ok(Box::new(pf_windows::WindowsPlatform::new()))
    }
    #[cfg(not(windows))]
    {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "no collector backend compiled for this OS",
        ))
    }
}

/// Execute one spec: warm-up runs (unrecorded), then recorded repetitions.
// Loop counters bounded by validated spec limits (reps<=100).
#[allow(clippy::arithmetic_side_effects)]
fn run_spec(
    spec: &WorkloadSpec,
    store_root: &std::path::Path,
    _logical_cpus: Option<u32>,
) -> Result<Vec<RunResult>, PfError> {
    let mut results = Vec::with_capacity(spec.repetitions as usize);
    for i in 0..spec.warmup {
        eprintln!("bench: warm-up {}/{}, discarding", i + 1, spec.warmup);
        run_child(spec, None)?;
    }
    for i in 0..spec.repetitions {
        eprintln!("bench: run {}/{}", i + 1, spec.repetitions);
        let (mut child, id) = run_child(spec, Some(store_root))?;
        // Read back what the recorder persisted (chunk store preferred).
        let dir = store_root.join(format!("{}.pfsession", id.as_hyphenated()));
        let snapshots = wait_and_load(&mut child, &dir)?;
        results.push(RunResult::from_snapshots(i, &snapshots));
    }
    Ok(results)
}

/// Launch + record one execution. `store_root = None` discards (warm-up).
/// The child is our direct process: terminated after the recording as
/// direct-parent cleanup, exactly like `record --run`.
fn run_child(
    spec: &WorkloadSpec,
    store_root: Option<&std::path::Path>,
) -> Result<(std::process::Child, SessionId), PfError> {
    let mut cmd = std::process::Command::new(&spec.exe);
    cmd.args(&spec.args);
    // Environment allowlist: only named variables are forwarded (PF-1102).
    for key in &spec.env_allowlist {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    // The child's stdout/stderr stay out of the bench report stream.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        PfError::new(
            ErrorCode::InvalidArgument,
            format!("failed to launch `{}`: {e}", spec.exe),
        )
        .with_remediation("pass an executable path directly; the bench never invokes a shell")
    })?;
    let pid = child.id();

    let backend = backend_factory()?;
    let mut source = backend.open(pid)?;
    let cfg = RecorderConfig {
        interval: std::time::Duration::from_millis(spec.interval_ms),
        max_duration: Some(std::time::Duration::from_millis(spec.duration_ms)),
        ..RecorderConfig::default()
    };
    let report = Recorder::new(source.as_mut(), cfg, None)?.run()?;

    let _ = child.kill();
    let _ = child.wait();

    let session_id = report.outcome.session_id;
    if let Some(root) = store_root {
        pf_daemonless_persist(&report, root, &spec.name, spec.interval_ms)?;
    }
    Ok((child, session_id))
}

/// Persist a bench-run session through the same store implementation the
/// daemon uses (chunk store + manifest), without a daemon.
fn pf_daemonless_persist(
    report: &pf_platform::record::RecorderReport,
    store_root: &std::path::Path,
    label: &str,
    interval_ms: u64,
) -> Result<(), PfError> {
    let store =
        pf_platform::session_store::SessionStore::create(store_root, report.outcome.session_id)?;
    store.append_samples(&report.snapshots)?;
    let backend = backend_factory()?;
    let caps = backend.capabilities();
    let manifest = pf_platform::session_store::manifest_v0(
        report.outcome.session_id,
        report.outcome.terminal_state(),
        caps.os.as_str(),
        &caps.os_version,
        &report.target_identity,
        &report.image_name,
        caps.logical_cpus,
        interval_ms,
        report.outcome.reason.as_str(),
        &[format!("label: {label}")],
        pf_platform::session_store::SCHEMA_0,
    );
    store.write_manifest(&manifest)?;
    // Chunk store for fast reads (PF-507 path).
    let mut writer = pf_storage::ChunkWriter::open(store.dir())?;
    for s in &report.snapshots {
        writer.push(s)?;
    }
    writer.seal()?;
    Ok(())
}

/// Wait for the child to exit, then load persisted snapshots.
fn wait_and_load(
    child: &mut std::process::Child,
    dir: &std::path::Path,
) -> Result<Vec<pf_types::CounterSnapshot>, PfError> {
    let _ = child.wait();
    let reader = pf_storage::ChunkReader::open(dir)?;
    if reader.total_records() > 0 {
        return reader.range(0, u64::MAX, 100_000).map(|p| p.snapshots);
    }
    let store = pf_platform::session_store::SessionStore::open(dir)?;
    store.read_samples().map(|(s, _)| s)
}

/// Add provenance blocks (PF-1102) to the comparison report.
fn attach_provenance(
    mut report: JsonValue,
    baseline: &WorkloadSpec,
    candidate: &WorkloadSpec,
    os: &str,
    logical_cpus: Option<u32>,
) -> JsonValue {
    use JsonValue as J;
    // The exe hash needs the binary; keep the prefix honest by hashing the
    // file bytes when readable (bounded prefix).
    // FNV over file bytes is deliberately wrapping.
    #[allow(clippy::arithmetic_side_effects)]
    let hash = |exe: &str| -> String {
        std::fs::read(exe)
            .map(|bytes| {
                // FNV-1a 64 — a provenance *prefix hint*, not a security
                // digest; the report says so.
                let mut h: u64 = 0xCBF2_9CE4_8422_2325;
                for b in &bytes {
                    h ^= u64::from(*b);
                    h = h.wrapping_mul(0x0000_0100_0000_01B3);
                }
                format!("fnv1a64:{h:016x} ({} bytes)", bytes.len())
            })
            .unwrap_or_else(|_| "unreadable".into())
    };
    let pairs = match &mut report {
        JsonValue::Object(pairs) => pairs,
        _ => return report,
    };
    pairs.push((
        "provenance".into(),
        J::Object(vec![
            ("os".into(), J::Str(os.into())),
            (
                "logical_cpus".into(),
                match logical_cpus {
                    Some(n) => J::UInt(u64::from(n)),
                    None => J::Null,
                },
            ),
            (
                "baseline".into(),
                baseline.provenance(caps_os(os), logical_cpus, &hash(&baseline.exe)),
            ),
            (
                "candidate".into(),
                candidate.provenance(caps_os(os), logical_cpus, &hash(&candidate.exe)),
            ),
        ]),
    ));
    report
}

fn caps_os(os: &str) -> pf_types::Os {
    if os == "linux" {
        pf_types::Os::Linux
    } else {
        pf_types::Os::Windows
    }
}

// Silence unused warning for ExitCode import (kept for future CLI refactor).
#[allow(dead_code)]
fn _exit_code_marker() -> ExitCode {
    ExitCode::SUCCESS
}
