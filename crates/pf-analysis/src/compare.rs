//! Observed-delta A/B comparison of two recorded sessions (roadmap
//! PF-1103).
//!
//! Rules enforced here:
//! - a single pair reports **observed deltas** with a caveat that one pair
//!   proves nothing statistically;
//! - scope compatibility is checked first (OS, interval class, CPU count);
//!   incompatible comparisons carry a `comparable: false` flag and require
//!   explicit opt-in, and the report says so;
//! - CPU **share** would be compared separately from CPU **time** — here we
//!   compare cumulative CPU time and wall duration, never a share that
//!   could fall while the program got slower.

use pf_types::json::JsonValue;
use pf_types::{CounterSnapshot, Os};

/// Facts extracted from one finished session.
#[derive(Clone, Debug)]
pub struct SessionFacts {
    pub label: String,
    pub os: Os,
    pub interval_ms: u64,
    pub logical_cpus: Option<u32>,
    pub wall_duration_ns: u64,
    /// Cumulative user+kernel CPU at the last sample (None when the OS
    /// never reported CPU time — compared as "unavailable", never zero).
    pub cpu_total_ns: Option<u64>,
    pub peak_resident_bytes: Option<u64>,
    pub peak_private_commit_bytes: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub sample_count: usize,
    pub warnings: Vec<String>,
}

impl SessionFacts {
    pub fn from_snapshots(
        label: &str,
        os: Os,
        interval_ms: u64,
        logical_cpus: Option<u32>,
        snapshots: &[CounterSnapshot],
        warnings: Vec<String>,
    ) -> SessionFacts {
        let wall = snapshots.last().map(|s| s.mono_ns).unwrap_or(0);
        let cpu_total = snapshots.last().and_then(|s| {
            s.cpu_user_ns
                .zip(s.cpu_kernel_ns)
                .and_then(|(u, k)| u.checked_add(k))
        });
        // Working-set peak: max of the observed gauge (the OS peak field is
        // a lifetime high-water mark that predates the recording window).
        let peak_res = snapshots.iter().filter_map(|s| s.resident_bytes).max();
        let peak_commit = snapshots
            .iter()
            .filter_map(|s| s.private_commit_bytes)
            .max();
        let first_read = snapshots.first().and_then(|s| s.io_read_bytes);
        let last_read = snapshots.last().and_then(|s| s.io_read_bytes);
        let io_read = match (first_read, last_read) {
            (Some(a), Some(b)) => b.checked_sub(a),
            _ => None,
        };
        let first_write = snapshots.first().and_then(|s| s.io_write_bytes);
        let last_write = snapshots.last().and_then(|s| s.io_write_bytes);
        let io_write = match (first_write, last_write) {
            (Some(a), Some(b)) => b.checked_sub(a),
            _ => None,
        };
        SessionFacts {
            label: label.to_string(),
            os,
            interval_ms,
            logical_cpus,
            wall_duration_ns: wall,
            cpu_total_ns: cpu_total,
            peak_resident_bytes: peak_res,
            peak_private_commit_bytes: peak_commit,
            io_read_bytes: io_read,
            io_write_bytes: io_write,
            sample_count: snapshots.len(),
            warnings,
        }
    }
}

/// One metric's observed delta.
#[derive(Clone, Debug)]
pub struct MetricDelta {
    pub name: &'static str,
    pub unit: &'static str,
    pub a: Option<f64>,
    pub b: Option<f64>,
    /// b − a (None when either side is unavailable).
    pub delta: Option<f64>,
    /// (b − a) / a when a > 0.
    pub relative: Option<f64>,
}

impl MetricDelta {
    fn to_json(&self) -> JsonValue {
        use pf_types::json::JsonValue as J;
        let opt = |v: Option<f64>| v.map(J::Float).unwrap_or(J::Null);
        J::Object(vec![
            ("unit".into(), J::Str(self.unit.into())),
            ("baseline".into(), opt(self.a)),
            ("candidate".into(), opt(self.b)),
            ("delta".into(), opt(self.delta)),
            ("relative".into(), opt(self.relative)),
            (
                "availability".into(),
                J::Str(
                    if self.a.is_some() && self.b.is_some() {
                        "available"
                    } else {
                        "unavailable"
                    }
                    .into(),
                ),
            ),
        ])
    }
}

/// Inputs to [`compare_sessions`].
pub struct ComparisonInput {
    pub baseline: SessionFacts,
    pub candidate: SessionFacts,
    /// User explicitly accepted comparing incompatible scopes.
    pub force_incompatible: bool,
}

/// Full comparison result.
pub struct Comparison {
    pub comparable: bool,
    pub compatibility_warnings: Vec<String>,
    pub metrics: Vec<MetricDelta>,
    pub caveats: Vec<String>,
}

fn delta(name: &'static str, unit: &'static str, a: Option<u64>, b: Option<u64>) -> MetricDelta {
    let (af, bf) = (a.map(|v| v as f64), b.map(|v| v as f64));
    let delta = match (af, bf) {
        (Some(x), Some(y)) => Some(y - x),
        _ => None,
    };
    let relative = match (af, delta) {
        (Some(x), Some(d)) if x > 0.0 => Some(d / x),
        _ => None,
    };
    MetricDelta {
        name,
        unit,
        a: af,
        b: bf,
        delta,
        relative,
    }
}

/// Compare two sessions: observed deltas + compatibility + caveats.
pub fn compare_sessions(input: &ComparisonInput) -> Comparison {
    let a = &input.baseline;
    let b = &input.candidate;
    let mut warnings = Vec::new();

    if a.os != b.os {
        warnings.push(format!(
            "OS differs ({} vs {}): memory counters are not byte-equivalent across OSes",
            a.os, b.os
        ));
    }
    if a.interval_ms != b.interval_ms {
        warnings.push(format!(
            "sampling interval differs ({} ms vs {} ms): rate-derived numbers are not directly comparable",
            a.interval_ms, b.interval_ms
        ));
    }
    if a.logical_cpus.is_some() && b.logical_cpus.is_some() && a.logical_cpus != b.logical_cpus {
        warnings.push(format!(
            "logical CPU count differs ({:?} vs {:?})",
            a.logical_cpus, b.logical_cpus
        ));
    }
    let comparable = warnings.is_empty();
    if !comparable {
        warnings.push(if input.force_incompatible {
            "scope incompatibility acknowledged by the caller; deltas below are still OBSERVED ONLY"
                .into()
        } else {
            "pass --force to compare anyway; the result is explicitly non-comparable".into()
        });
    }

    let mut metrics = vec![
        delta(
            "wall_duration",
            "nanoseconds",
            Some(a.wall_duration_ns),
            Some(b.wall_duration_ns),
        ),
        delta("cpu_total", "nanoseconds", a.cpu_total_ns, b.cpu_total_ns),
        delta(
            "peak_resident",
            "bytes",
            a.peak_resident_bytes,
            b.peak_resident_bytes,
        ),
        delta(
            "peak_private_commit",
            "bytes",
            a.peak_private_commit_bytes,
            b.peak_private_commit_bytes,
        ),
        delta("io_read", "bytes", a.io_read_bytes, b.io_read_bytes),
        delta("io_write", "bytes", a.io_write_bytes, b.io_write_bytes),
    ];
    if a.cpu_total_ns.is_none() || b.cpu_total_ns.is_none() {
        // Keep CPU metrics in the report but mark unavailable — the honest
        // state for environments that do not account CPU time.
        metrics[1].a = a.cpu_total_ns.map(|v| v as f64);
        metrics[1].b = b.cpu_total_ns.map(|v| v as f64);
    }

    let mut caveats = vec![
        "single A/B pair: deltas are OBSERVED ONLY, not statistically significant; \
         repeat both sides and compare distributions"
            .into(),
    ];
    if a.cpu_total_ns == Some(0) || b.cpu_total_ns == Some(0) {
        caveats.push(
            "one side reports zero CPU time (sandboxed accounting); treat CPU deltas as absent"
                .into(),
        );
    }
    caveats.push(
        "a function's CPU *share* can fall while total CPU *time* rises; \
         never read a share delta as a speedup"
            .into(),
    );

    Comparison {
        comparable,
        compatibility_warnings: warnings,
        metrics,
        caveats,
    }
}

impl Comparison {
    pub fn to_json(&self) -> JsonValue {
        use pf_types::json::JsonValue as J;
        J::Object(vec![
            ("schema".into(), J::Str("pulseforge.compare/0".into())),
            ("comparable".into(), J::Bool(self.comparable)),
            (
                "compatibility_warnings".into(),
                J::Array(
                    self.compatibility_warnings
                        .iter()
                        .map(|w| J::Str(w.clone()))
                        .collect(),
                ),
            ),
            (
                "metrics".into(),
                J::Object(
                    self.metrics
                        .iter()
                        .map(|m| (m.name.to_string(), m.to_json()))
                        .collect(),
                ),
            ),
            (
                "caveats".into(),
                J::Array(self.caveats.iter().map(|c| J::Str(c.clone())).collect()),
            ),
        ])
    }

    /// Human-facing Markdown table (PF-1107), caveats included.
    pub fn to_markdown(&self, baseline_label: &str, candidate_label: &str) -> String {
        let mut out = String::new();
        out.push_str("# PulseForge A/B comparison\n\n");
        out.push_str(&format!(
            "baseline: `{baseline_label}` · candidate: `{candidate_label}`\n\n"
        ));
        if !self.comparable {
            out.push_str("> **Not comparable** — scope differences:\n");
            for w in &self.compatibility_warnings {
                out.push_str(&format!("> - {w}\n"));
            }
            out.push('\n');
        }
        out.push_str("| metric | unit | baseline | candidate | delta | relative |\n");
        out.push_str("|---|---|---:|---:|---:|---:|\n");
        for m in &self.metrics {
            let fmt = |v: Option<f64>| {
                v.map(|x| format!("{x:.3}"))
                    .unwrap_or_else(|| "unavailable".into())
            };
            let rel = m
                .relative
                .map(|r| format!("{:+.1}%", r * 100.0))
                .unwrap_or_else(|| "—".into());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                m.name,
                m.unit,
                fmt(m.a),
                fmt(m.b),
                fmt(m.delta),
                rel
            ));
        }
        out.push_str("\n## Caveats\n\n");
        for c in &self.caveats {
            out.push_str(&format!("- {c}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mono: u64, cpu: u64, ws: u64, io: u64) -> CounterSnapshot {
        CounterSnapshot {
            mono_ns: mono,
            cpu_user_ns: Some(cpu),
            cpu_kernel_ns: Some(0),
            resident_bytes: Some(ws),
            private_commit_bytes: Some(ws / 2),
            io_read_bytes: Some(io),
            ..CounterSnapshot::default()
        }
    }

    fn facts(label: &str, snaps: &[CounterSnapshot]) -> SessionFacts {
        SessionFacts::from_snapshots(label, Os::Windows, 250, Some(16), snaps, vec![])
    }

    #[test]
    fn observed_deltas_are_exact_and_labeled() {
        let a = facts(
            "a",
            &[snap(0, 0, 4096, 0), snap(1_000_000, 500_000, 8192, 1_000)],
        );
        let b = facts(
            "b",
            &[snap(0, 0, 4096, 0), snap(900_000, 400_000, 8192, 800)],
        );
        let cmp = compare_sessions(&ComparisonInput {
            baseline: a,
            candidate: b,
            force_incompatible: false,
        });
        assert!(cmp.comparable);
        let wall = cmp
            .metrics
            .iter()
            .find(|m| m.name == "wall_duration")
            .unwrap();
        assert_eq!(wall.delta, Some(-100_000.0));
        let cpu = cmp.metrics.iter().find(|m| m.name == "cpu_total").unwrap();
        assert_eq!(cpu.delta, Some(-100_000.0));
        assert_eq!(cpu.relative, Some(-0.2));
        // Caveats must include the no-significance warning.
        assert!(cmp.caveats[0].contains("OBSERVED ONLY"));
    }

    #[test]
    fn unavailable_metrics_are_null_never_zero() {
        let a = SessionFacts::from_snapshots(
            "a",
            Os::Windows,
            250,
            Some(16),
            &[CounterSnapshot {
                mono_ns: 10,
                ..CounterSnapshot::default()
            }],
            vec![],
        );
        let b = facts("b", &[snap(0, 0, 4096, 0), snap(1_000, 100, 4096, 0)]);
        let cmp = compare_sessions(&ComparisonInput {
            baseline: a,
            candidate: b,
            force_incompatible: false,
        });
        let cpu = cmp.metrics.iter().find(|m| m.name == "cpu_total").unwrap();
        assert!(cpu.a.is_none());
        assert!(cpu.delta.is_none(), "delta must not be invented");
        let json = cmp.to_json();
        let metrics = json
            .get("metrics")
            .and_then(|m| m.as_object_pairs())
            .unwrap();
        let cpu_json = metrics
            .iter()
            .find(|(k, _)| k == "cpu_total")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(
            cpu_json.get("availability").and_then(|v| v.as_str()),
            Some("unavailable")
        );
    }

    #[test]
    fn incompatible_scope_is_flagged_and_needs_opt_in() {
        let a = SessionFacts::from_snapshots(
            "a",
            Os::Windows,
            250,
            Some(16),
            &[snap(1, 1, 1, 1)],
            vec![],
        );
        let b_snap = snap(1, 1, 1, 1);
        let b = SessionFacts::from_snapshots("b", Os::Linux, 250, Some(16), &[b_snap], vec![]);
        let b2 = SessionFacts::from_snapshots(
            "b",
            Os::Linux,
            250,
            Some(16),
            &[snap(1, 1, 1, 1)],
            vec![],
        );
        let cmp = compare_sessions(&ComparisonInput {
            baseline: a,
            candidate: b,
            force_incompatible: false,
        });
        assert!(!cmp.comparable);
        assert!(
            cmp.compatibility_warnings
                .iter()
                .any(|w| w.contains("OS differs"))
        );
        assert!(
            cmp.compatibility_warnings
                .iter()
                .any(|w| w.contains("--force"))
        );
        // With opt-in the comparison still runs, still flagged.
        let cmp = compare_sessions(&ComparisonInput {
            baseline: SessionFacts::from_snapshots(
                "a",
                Os::Windows,
                250,
                Some(16),
                &[snap(1, 1, 1, 1)],
                vec![],
            ),
            candidate: b2,
            force_incompatible: true,
        });
        assert!(!cmp.comparable);
        assert!(
            cmp.compatibility_warnings
                .iter()
                .any(|w| w.contains("acknowledged"))
        );
    }

    #[test]
    fn markdown_renders_table_and_caveats() {
        let a = facts(
            "a",
            &[snap(0, 0, 4096, 0), snap(1_000_000, 500_000, 8192, 1_000)],
        );
        let b = facts(
            "b",
            &[snap(0, 0, 4096, 0), snap(900_000, 400_000, 8192, 800)],
        );
        let cmp = compare_sessions(&ComparisonInput {
            baseline: a,
            candidate: b,
            force_incompatible: false,
        });
        let md = cmp.to_markdown("run-a", "run-b");
        assert!(md.contains("| metric |"), "{md}");
        assert!(md.contains("wall_duration"));
        assert!(md.contains("## Caveats"));
        assert!(md.contains("OBSERVED ONLY"));
    }
}
