//! Report rendering shared by `record` and `report` (roadmap PF-208): both
//! paths produce byte-identical summaries for the same session — there is
//! exactly one metric implementation in the product.

use crate::record::RecorderReport;
use pf_types::json::JsonValue;
use pf_types::metrics::{MetricId, MetricSummary, derive_samples};
use pf_types::{CounterSnapshot, ProcessIdentity, SessionState};

use JsonValue as J;

/// All public metrics, in canonical output order.
pub const REPORT_METRICS: [MetricId; 16] = [
    MetricId::CpuCorePct,
    MetricId::CpuMachinePct,
    MetricId::CpuUserNs,
    MetricId::CpuKernelNs,
    MetricId::CpuTotalNs,
    MetricId::ResidentBytes,
    MetricId::ResidentPeakBytes,
    MetricId::PrivateCommitBytes,
    MetricId::PrivateCommitPeakBytes,
    MetricId::IoReadOps,
    MetricId::IoWriteOps,
    MetricId::IoReadBytes,
    MetricId::IoWriteBytes,
    MetricId::ThreadCount,
    MetricId::HandleCount,
    MetricId::PageFaults,
];

fn identity_json(id: &ProcessIdentity) -> JsonValue {
    J::Object(vec![
        ("os".into(), J::Str(id.os.as_str().into())),
        ("pid".into(), J::UInt(u64::from(id.pid))),
        ("creation_identity".into(), J::UInt(id.creation_identity)),
        ("boot_identity".into(), J::Str(id.boot_identity.clone())),
    ])
}

/// Inputs to [`build_report`] — grouped so the builder signature stays
/// reviewable.
pub struct ReportInput<'a> {
    pub schema_label: &'a str,
    pub session_id: &'a pf_types::SessionId,
    pub state: SessionState,
    pub stop_reason: &'a str,
    pub identity: &'a ProcessIdentity,
    pub image_name: &'a str,
    pub snapshots: &'a [CounterSnapshot],
    pub logical_cpus: Option<u32>,
    pub warnings: &'a [String],
    pub poll_p50_ms: f64,
    pub poll_max_ms: f64,
    pub partial_read: Option<&'a str>,
    /// Parsed `stacks-summary.json` when the session carried sampled stacks
    /// (Linux perf pipeline, PF-1001/1002). `None` = stacks unavailable;
    /// absence is never rendered as zero.
    pub stacks_summary: Option<&'a JsonValue>,
}

/// The single report builder: raw snapshots + metadata in, typed JSON out.
pub fn build_report(input: ReportInput<'_>) -> JsonValue {
    let ReportInput {
        schema_label,
        session_id,
        state,
        stop_reason,
        identity,
        image_name,
        snapshots,
        logical_cpus,
        warnings,
        poll_p50_ms,
        poll_max_ms,
        partial_read,
        stacks_summary,
    } = input;
    build_report_fields(
        schema_label,
        session_id,
        state,
        stop_reason,
        identity,
        image_name,
        snapshots,
        logical_cpus,
        warnings,
        poll_p50_ms,
        poll_max_ms,
        partial_read,
        stacks_summary,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_report_fields(
    schema_label: &str,
    session_id: &pf_types::SessionId,
    state: SessionState,
    stop_reason: &str,
    identity: &ProcessIdentity,
    image_name: &str,
    snapshots: &[CounterSnapshot],
    logical_cpus: Option<u32>,
    warnings: &[String],
    poll_p50_ms: f64,
    poll_max_ms: f64,
    partial_read: Option<&str>,
    stacks_summary: Option<&JsonValue>,
) -> JsonValue {
    let derived = derive_samples(snapshots, logical_cpus);
    let mut metric_pairs = Vec::with_capacity(REPORT_METRICS.len());
    for id in REPORT_METRICS {
        let summary = MetricSummary::summarize(id, &derived);
        metric_pairs.push((id.name().to_string(), summary.to_json()));
    }
    // Availability of cpu_core_pct must never be silently "available" when
    // no rate could be computed at all.
    let mut all_warnings: Vec<String> = warnings.to_vec();
    if let Some(p) = partial_read {
        all_warnings.push(format!("PARTIAL_DATA: {p}"));
    }
    let cpu_accounted = snapshots.windows(2).any(|w| {
        let total = |s: &CounterSnapshot| {
            s.cpu_user_ns
                .unwrap_or(0)
                .saturating_add(s.cpu_kernel_ns.unwrap_or(0))
        };
        total(&w[1]) > total(&w[0])
    });
    if !snapshots.is_empty() && !cpu_accounted {
        all_warnings.push(
            "ENVIRONMENT: process CPU time counters report zero on this machine for this \
             process (observed in some sandboxes; cross-checked with an independent tool \
             during probing) — CPU rates are structurally correct but read 0%"
                .into(),
        );
    }

    let mut fields = vec![
        ("schema".to_string(), J::Str(schema_label.into())),
        ("session_id".to_string(), J::Str(session_id.as_hyphenated())),
        ("mode".to_string(), J::Str("counters".into())),
        ("state".to_string(), J::Str(state.as_str().into())),
        ("stop_reason".to_string(), J::Str(stop_reason.into())),
        ("samples".to_string(), J::UInt(snapshots.len() as u64)),
        ("target".to_string(), {
            let mut pairs = vec![
                ("identity".to_string(), identity_json(identity)),
                ("image".to_string(), J::Str(image_name.into())),
            ];
            if let Some(cpus) = logical_cpus {
                pairs.push(("logical_cpus".to_string(), J::UInt(u64::from(cpus))));
            }
            J::Object(pairs)
        }),
        ("metrics".to_string(), J::Object(metric_pairs)),
        (
            "polling".to_string(),
            J::Object(vec![
                ("poll_cost_p50_ms".to_string(), J::Float(poll_p50_ms)),
                ("poll_cost_max_ms".to_string(), J::Float(poll_max_ms)),
            ]),
        ),
        (
            "warnings".to_string(),
            J::Array(all_warnings.iter().map(|w| J::Str(w.clone())).collect()),
        ),
    ];
    if let Some(stacks) = stacks_summary {
        fields.push(("stacks".to_string(), stacks.clone()));
    }
    J::Object(fields)
}

/// Convenience wrapper over a recorder report.
pub fn from_recorder(
    r: &RecorderReport,
    logical_cpus: Option<u32>,
    stacks_summary: Option<&JsonValue>,
) -> JsonValue {
    build_report(ReportInput {
        schema_label: "pulseforge.report/0",
        session_id: &r.outcome.session_id,
        state: r.outcome.terminal_state(),
        stop_reason: r.outcome.reason.as_str(),
        identity: &r.target_identity,
        image_name: &r.image_name,
        snapshots: &r.snapshots,
        logical_cpus,
        warnings: &r.outcome.warnings,
        poll_p50_ms: r.poll_cost_p50_ms,
        poll_max_ms: r.poll_cost_max_ms,
        partial_read: None,
        stacks_summary,
    })
}

/// Extract a human line for `record` stdout (report JSON still goes to
/// `--out`/stdout per flags).
pub fn one_line_summary(report: &JsonValue) -> String {
    let cpu = report
        .get("metrics")
        .and_then(|m| m.get(MetricId::CpuCorePct.name()))
        .and_then(|c| c.get("mean"))
        .and_then(|v| v.as_f64());
    let rss = report
        .get("metrics")
        .and_then(|m| m.get(MetricId::ResidentBytes.name()))
        .and_then(|c| c.get("max"))
        .and_then(|v| v.as_f64());
    let state = report.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    let samples = report.get("samples").and_then(|v| v.as_u64()).unwrap_or(0);
    let cpu_txt = match cpu {
        Some(v) => format!("cpu={v:.1}%"),
        None => "cpu=unavailable".to_string(),
    };
    let rss_txt = match rss {
        Some(v) => format!("peak_ws={:.1}MiB", v / (1024.0 * 1024.0)),
        None => "peak_ws=unavailable".to_string(),
    };
    format!("{state} samples={samples} {cpu_txt} {rss_txt}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_types::{Os, SessionId};

    fn snap(mono: u64, user: u64, ws: u64) -> CounterSnapshot {
        CounterSnapshot {
            mono_ns: mono,
            cpu_user_ns: Some(user),
            cpu_kernel_ns: Some(0),
            resident_bytes: Some(ws),
            thread_count: Some(2),
            ..CounterSnapshot::default()
        }
    }

    #[test]
    fn report_contains_every_public_metric_with_units() {
        let id = SessionId::now_v7();
        let identity = ProcessIdentity::new(Os::Windows, 1, 2, "boot-t");
        let snaps = [snap(0, 0, 1024), snap(250_000_000, 250_000_000, 4096)];
        let report = build_report(ReportInput {
            schema_label: "pulseforge.report/0",
            session_id: &id,
            state: SessionState::Completed,
            stop_reason: "duration_reached",
            identity: &identity,
            image_name: "test.exe",
            snapshots: &snaps,
            logical_cpus: Some(8),
            warnings: &[],
            poll_p50_ms: 250.0,
            poll_max_ms: 260.0,
            partial_read: None,
            stacks_summary: None,
        });
        let metrics = report
            .get("metrics")
            .and_then(|m| m.as_object_pairs())
            .unwrap();
        assert_eq!(metrics.len(), REPORT_METRICS.len());
        for (name, value) in metrics {
            assert!(
                value.get("unit").and_then(|u| u.as_str()).is_some(),
                "{name} must carry a unit"
            );
            assert!(
                value.get("availability").and_then(|u| u.as_str()).is_some(),
                "{name} must carry availability"
            );
        }
        // 250ms CPU / 250ms wall == 100% core
        let cpu = metrics
            .iter()
            .find(|(k, _)| k == "process.cpu_core_pct")
            .map(|(_, v)| v)
            .unwrap();
        let mean = cpu.get("mean").and_then(|v| v.as_f64()).expect("mean");
        assert!((mean - 100.0).abs() < 1e-6);
        assert_eq!(
            report.get("mode").and_then(|v| v.as_str()),
            Some("counters")
        );
    }

    #[test]
    fn zero_cpu_environment_gets_honest_warning() {
        let id = SessionId::now_v7();
        let identity = ProcessIdentity::new(Os::Windows, 1, 2, "boot-t");
        // CPU accumulates zero delta between samples -> warning present.
        let snaps = [snap(0, 0, 1024), snap(250_000_000, 0, 4096)];
        let report = build_report(ReportInput {
            schema_label: "pulseforge.report/0",
            session_id: &id,
            state: SessionState::Completed,
            stop_reason: "duration_reached",
            identity: &identity,
            image_name: "test.exe",
            snapshots: &snaps,
            logical_cpus: Some(8),
            warnings: &[],
            poll_p50_ms: 250.0,
            poll_max_ms: 260.0,
            partial_read: None,
            stacks_summary: None,
        });
        let warnings = report.get("warnings").and_then(|w| w.as_array()).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("ENVIRONMENT")),
            "{warnings:?}"
        );
    }

    #[test]
    fn one_line_summary_renders_values() {
        let id = SessionId::now_v7();
        let identity = ProcessIdentity::new(Os::Windows, 1, 2, "boot-t");
        let snaps = [
            snap(0, 0, 1024),
            snap(250_000_000, 250_000_000, 2 * 1024 * 1024),
        ];
        let report = build_report(ReportInput {
            schema_label: "pulseforge.report/0",
            session_id: &id,
            state: SessionState::Completed,
            stop_reason: "duration_reached",
            identity: &identity,
            image_name: "t.exe",
            snapshots: &snaps,
            logical_cpus: Some(8),
            warnings: &[],
            poll_p50_ms: 250.0,
            poll_max_ms: 260.0,
            partial_read: None,
            stacks_summary: None,
        });
        let line = one_line_summary(&report);
        assert!(line.contains("COMPLETED"), "{line}");
        assert!(line.contains("cpu=100.0%"), "{line}");
        assert!(line.contains("peak_ws=2.0MiB"), "{line}");
    }
}
