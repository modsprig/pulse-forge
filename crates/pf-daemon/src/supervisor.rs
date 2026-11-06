//! Session supervisor (roadmap PF-401).
//!
//! Owns every recording session: target lifecycle, recorder worker threads,
//! finite-state transitions, cancellation and **idempotent stop**. The
//! supervisor — not any client — owns sessions, so a disconnecting client
//! never ends a recording (exit gate P4).
//!
//! State machine (`pf_types::SessionState`):
//! `Created → Starting → Recording → Stopping → Finalizing → terminal`,
//! where terminal is `Completed`, `Failed`, `Cancelled` or
//! `RecoveredPartial`. Workers finalize themselves: they persist the session
//! store, publish samples, and serialize their own final state transition
//! under the supervisor lock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_platform::contracts::PlatformBackend;
use pf_platform::paths;
use pf_platform::record::{Recorder, RecorderConfig, RecorderReport, StopReason};
use pf_platform::report;
use pf_platform::session_store::{SCHEMA_0, SessionStore, manifest_v0};
use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId, SessionState};

/// What to profile. Mirrors the CLI's two paths.
#[derive(Clone, Debug)]
pub enum TargetSpec {
    /// Attach to an existing PID. The target is never signalled.
    Attach { pid: u32 },
    /// Launch an executable directly with an argv (no shell, ever). The
    /// daemon becomes the direct parent; the child is terminated after the
    /// recording only as direct-parent cleanup.
    Launch { exe: String, args: Vec<String> },
}

/// Per-session request options (validated through `RecorderConfig`).
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub interval_ms: u64,
    pub duration_ms: u64,
    pub out_dir: Option<PathBuf>,
    pub label: Option<String>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            interval_ms: 250,
            duration_ms: 10_000,
            out_dir: None,
            label: None,
        }
    }
}

/// Snapshot view of one session, safe to hand to IPC clients.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: SessionId,
    pub state: SessionState,
    pub stop_reason: Option<String>,
    pub target_pid: Option<u32>,
    pub image: String,
    pub sample_count: usize,
    pub session_dir: Option<PathBuf>,
    pub report_path: Option<PathBuf>,
    pub recorded_ms: Option<u64>,
    pub warnings: Vec<String>,
}

struct SessionRecord {
    id: SessionId,
    spec: TargetSpec,
    options: SessionOptions,
    state: SessionState,
    stop_reason: Option<String>,
    target_pid: Option<u32>,
    image: String,
    samples: Arc<Mutex<Vec<pf_types::CounterSnapshot>>>,
    session_dir: Option<PathBuf>,
    report_path: Option<PathBuf>,
    recorded_ms: Option<u64>,
    warnings: Vec<String>,
    worker: Option<WorkerHandle>,
    stop_requested: bool,
}

struct WorkerHandle {
    cancel: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Typed supervisor errors; never a panic.
#[derive(Debug)]
pub enum SupervisorError {
    Pf(PfError),
    UnknownSession(SessionId),
    InvalidState {
        id: SessionId,
        from: String,
        wanted: String,
    },
}

impl From<PfError> for SupervisorError {
    fn from(e: PfError) -> Self {
        SupervisorError::Pf(e)
    }
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisorError::Pf(e) => write!(f, "{e}"),
            SupervisorError::UnknownSession(id) => write!(f, "no session {id}"),
            SupervisorError::InvalidState { id, from, wanted } => {
                write!(f, "session {id} is {from}, expected {wanted}")
            }
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<SupervisorError> for PfError {
    fn from(e: SupervisorError) -> PfError {
        match e {
            SupervisorError::Pf(p) => p,
            SupervisorError::UnknownSession(id) => {
                PfError::new(ErrorCode::SessionNotFound, format!("no session {id}"))
            }
            SupervisorError::InvalidState { id, from, wanted } => PfError::new(
                ErrorCode::InvalidArgument,
                format!("session {id} is {from}, expected {wanted}"),
            ),
        }
    }
}

/// The supervisor. One instance per daemon. Worker threads share the session
/// map through an `Arc` so they can finalize their own sessions.
pub struct SessionSupervisor {
    sessions: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
    backend_factory: Arc<dyn Fn() -> Result<Box<dyn PlatformBackend>, PfError> + Send + Sync>,
}

impl SessionSupervisor {
    pub fn new(
        backend_factory: Box<dyn Fn() -> Result<Box<dyn PlatformBackend>, PfError> + Send + Sync>,
    ) -> Self {
        SessionSupervisor {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            backend_factory: Arc::from(backend_factory),
        }
    }

    /// Create a session in `Created` state (no target interaction yet).
    pub fn create(&self, spec: TargetSpec) -> Result<SessionId, SupervisorError> {
        let id = SessionId::now_v7();
        let record = SessionRecord {
            id,
            spec,
            options: SessionOptions::default(),
            state: SessionState::Created,
            stop_reason: None,
            target_pid: None,
            image: String::new(),
            samples: Arc::new(Mutex::new(Vec::new())),
            session_dir: None,
            report_path: None,
            recorded_ms: None,
            warnings: Vec::new(),
            worker: None,
            stop_requested: false,
        };
        self.sessions
            .lock()
            .expect("supervisor mutex")
            .insert(id, record);
        Ok(id)
    }

    /// Start recording: validates options, resolves the target, spawns the
    /// self-finalizing worker thread, returns immediately.
    pub fn start(
        &self,
        id: SessionId,
        options: SessionOptions,
    ) -> Result<SessionState, SupervisorError> {
        RecorderConfig {
            interval: Duration::from_millis(options.interval_ms),
            max_duration: Some(Duration::from_millis(options.duration_ms)),
            ..RecorderConfig::default()
        }
        .validate()
        .map_err(SupervisorError::Pf)?;

        let mut sessions = self.sessions.lock().expect("supervisor mutex");
        let record = sessions
            .get_mut(&id)
            .ok_or(SupervisorError::UnknownSession(id))?;
        record.state = record
            .state
            .transition(SessionState::Starting)
            .and_then(|s| s.transition(SessionState::Recording))
            .map_err(|e| SupervisorError::InvalidState {
                id,
                from: e.from.as_str().into(),
                wanted: "CREATED".into(),
            })?;
        record.options = options.clone();

        // Resolve the target before spawning (launch failures must be
        // reported synchronously to the requesting client).
        let (pid, child) = match record.spec.clone() {
            TargetSpec::Attach { pid } => (pid, None),
            TargetSpec::Launch { exe, args } => {
                let mut cmd = std::process::Command::new(&exe);
                cmd.args(&args).stdin(std::process::Stdio::null());
                let c = cmd.spawn().map_err(|e| {
                    SupervisorError::Pf(
                        PfError::new(
                            ErrorCode::InvalidArgument,
                            format!("failed to launch `{exe}`: {e}"),
                        )
                        .with_remediation(
                            "pass an executable path directly; the daemon never invokes a shell",
                        ),
                    )
                })?;
                (c.id(), Some(c))
            }
        };
        record.target_pid = Some(pid);
        // Publish the session directory immediately: clients (GUI, agents)
        // must be able to read the durable chunk store while the recording
        // is still running, not only after finalization.
        {
            let store_root = options
                .out_dir
                .clone()
                .map(Ok)
                .unwrap_or_else(paths::sessions_root)
                .unwrap_or_else(|_| std::env::temp_dir().join("pulseforge-fallback-sessions"));
            record.session_dir = Some(store_root.join(format!("{}.pfsession", id.as_hyphenated())));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let worker = WorkerHandle {
            cancel: Arc::clone(&cancel),
            join: None,
        };
        record.worker = Some(worker);
        drop(sessions);

        let thread = std::thread::Builder::new()
            .name(format!("pf-recorder-{id}"))
            .spawn({
                let sessions = Arc::clone(&self.sessions);
                let factory = Arc::clone(&self.backend_factory);
                let cancel = Arc::clone(&cancel);
                let record_samples = {
                    let sessions = self.sessions.lock().expect("supervisor mutex");
                    sessions
                        .get(&id)
                        .map(|r| Arc::clone(&r.samples))
                        .expect("session exists under lock")
                };
                move || {
                    run_worker(sessions, factory, id, pid, options, cancel, record_samples);
                    // Direct-parent cleanup AFTER the recording finalized.
                    if let Some(mut c) = child {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            })
            .map_err(|e| {
                SupervisorError::Pf(PfError::new(
                    ErrorCode::BackendFailure,
                    format!("failed to spawn recorder thread: {e}"),
                ))
            })?;

        let mut sessions = self.sessions.lock().expect("supervisor mutex");
        if let Some(record) = sessions.get_mut(&id) {
            if let Some(w) = &mut record.worker {
                w.join = Some(thread);
            }
        }
        Ok(SessionState::Recording)
    }

    /// Request a stop. **Idempotent** (PF-401): stopping an already-stopping
    /// or terminal session is a successful no-op returning the current
    /// state. Never signals an attached target — the cancel flag only ends
    /// *recording*.
    pub fn stop(&self, id: SessionId) -> Result<SessionState, SupervisorError> {
        let mut sessions = self.sessions.lock().expect("supervisor mutex");
        let record = sessions
            .get_mut(&id)
            .ok_or(SupervisorError::UnknownSession(id))?;
        if record.state.is_terminal() || record.state == SessionState::Stopping {
            record.stop_requested = true;
            return Ok(record.state);
        }
        if record.state != SessionState::Recording {
            return Err(SupervisorError::InvalidState {
                id,
                from: record.state.as_str().into(),
                wanted: "RECORDING".into(),
            });
        }
        record.state = record
            .state
            .transition(SessionState::Stopping)
            .map_err(|e| SupervisorError::InvalidState {
                id,
                from: e.from.as_str().into(),
                wanted: "RECORDING".into(),
            })?;
        record.stop_requested = true;
        if let Some(worker) = &record.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        Ok(record.state)
    }

    /// Bounded wait until the session reaches a terminal state.
    #[allow(clippy::arithmetic_side_effects)] // Instant + bounded timeout
    pub fn wait(&self, id: SessionId, timeout: Duration) -> Result<SessionState, SupervisorError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let state = {
                let sessions = self.sessions.lock().expect("supervisor mutex");
                let record = sessions
                    .get(&id)
                    .ok_or(SupervisorError::UnknownSession(id))?;
                if record.state.is_terminal() {
                    record.state
                } else {
                    SessionState::Recording
                }
            };
            if state.is_terminal() {
                return Ok(state);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(self.view(id)?.state);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Join a finished worker (if any) and return the session view. Joining
    /// a panicked worker marks the session `Failed` instead of propagating.
    ///
    /// Lock discipline: the join happens OUTSIDE the sessions mutex — the
    /// worker needs that mutex to finalize itself, so joining under the
    /// lock would deadlock.
    pub fn finalize(&self, id: SessionId) -> Result<SessionView, SupervisorError> {
        // Take the join handle out under the lock, then release it. A
        // STILL-RUNNING worker is left alone: joining it would block the
        // caller for the whole recording (status queries must be instant).
        let join = {
            let mut sessions = self.sessions.lock().expect("supervisor mutex");
            let record = sessions
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownSession(id))?;
            let finished = record
                .worker
                .as_ref()
                .and_then(|w| w.join.as_ref())
                .is_some_and(|j| j.is_finished());
            let handle = if finished {
                record.worker.as_mut().and_then(|w| w.join.take())
            } else {
                None
            };
            if record.worker.as_ref().is_some_and(|w| w.join.is_none()) && !finished {
                // keep the worker; nothing to reap
            }
            handle
        };
        let panicked = join.map(|j| j.join().is_err()).unwrap_or(false);
        let mut sessions = self.sessions.lock().expect("supervisor mutex");
        let record = sessions
            .get_mut(&id)
            .ok_or(SupervisorError::UnknownSession(id))?;
        record.worker = None;
        if panicked {
            // Worker panicked (a bug): record it honestly.
            record.state = SessionState::Failed;
            record
                .warnings
                .push("worker thread panicked; partial data only".into());
        }
        Ok(view_locked(record))
    }

    pub fn view(&self, id: SessionId) -> Result<SessionView, SupervisorError> {
        let sessions = self.sessions.lock().expect("supervisor mutex");
        let record = sessions
            .get(&id)
            .ok_or(SupervisorError::UnknownSession(id))?;
        Ok(view_locked(record))
    }

    pub fn list(&self) -> Vec<SessionView> {
        let sessions = self.sessions.lock().expect("supervisor mutex");
        let mut views: Vec<SessionView> = sessions.values().map(view_locked).collect();
        views.sort_by_key(|v| v.id);
        views
    }

    /// Raw samples of a session (bounded by the recorder's sample cap).
    pub fn samples(
        &self,
        id: SessionId,
    ) -> Result<Vec<pf_types::CounterSnapshot>, SupervisorError> {
        let sessions = self.sessions.lock().expect("supervisor mutex");
        let record = sessions
            .get(&id)
            .ok_or(SupervisorError::UnknownSession(id))?;
        Ok(record.samples.lock().map(|s| s.clone()).unwrap_or_default())
    }

    /// Number of live sessions (non-terminal).
    pub fn active_count(&self) -> usize {
        let sessions = self.sessions.lock().expect("supervisor mutex");
        sessions.values().filter(|r| !r.state.is_terminal()).count()
    }

    /// Delete a terminal session: remove the in-memory record AND its
    /// on-disk `.pfsession` store, returning bytes freed.
    ///
    /// Refuses live sessions with [`SupervisorError::InvalidState`] —
    /// stop first. An id unknown to this supervisor falls back to the
    /// default sessions root (older daemon runs may have left a directory
    /// this process never adopted); if nothing exists there either, the
    /// error is [`SupervisorError::UnknownSession`]. If the record exists
    /// but its directory is already gone, the record is still removed and
    /// 0 bytes are reported — deleting is idempotent at the record level.
    pub fn delete(&self, id: SessionId) -> Result<u64, SupervisorError> {
        let dir: Option<PathBuf> = {
            let sessions = self.sessions.lock().expect("supervisor mutex");
            match sessions.get(&id) {
                Some(record) => {
                    if !record.state.is_terminal() {
                        return Err(SupervisorError::InvalidState {
                            id,
                            from: record.state.as_str().into(),
                            wanted: "terminal — stop the session before deleting".into(),
                        });
                    }
                    Some(
                        record
                            .session_dir
                            .clone()
                            .unwrap_or_else(|| default_session_dir(id)),
                    )
                }
                None => {
                    let fallback = default_session_dir(id);
                    (fallback.exists()).then_some(fallback)
                }
            }
        };

        // Delete on disk OUTSIDE the supervisor lock — the recursive
        // remove can take real time on slow disks.
        let freed = match dir {
            Some(dir) => match pf_storage::delete_session_dir(&dir) {
                Ok(bytes) => bytes,
                // Record exists but nothing on disk: still drop the record.
                Err(e) if e.code == pf_types::ErrorCode::SessionNotFound => 0,
                Err(e) => return Err(SupervisorError::Pf(e)),
            },
            None => return Err(SupervisorError::UnknownSession(id)),
        };

        let mut sessions = self.sessions.lock().expect("supervisor mutex");
        sessions.remove(&id);
        Ok(freed)
    }
}

/// Default on-disk location for a session id (`<sessions_root>/<uuid>.pfsession`).
fn default_session_dir(id: SessionId) -> PathBuf {
    let root = paths::sessions_root()
        .unwrap_or_else(|_| std::env::temp_dir().join("pulseforge-fallback-sessions"));
    root.join(format!("{}.pfsession", id.as_hyphenated()))
}

fn view_locked(record: &SessionRecord) -> SessionView {
    SessionView {
        id: record.id,
        state: record.state,
        stop_reason: record.stop_reason.clone(),
        target_pid: record.target_pid,
        image: record.image.clone(),
        sample_count: record.samples.lock().map(|s| s.len()).unwrap_or(0),
        session_dir: record.session_dir.clone(),
        report_path: record.report_path.clone(),
        recorded_ms: record.recorded_ms,
        warnings: record.warnings.clone(),
    }
}

/// Worker body: record, then finalize the session record in place.
///
/// Every outcome — including target-exit mid-recording and collector errors
/// — reaches a terminal state with data preserved; nothing is left
/// `RECORDING` forever.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    sessions: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
    factory: Arc<dyn Fn() -> Result<Box<dyn PlatformBackend>, PfError> + Send + Sync>,
    id: SessionId,
    pid: u32,
    options: SessionOptions,
    cancel: Arc<AtomicBool>,
    record_samples: Arc<Mutex<Vec<pf_types::CounterSnapshot>>>,
) {
    // Persist the session record BEFORE recording starts: a daemon crash
    // mid-recording then leaves a RECOVERED_PARTIAL orphan on restart
    // (PF-407) instead of vanishing. Sample durability itself arrives with
    // the chunked store (P5); today the *record* survives.
    let store_root = options
        .out_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(paths::sessions_root)
        .unwrap_or_else(|_| std::env::temp_dir().join("pulseforge-fallback-sessions"));
    let _ = std::fs::create_dir_all(&store_root);
    let store = SessionStore::create(&store_root, id).ok();
    let chunk_writer = store
        .as_ref()
        .and_then(|st| pf_storage::ChunkWriter::open(st.dir()).ok())
        .map(|w| w.with_flush_period(Duration::from_millis(1000)))
        .map(|w| Arc::new(std::sync::Mutex::new(w)));
    // PF-1001/1002 (Linux): perf stack sampler alongside the counter source.
    // The /proc/<pid>/maps TEXT is captured at start (used later to resolve
    // sampled leaf IPs); the pid may be gone by finalize time.
    #[cfg(target_os = "linux")]
    let maps_at_start: Option<String> = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok();
    #[cfg(not(target_os = "linux"))]
    let _maps_at_start: Option<String> = None;
    #[cfg(target_os = "linux")]
    let mut stack_recorder: Option<crate::stacks::StackRecorder> =
        pf_linux::perf::probe().ok().and_then(|_| {
            crate::stacks::StackRecorder::open(
                i32::try_from(pid).unwrap_or(0),
                1_000_000, // 1 ms period
                20_000,    // bounded; truncated flag past the cap
                Arc::clone(&cancel),
            )
            .ok()
        });
    #[cfg(not(target_os = "linux"))]
    let _stack_recorder: Option<()> = None;

    // Publish the live session directory and an in-progress manifest so a
    // crash mid-recording leaves a recoverable record (PF-407). The real
    // OS string goes in (clients like `report` reject an unknown OS).
    if let Some(store) = &store {
        let backend = (factory)();
        let (os_str, os_version, logical_cpus) = match backend {
            Ok(b) => {
                let caps = b.capabilities();
                (
                    caps.os.as_str().to_string(),
                    caps.os_version.clone(),
                    caps.logical_cpus,
                )
            }
            Err(_) => ("unknown".to_string(), String::new(), None),
        };
        let identity_hint = pf_types::ProcessIdentity::new(
            if os_str == "linux" {
                pf_types::Os::Linux
            } else {
                pf_types::Os::Windows
            },
            pid,
            0,
            "",
        );
        let manifest = manifest_v0(
            id,
            SessionState::Recording,
            &os_str,
            &os_version,
            &identity_hint,
            "",
            logical_cpus,
            options.interval_ms,
            "in_progress",
            &[],
            SCHEMA_0,
        );
        let _ = store.write_manifest(&manifest);
    }

    let result: Result<
        (
            RecorderReport,
            String,
            pf_platform::contracts::PlatformCapabilities,
        ),
        PfError,
    > = (|| {
        let backend = factory()?;
        let caps = backend.capabilities();
        let mut source = backend.open(pid)?;
        let image = source.image_name().to_string();
        let cfg = RecorderConfig {
            interval: Duration::from_millis(options.interval_ms),
            max_duration: Some(Duration::from_millis(options.duration_ms)),
            ..RecorderConfig::default()
        };
        let shared = Arc::clone(&record_samples);
        let chunks = chunk_writer.clone();
        let mut sink = move |s: &pf_types::CounterSnapshot| {
            if let Ok(mut v) = shared.lock() {
                v.push(s.clone());
            }
            if let Some(cw) = &chunks {
                if let Ok(mut w) = cw.lock() {
                    // Durability path: sealed chunks survive a daemon kill;
                    // push errors (full disk, sealed writer) must never
                    // disturb the recording itself.
                    let _ = w.push(s);
                }
            }
        };
        let report = Recorder::with_options(
            source.as_mut(),
            cfg,
            Some(&cancel),
            Some(&mut sink),
            Some(id),
        )?
        .run()?;
        Ok((report, image, caps))
    })();

    let mut sessions = sessions.lock().expect("supervisor mutex");
    let Some(record) = sessions.get_mut(&id) else {
        return; // session was dropped from the map; nothing to update
    };
    // Publish the live session directory on the already-borrowed record.
    record.session_dir = store.as_ref().map(|st| st.dir().to_path_buf());

    match result {
        Ok((rep, image, caps)) => {
            // Stack sampler lifecycle: stop + join + persist (PF-1001/1002).
            #[cfg(target_os = "linux")]
            let (stack_samples, stack_lost, stack_kernel_skipped, stack_truncated) =
                if let Some(sr) = stack_recorder.as_mut() {
                    sr.request_stop();
                    let samples = sr.join();
                    (samples, sr.lost(), sr.kernel_skipped(), sr.truncated())
                } else {
                    (Vec::new(), 0u64, 0u64, false)
                };
            if let Some(cw) = &chunk_writer {
                if let Ok(mut w) = cw.lock() {
                    let _ = w.seal();
                }
            }
            // Persist final data + terminal manifest, then transition.
            let persist = store
                .as_ref()
                .ok_or_else(|| PfError::new(ErrorCode::StorageIo, "session store unavailable"))
                .and_then(|st| {
                    st.append_samples(&rep.snapshots)?;
                    let mut warnings = rep.outcome.warnings.clone();
                    warnings.extend(label_warning(&options));
                    // PF-1001/1002: persist sampled stacks first so the
                    // manifest and report can carry the honest outcome.
                    #[cfg(target_os = "linux")]
                    let stacks_summary: Option<pf_types::json::JsonValue> = {
                        let maps = maps_at_start
                            .as_deref()
                            .map(pf_linux::symbols::parse_maps)
                            .unwrap_or_default();
                        match crate::stacks::persist_stacks(
                            st.dir(),
                            &stack_samples,
                            stack_lost,
                            stack_kernel_skipped,
                            stack_truncated,
                            &maps,
                        ) {
                            Ok((_, summary)) => summary,
                            Err(e) => {
                                warnings.push(format!("stack persistence failed: {e}"));
                                None
                            }
                        }
                    };
                    #[cfg(not(target_os = "linux"))]
                    let stacks_summary: Option<pf_types::json::JsonValue> = None;
                    let manifest = manifest_v0(
                        id,
                        rep.outcome.terminal_state(),
                        caps.os.as_str(),
                        &caps.os_version,
                        &rep.target_identity,
                        &image,
                        caps.logical_cpus,
                        options.interval_ms,
                        rep.outcome.reason.as_str(),
                        &warnings,
                        SCHEMA_0,
                    );
                    st.write_manifest(&manifest)?;
                    let json = pf_platform::report::from_recorder(
                        &rep,
                        caps.logical_cpus,
                        stacks_summary.as_ref(),
                    );
                    let report_path = st.dir().join("report.json");
                    std::fs::write(
                        &report_path,
                        format!(
                            "{}
",
                            json.to_json_string()
                        ),
                    )
                    .map_err(PfError::from)?;
                    Ok((st.dir().to_path_buf(), report_path))
                });
            // Samples were published live via the sink; the authoritative
            // final copy replaces any partial view.
            if let Ok(mut samples) = record.samples.lock() {
                *samples = rep.snapshots.clone();
            }
            record.image = image;
            record.recorded_ms = Some(rep.outcome.recorded_ns / 1_000_000);
            record.warnings.extend(rep.outcome.warnings.iter().cloned());
            record.stop_reason = Some(rep.outcome.reason.as_str().to_string());
            match persist {
                Ok((dir, report_path)) => {
                    record.session_dir = Some(dir);
                    record.report_path = Some(report_path);
                }
                Err(e) => {
                    record
                        .warnings
                        .push(format!("session persistence failed: {e}"));
                }
            }
            let terminal = rep.outcome.terminal_state();
            // Transition through Finalizing; state may currently be
            // Recording (target exit path) or Stopping (client stop path).
            record.state = match record.state {
                SessionState::Stopping => record
                    .state
                    .transition(SessionState::Finalizing)
                    .unwrap_or(SessionState::Finalizing),
                _ => SessionState::Finalizing,
            };
            record.state = record
                .state
                .transition(terminal)
                .unwrap_or(SessionState::Failed);
            let _ = StopReason::Cancelled; // (reason surfaced via stop_reason)
        }
        Err(e) => {
            #[cfg(target_os = "linux")]
            if let Some(sr) = stack_recorder.as_mut() {
                sr.request_stop();
                sr.join();
            }
            if let Some(cw) = &chunk_writer {
                if let Ok(mut w) = cw.lock() {
                    let _ = w.seal();
                }
            }
            record.stop_reason = Some(e.code.as_str().to_string());
            record.warnings.push(e.to_string());
            // Rewrite the in-progress manifest to FAILED (no orphan lies
            // around claiming an active recording).
            if let Some(store) = &store {
                let identity_hint =
                    pf_types::ProcessIdentity::new(pf_types::Os::Windows, pid, 0, "");
                let manifest = manifest_v0(
                    id,
                    SessionState::Failed,
                    "unknown",
                    "",
                    &identity_hint,
                    "",
                    None,
                    options.interval_ms,
                    e.code.as_str(),
                    &[e.to_string()],
                    SCHEMA_0,
                );
                let _ = store.write_manifest(&manifest);
            }
            record.state = match record.state {
                SessionState::Stopping => record
                    .state
                    .transition(SessionState::Finalizing)
                    .and_then(|s| s.transition(SessionState::Failed))
                    .unwrap_or(SessionState::Failed),
                _ => SessionState::Failed,
            };
        }
    }
}

fn label_warning(options: &SessionOptions) -> Vec<String> {
    options
        .label
        .as_ref()
        .map(|l| vec![format!("label: {l}")])
        .unwrap_or_default()
}

/// Persist a finished recording into the session store and produce the
/// report JSON path. Shared by supervisor workers and the in-process CLI
/// path (single implementation of "session on disk").
#[allow(clippy::too_many_arguments)]
pub fn persist_recording(
    report: &RecorderReport,
    store_root: &std::path::Path,
    image_name: &str,
    os: &str,
    os_version: &str,
    logical_cpus: Option<u32>,
    extra_warnings: &[String],
    interval_ms: u64,
) -> Result<(PathBuf, PathBuf), PfError> {
    let store = SessionStore::create(store_root, report.outcome.session_id)?;
    store.append_samples(&report.snapshots)?;
    let mut warnings = report.outcome.warnings.clone();
    warnings.extend_from_slice(extra_warnings);
    let manifest = manifest_v0(
        report.outcome.session_id,
        report.outcome.terminal_state(),
        os,
        os_version,
        &report.target_identity,
        image_name,
        logical_cpus,
        interval_ms,
        report.outcome.reason.as_str(),
        &warnings,
        SCHEMA_0,
    );
    store.write_manifest(&manifest)?;
    let json = report::from_recorder(report, logical_cpus, None);
    let report_path = store.dir().join("report.json");
    std::fs::write(&report_path, format!("{}\n", json.to_json_string())).map_err(PfError::from)?;
    Ok((store.dir().to_path_buf(), report_path))
}

impl SessionView {
    pub fn to_json(&self) -> JsonValue {
        use pf_types::json::JsonValue as J;
        J::Object(vec![
            ("session_id".into(), J::Str(self.id.as_hyphenated())),
            ("state".into(), J::Str(self.state.as_str().into())),
            (
                "stop_reason".into(),
                match &self.stop_reason {
                    Some(r) => J::Str(r.clone()),
                    None => J::Null,
                },
            ),
            (
                "target_pid".into(),
                match self.target_pid {
                    Some(p) => J::UInt(u64::from(p)),
                    None => J::Null,
                },
            ),
            ("image".into(), J::Str(self.image.clone())),
            ("samples".into(), J::UInt(self.sample_count as u64)),
            (
                "session_dir".into(),
                match &self.session_dir {
                    Some(p) => J::Str(p.display().to_string()),
                    None => J::Null,
                },
            ),
            (
                "report_path".into(),
                match &self.report_path {
                    Some(p) => J::Str(p.display().to_string()),
                    None => J::Null,
                },
            ),
            (
                "recorded_ms".into(),
                match self.recorded_ms {
                    Some(ms) => J::UInt(ms),
                    None => J::Null,
                },
            ),
            (
                "warnings".into(),
                J::Array(self.warnings.iter().map(|w| J::Str(w.clone())).collect()),
            ),
        ])
    }
}
