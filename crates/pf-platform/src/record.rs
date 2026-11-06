//! The OS-neutral recording loop (roadmap PF-206/207 semantics).
//!
//! Polls a [`CounterSource`] at a fixed interval on the session clock,
//! records raw snapshots, and stops on: duration budget, sample-count cap,
//! target exit, identity mismatch, or caller cancellation. Every stop reason
//! is surfaced — a partial recording is labelled partial, never completed.

use std::time::{Duration, Instant};

use pf_types::{ErrorCode, PfError, SessionId, SessionState};

use crate::contracts::CounterSource;

/// Hard bounds that protect both the target and PulseForge itself.
#[derive(Clone, Debug)]
pub struct RecorderConfig {
    /// Polling interval. Valid range: 100 ms – 5 s (architecture §5.3).
    pub interval: Duration,
    /// Recording wall-time budget. `None` = until stopped by sample cap or
    /// caller. Callers should always pass one for unattended runs.
    pub max_duration: Option<Duration>,
    /// Hard cap on stored raw samples (memory bound). Default 100_000.
    pub max_samples: usize,
    /// Warn threshold: polling below this has a measured-cost caveat.
    pub warn_below: Duration,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        RecorderConfig {
            interval: Duration::from_millis(250),
            max_duration: None,
            max_samples: 100_000,
            warn_below: Duration::from_millis(200),
        }
    }
}

impl RecorderConfig {
    pub fn validate(&self) -> Result<(), PfError> {
        let min = Duration::from_millis(100);
        let max = Duration::from_secs(5);
        if self.interval < min || self.interval > max {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "interval must be between 100 ms and 5 s, got {} ms",
                    self.interval.as_millis()
                ),
            ));
        }
        if self.max_samples == 0 || self.max_samples > 1_000_000 {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "max_samples must be within 1..=1_000_000",
            ));
        }
        Ok(())
    }

    pub fn warns_on_interval(&self) -> bool {
        self.interval < self.warn_below
    }
}

/// Why the recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    DurationReached,
    SampleCapReached,
    TargetExited,
    IdentityMismatch,
    CollectorError,
    Cancelled,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::DurationReached => "duration_reached",
            StopReason::SampleCapReached => "sample_cap_reached",
            StopReason::TargetExited => "target_exited",
            StopReason::IdentityMismatch => "identity_mismatch",
            StopReason::CollectorError => "collector_error",
            StopReason::Cancelled => "cancelled",
        }
    }
}

/// How the recorder ended. Maps onto [`SessionState`].
#[derive(Clone, Debug)]
pub struct RecorderOutcome {
    pub session_id: SessionId,
    pub reason: StopReason,
    pub started_at: Instant,
    /// Session-clock duration actually recorded.
    pub recorded_ns: u64,
    /// True when at least one snapshot was captured.
    pub has_any_data: bool,
    /// Structured warnings collected during the run.
    pub warnings: Vec<String>,
}

impl RecorderOutcome {
    pub fn terminal_state(&self) -> SessionState {
        match self.reason {
            StopReason::DurationReached | StopReason::SampleCapReached => {
                if self.has_any_data {
                    SessionState::Completed
                } else {
                    SessionState::Failed
                }
            }
            StopReason::TargetExited if self.has_any_data => SessionState::Completed,
            StopReason::Cancelled if self.has_any_data => SessionState::Completed,
            StopReason::IdentityMismatch if self.has_any_data => SessionState::Completed,
            StopReason::TargetExited | StopReason::Cancelled | StopReason::IdentityMismatch => {
                SessionState::Failed
            }
            StopReason::CollectorError => {
                if self.has_any_data {
                    SessionState::RecoveredPartial
                } else {
                    SessionState::Failed
                }
            }
        }
    }
}

/// Full recorder result: raw series + outcome.
pub struct RecorderReport {
    pub outcome: RecorderOutcome,
    pub snapshots: Vec<pf_types::CounterSnapshot>,
    pub target_identity: pf_types::ProcessIdentity,
    pub image_name: String,
    /// Actual observed poll interval statistics (measured, not requested).
    pub poll_cost_p50_ms: f64,
    pub poll_cost_max_ms: f64,
}

/// Live sample sink: called once per captured snapshot so supervisors can
/// report progress while a recording runs (architecture `profile_status`).
pub type SampleSink<'b> = &'b mut (dyn FnMut(&pf_types::CounterSnapshot) + Send);

/// Polling recorder. Generic over source so tests can drive a fake; the CLI
/// drives the real Windows backend.
pub struct Recorder<'a> {
    source: &'a mut dyn CounterSource,
    config: RecorderConfig,
    session_id: SessionId,
    cancel: Option<&'a std::sync::atomic::AtomicBool>,
    sink: Option<SampleSink<'a>>,
}

impl<'a> Recorder<'a> {
    pub fn new(
        source: &'a mut dyn CounterSource,
        config: RecorderConfig,
        cancel: Option<&'a std::sync::atomic::AtomicBool>,
    ) -> Result<Self, PfError> {
        Self::with_sink(source, config, cancel, None)
    }

    pub fn with_sink(
        source: &'a mut dyn CounterSource,
        config: RecorderConfig,
        cancel: Option<&'a std::sync::atomic::AtomicBool>,
        sink: Option<SampleSink<'a>>,
    ) -> Result<Self, PfError> {
        Self::with_options(source, config, cancel, sink, None)
    }

    /// Full constructor: optional live sample sink and an externally owned
    /// session id (supervisors own the id; standalone callers get a fresh
    /// UUIDv7).
    pub fn with_options(
        source: &'a mut dyn CounterSource,
        config: RecorderConfig,
        cancel: Option<&'a std::sync::atomic::AtomicBool>,
        sink: Option<SampleSink<'a>>,
        session_id: Option<SessionId>,
    ) -> Result<Self, PfError> {
        config.validate()?;
        Ok(Recorder {
            source,
            config,
            session_id: session_id.unwrap_or_else(SessionId::now_v7),
            cancel,
            sink,
        })
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Run the polling loop to completion. Returns raw snapshots and the
    /// outcome; never panics across the collector boundary.
    // Index arithmetic below is guarded by the non-empty poll-gaps check;
    // loop counters are capped by config validation.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn run(mut self) -> Result<RecorderReport, PfError> {
        let started = Instant::now();
        let mut snapshots: Vec<pf_types::CounterSnapshot> =
            Vec::with_capacity(self.config.max_samples.min(8_192));
        let mut warnings = Vec::new();
        if self.config.warns_on_interval() {
            warnings.push(format!(
                "polling interval {} ms is below the 200 ms measured-cost threshold; \
                 expect scheduling jitter to dominate rate accuracy",
                self.config.interval.as_millis()
            ));
        }

        let mut poll_gaps_ms: Vec<f64> = Vec::new();
        let reason = loop {
            if let Some(flag) = self.cancel {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break StopReason::Cancelled;
                }
            }
            if let Some(max) = self.config.max_duration {
                if started.elapsed() >= max {
                    break StopReason::DurationReached;
                }
            }
            if snapshots.len() >= self.config.max_samples {
                break StopReason::SampleCapReached;
            }

            let before_poll = Instant::now();
            // The session clock is Instant-based (QPC on Windows): strictly
            // monotonic across the process lifetime.
            let mono_ns = started.elapsed().as_nanos();
            let mono_ns = u64::try_from(mono_ns).unwrap_or(u64::MAX);
            match self.source.snapshot(mono_ns) {
                Ok(s) => {
                    if let Some(sink) = &mut self.sink {
                        sink(&s);
                    }
                    snapshots.push(s);
                }
                Err(e) => {
                    // The OS never let us see this process: propagate the
                    // typed permission error instead of an empty session.
                    if e.code == ErrorCode::PermissionDenied && snapshots.is_empty() {
                        return Err(e);
                    }
                    if e.code == ErrorCode::TargetExited {
                        break StopReason::TargetExited;
                    }
                    if e.code == ErrorCode::IdentityMismatch {
                        break StopReason::IdentityMismatch;
                    }
                    // Any other error (including mid-run permission loss)
                    // ends the recording; data captured so far is kept and
                    // the session finalizes as partial.
                    warnings.push(format!("collector error: {e}"));
                    break StopReason::CollectorError;
                }
            }

            // Sleep to the next tick; measure real poll cost (scheduling
            // jitter shows up here and is reported, not hidden).
            let poll_cost = before_poll.elapsed();
            if let Some(remaining) = self.config.interval.checked_sub(poll_cost) {
                std::thread::sleep(remaining);
            }
            poll_gaps_ms.push(poll_cost.as_secs_f64() * 1000.0);
        };

        let recorded_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let identity = self.source.identity().clone();
        let image_name = self.source.image_name().to_string();
        let has_any_data = !snapshots.is_empty();
        let outcome = RecorderOutcome {
            session_id: self.session_id,
            reason,
            started_at: started,
            recorded_ns,
            has_any_data,
            warnings,
        };
        let mut poll_cost_p50_ms = 0.0;
        let mut poll_cost_max_ms = 0.0;
        if !poll_gaps_ms.is_empty() {
            let mut sorted = poll_gaps_ms.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            poll_cost_p50_ms = sorted[sorted.len() / 2];
            poll_cost_max_ms = sorted[sorted.len() - 1];
        }
        Ok(RecorderReport {
            outcome,
            snapshots,
            target_identity: identity,
            image_name,
            poll_cost_p50_ms,
            poll_cost_max_ms,
        })
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use pf_types::{Os, ProcessIdentity};
    use std::sync::atomic::AtomicBool;

    struct FakeSource {
        identity: ProcessIdentity,
        exit_after: Option<usize>,
        calls: usize,
    }

    impl CounterSource for FakeSource {
        fn identity(&self) -> &ProcessIdentity {
            &self.identity
        }
        fn image_name(&self) -> &str {
            "fake.exe"
        }
        fn snapshot(&mut self, mono_ns: u64) -> Result<pf_types::CounterSnapshot, PfError> {
            self.calls += 1;
            if let Some(n) = self.exit_after {
                if self.calls > n {
                    return Err(PfError::new(ErrorCode::TargetExited, "process exited"));
                }
            }
            Ok(pf_types::CounterSnapshot {
                mono_ns,
                cpu_user_ns: Some(u64::try_from(self.calls).unwrap_or(0) * 1_000_000),
                cpu_kernel_ns: Some(0),
                resident_bytes: Some(4096),
                ..pf_types::CounterSnapshot::default()
            })
        }
    }

    fn fake() -> FakeSource {
        FakeSource {
            identity: ProcessIdentity::new(Os::Windows, 42, 1000, "boot-test"),
            exit_after: None,
            calls: 0,
        }
    }

    #[test]
    fn config_rejects_out_of_range_interval() {
        let c = |interval| RecorderConfig {
            interval,
            ..RecorderConfig::default()
        };
        assert_eq!(
            c(Duration::from_millis(50)).validate().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            c(Duration::from_secs(6)).validate().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert!(c(Duration::from_millis(100)).validate().is_ok());
        let zero_samples = RecorderConfig {
            max_samples: 0,
            ..RecorderConfig::default()
        };
        assert_eq!(
            zero_samples.validate().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn duration_budget_ends_recording_with_data() {
        let mut src = fake();
        let cfg = RecorderConfig {
            interval: Duration::from_millis(100),
            max_duration: Some(Duration::from_millis(350)),
            ..RecorderConfig::default()
        };
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert_eq!(report.outcome.reason, StopReason::DurationReached);
        assert!(report.outcome.has_any_data);
        assert_eq!(report.outcome.terminal_state(), SessionState::Completed);
        assert!(!report.snapshots.is_empty());
    }

    #[test]
    fn target_exit_yields_partial_then_completed_if_data() {
        let mut src = fake();
        src.exit_after = Some(3);
        let cfg = RecorderConfig {
            interval: Duration::from_millis(100),
            max_duration: Some(Duration::from_secs(30)),
            ..RecorderConfig::default()
        };
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert_eq!(report.outcome.reason, StopReason::TargetExited);
        assert_eq!(report.snapshots.len(), 3);
        assert_eq!(report.outcome.terminal_state(), SessionState::Completed);
    }

    #[test]
    fn target_exit_with_zero_data_is_failed() {
        let mut src = fake();
        src.exit_after = Some(0);
        let cfg = RecorderConfig::default();
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert_eq!(report.outcome.reason, StopReason::TargetExited);
        assert!(!report.outcome.has_any_data);
        assert_eq!(report.outcome.terminal_state(), SessionState::Failed);
    }

    #[test]
    fn cancellation_flag_stops_loop() {
        let flag = AtomicBool::new(true);
        let mut src = fake();
        let report = Recorder::new(
            &mut src,
            RecorderConfig {
                interval: Duration::from_millis(100),
                ..RecorderConfig::default()
            },
            Some(&flag),
        )
        .unwrap()
        .run()
        .unwrap();
        assert_eq!(report.outcome.reason, StopReason::Cancelled);
        assert!(report.snapshots.is_empty());
    }

    #[test]
    fn sample_cap_bounds_memory() {
        let mut src = fake();
        let cfg = RecorderConfig {
            interval: Duration::from_millis(100),
            max_samples: 3,
            max_duration: Some(Duration::from_secs(60)),
            ..RecorderConfig::default()
        };
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert_eq!(report.outcome.reason, StopReason::SampleCapReached);
        assert_eq!(report.snapshots.len(), 3);
    }

    #[test]
    fn fast_interval_produces_measured_cost_warning() {
        let mut src = fake();
        let cfg = RecorderConfig {
            interval: Duration::from_millis(100),
            max_duration: Some(Duration::from_millis(150)),
            ..RecorderConfig::default()
        };
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert!(
            report
                .outcome
                .warnings
                .iter()
                .any(|w| w.contains("measured-cost threshold")),
            "warnings: {:?}",
            report.outcome.warnings
        );
    }

    #[test]
    fn default_interval_does_not_warn() {
        let mut src = fake();
        let cfg = RecorderConfig {
            max_duration: Some(Duration::from_millis(300)),
            ..RecorderConfig::default()
        };
        let report = Recorder::new(&mut src, cfg, None).unwrap().run().unwrap();
        assert!(report.outcome.warnings.is_empty());
    }
}
