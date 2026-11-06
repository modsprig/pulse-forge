//! Workload specifications and the repeated-run benchmark harness
//! (roadmap PF-1101/1102/1104/1108).
//!
//! A [`WorkloadSpec`] names an executable (launched directly — never a
//! shell), its argv, warm-up and run parameters, and an environment
//! allowlist. [`run_spec`] executes `repetitions` recorded runs and returns
//! per-run [`RunResult`]s; [`compare_repeated`] turns baseline/candidate
//! run sets into distribution statistics with the contract's honesty rules
//! (observed deltas; medians and IQR/MAD, never invented significance).

use pf_types::json::{self, JsonValue};
use pf_types::{CounterSnapshot, Os};

/// A named, repeatable workload (PF-1101).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkloadSpec {
    pub name: String,
    /// Executable path — launched directly via argv (no shell).
    pub exe: String,
    pub args: Vec<String>,
    /// Warm-up executions before measured runs (result discarded).
    pub warmup: u32,
    pub repetitions: u32,
    pub duration_ms: u64,
    pub interval_ms: u64,
    /// Environment variables allowed through to the child (PF-1102:
    /// privacy — nothing else is forwarded).
    pub env_allowlist: Vec<String>,
}

impl WorkloadSpec {
    /// Parse from a JSON document (the spec file format).
    pub fn from_json(v: &JsonValue) -> Result<WorkloadSpec, String> {
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or("spec needs `name`")?
            .to_string();
        let exe = v
            .get("exe")
            .and_then(|x| x.as_str())
            .ok_or("spec needs `exe`")?
            .to_string();
        let args = v
            .get("args")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .map(|s| s.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let warmup =
            u32::try_from(v.get("warmup").and_then(|x| x.as_u64()).unwrap_or(0)).unwrap_or(0);
        let repetitions =
            u32::try_from(v.get("repetitions").and_then(|x| x.as_u64()).unwrap_or(3)).unwrap_or(3);
        let duration_ms = v
            .get("duration_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(2_000);
        let interval_ms = v.get("interval_ms").and_then(|x| x.as_u64()).unwrap_or(250);
        let env_allowlist = v
            .get("env_allowlist")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let spec = WorkloadSpec {
            name,
            exe,
            args,
            warmup,
            repetitions,
            duration_ms,
            interval_ms,
            env_allowlist,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn parse(text: &str) -> Result<WorkloadSpec, String> {
        if text.len() > 64 * 1024 {
            return Err("spec exceeds 64 KiB".into());
        }
        let v = json::parse(text).map_err(|e| e.to_string())?;
        Self::from_json(&v)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if self.exe.trim().is_empty() {
            return Err("exe must not be empty".into());
        }
        if self.repetitions == 0 || self.repetitions > 100 {
            return Err("repetitions must be within 1..=100".into());
        }
        if self.warmup > 20 {
            return Err("warmup must be within 0..=20".into());
        }
        if !(1..=3_600_000).contains(&self.duration_ms) {
            return Err("duration_ms must be within 1..=3600000".into());
        }
        if !(100..=5_000).contains(&self.interval_ms) {
            return Err("interval_ms must be within 100..=5000".into());
        }
        Ok(())
    }

    /// Provenance record (PF-1102): what was run, under which environment
    /// policy — stored with the results, redacted of the full env.
    pub fn provenance(&self, os: Os, logical_cpus: Option<u32>, exe_hash: &str) -> JsonValue {
        use JsonValue as J;
        J::Object(vec![
            ("spec_name".into(), J::Str(self.name.clone())),
            ("exe".into(), J::Str(self.exe.clone())),
            (
                "args".into(),
                J::Array(self.args.iter().map(|a| J::Str(a.clone())).collect()),
            ),
            ("os".into(), J::Str(os.as_str().into())),
            (
                "logical_cpus".into(),
                match logical_cpus {
                    Some(n) => J::UInt(u64::from(n)),
                    None => J::Null,
                },
            ),
            ("exe_sha256_prefix".into(), J::Str(exe_hash.into())),
            (
                "env_policy".into(),
                J::Str(if self.env_allowlist.is_empty() {
                    "none forwarded".into()
                } else {
                    format!("allowlist: {}", self.env_allowlist.join(", "))
                }),
            ),
            ("repetitions".into(), J::UInt(u64::from(self.repetitions))),
        ])
    }
}

/// Result of one recorded run of a spec.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub index: u32,
    /// Wall duration of the recorded window (mono_ns span).
    pub wall_ns: u64,
    /// Cumulative CPU at the end of the run (None when unaccounted).
    pub cpu_total_ns: Option<u64>,
    pub peak_resident_bytes: Option<u64>,
    pub io_read_delta: Option<u64>,
    pub io_write_delta: Option<u64>,
    pub sample_count: usize,
}

impl RunResult {
    pub fn from_snapshots(index: u32, s: &[CounterSnapshot]) -> RunResult {
        let wall = s.last().map(|x| x.mono_ns).unwrap_or(0);
        let cpu = s.last().and_then(|x| {
            x.cpu_user_ns
                .zip(x.cpu_kernel_ns)
                .and_then(|(u, k)| u.checked_add(k))
        });
        let peak = s.iter().filter_map(|x| x.resident_bytes).max();
        let io = |pick: fn(&CounterSnapshot) -> Option<u64>| {
            let first = s.first().and_then(pick);
            let last = s.last().and_then(pick);
            match (first, last) {
                (Some(a), Some(b)) => b.checked_sub(a),
                _ => None,
            }
        };
        RunResult {
            index,
            wall_ns: wall,
            cpu_total_ns: cpu,
            peak_resident_bytes: peak,
            io_read_delta: io(|x| x.io_read_bytes),
            io_write_delta: io(|x| x.io_write_bytes),
            sample_count: s.len(),
        }
    }
}

/// Statistics over repeated runs of one metric — reuses
/// [`pf_analysis::distribution::Distribution`] and maps None (unavailable)
/// honestly.
pub struct RepeatedMetric {
    pub name: &'static str,
    pub unit: &'static str,
    /// Per-run values (None = unavailable on that run).
    pub values: Vec<Option<f64>>,
}

impl RepeatedMetric {
    pub fn distribution(&self) -> Result<Option<crate::distribution::Distribution>, String> {
        let present: Vec<f64> = self.values.iter().filter_map(|v| *v).collect();
        if present.is_empty() {
            return Ok(None);
        }
        crate::distribution::distribution_of(&present).map(Some)
    }

    pub fn available_runs(&self) -> usize {
        self.values.iter().filter(|v| v.is_some()).count()
    }
}

/// Compare two sets of repeated runs: per-metric baseline/candidate
/// distributions plus the observed median delta. No significance claims.
pub fn compare_repeated(
    baseline: &[RunResult],
    candidate: &[RunResult],
) -> Result<JsonValue, String> {
    use JsonValue as J;
    if baseline.is_empty() || candidate.is_empty() {
        return Err("both sides need at least one run".into());
    }
    let metric = |name: &'static str,
                  unit: &'static str,
                  pick: fn(&RunResult) -> Option<u64>|
     -> (String, JsonValue) {
        let a = RepeatedMetric {
            name,
            unit,
            values: baseline.iter().map(|r| pick(r).map(|v| v as f64)).collect(),
        };
        let b = RepeatedMetric {
            name,
            unit,
            values: candidate
                .iter()
                .map(|r| pick(r).map(|v| v as f64))
                .collect(),
        };
        let (da, db) = (a.distribution(), b.distribution());
        let render = |d: &Result<Option<crate::distribution::Distribution>, String>| match d {
            Ok(Some(d)) => d.to_json(unit),
            _ => J::Str("unavailable".into()),
        };
        let median_delta = match (
            da.as_ref().ok().and_then(|d| d.as_ref()),
            db.as_ref().ok().and_then(|d| d.as_ref()),
        ) {
            (Some(x), Some(y)) => J::Float(y.median - x.median),
            _ => J::Null,
        };
        (
            name.to_string(),
            J::Object(vec![
                ("unit".into(), J::Str(unit.into())),
                ("baseline".into(), render(&da)),
                ("candidate".into(), render(&db)),
                ("median_delta".into(), median_delta),
            ]),
        )
    };
    let metrics = vec![
        metric("wall_duration", "nanoseconds", |r| Some(r.wall_ns)),
        metric("cpu_total", "nanoseconds", |r| r.cpu_total_ns),
        metric("peak_resident", "bytes", |r| r.peak_resident_bytes),
        metric("io_read", "bytes", |r| r.io_read_delta),
        metric("io_write", "bytes", |r| r.io_write_delta),
    ];
    Ok(J::Object(vec![
        (
            "schema".into(),
            J::Str("pulseforge.compare-repeated/0".into()),
        ),
        (
            "runs".into(),
            J::Object(vec![
                ("baseline".into(), J::UInt(baseline.len() as u64)),
                ("candidate".into(), J::UInt(candidate.len() as u64)),
            ]),
        ),
        ("metrics".into(), J::Object(metrics)),
        (
            "caveats".into(),
            J::Array(vec![J::Str(
                "medians/IQR/MAD describe these runs only; this is not a statistical \
                 significance claim"
                    .into(),
            )]),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mono: u64, cpu: u64, ws: u64, r: u64, w: u64) -> CounterSnapshot {
        CounterSnapshot {
            mono_ns: mono,
            cpu_user_ns: Some(cpu),
            cpu_kernel_ns: Some(0),
            resident_bytes: Some(ws),
            io_read_bytes: Some(r),
            io_write_bytes: Some(w),
            ..CounterSnapshot::default()
        }
    }

    fn spec_json() -> &'static str {
        r#"{"name":"spin","exe":"C:/x/spin.exe","args":["--ms","2000"],"warmup":1,"repetitions":5,"duration_ms":2500,"interval_ms":250,"env_allowlist":["PATH"]}"#
    }

    #[test]
    fn spec_parses_validates_and_rejects() {
        let s = WorkloadSpec::parse(spec_json()).unwrap();
        assert_eq!(s.name, "spin");
        assert_eq!(s.repetitions, 5);
        assert_eq!(s.env_allowlist, vec!["PATH"]);
        assert!(s.validate().is_ok());

        // Boundaries.
        let mut bad = s.clone();
        bad.repetitions = 0;
        assert!(bad.validate().is_err());
        bad.repetitions = 101;
        assert!(bad.validate().is_err());
        bad.interval_ms = 50;
        assert!(bad.validate().is_err());
        bad.name = " ".into();
        assert!(bad.validate().is_err());
        // Malformed JSON.
        assert!(WorkloadSpec::parse("{not json").is_err());
        assert!(WorkloadSpec::parse(r#"{"name":"x"}"#).is_err()); // no exe
    }

    #[test]
    fn provenance_is_complete_and_redacted() {
        let s = WorkloadSpec::parse(spec_json()).unwrap();
        let p = s.provenance(Os::Windows, Some(16), "abc123");
        assert_eq!(p.get("spec_name").and_then(|v| v.as_str()), Some("spin"));
        assert_eq!(
            p.get("env_policy").and_then(|v| v.as_str()),
            Some("allowlist: PATH")
        );
        assert!(p.get("exe_sha256_prefix").is_some());
        // No full-environment leakage by construction (only the policy line).
    }

    #[test]
    fn run_results_extracts_metrics() {
        let s = vec![
            snap(0, 0, 4096, 0, 0),
            snap(1_000_000, 900_000, 8192, 5_000, 3_000),
        ];
        let r = RunResult::from_snapshots(0, &s);
        assert_eq!(r.wall_ns, 1_000_000);
        assert_eq!(r.cpu_total_ns, Some(900_000));
        assert_eq!(r.peak_resident_bytes, Some(8192));
        assert_eq!(r.io_read_delta, Some(5_000));
        assert_eq!(r.io_write_delta, Some(3_000));
    }

    #[test]
    fn repeated_comparison_reports_distributions_and_caveats() {
        let mk = |cpu: u64, n: u32| -> Vec<RunResult> {
            (0..n)
                .map(|i| {
                    RunResult::from_snapshots(
                        i,
                        &[
                            snap(0, 0, 4096, 0, 0),
                            snap(1_000_000, cpu + u64::from(i) * 10_000, 8192, 100, 100),
                        ],
                    )
                })
                .collect()
        };
        let out = compare_repeated(&mk(1_000_000, 5), &mk(900_000, 5)).unwrap();
        assert_eq!(
            out.get("runs")
                .and_then(|r| r.get("baseline"))
                .and_then(|v| v.as_u64()),
            Some(5)
        );
        let cpu = out
            .get("metrics")
            .and_then(|m| m.get("cpu_total"))
            .expect("cpu metric");
        let base_med = cpu
            .get("baseline")
            .and_then(|b| b.get("median"))
            .and_then(|v| v.as_f64())
            .expect("baseline distribution");
        let cand_med = cpu
            .get("candidate")
            .and_then(|b| b.get("median"))
            .and_then(|v| v.as_f64())
            .expect("candidate distribution");
        assert!(cand_med < base_med, "candidate ran less CPU");
        // Caveats present.
        let caveats = out.get("caveats").and_then(|c| c.as_array()).unwrap();
        assert!(caveats.iter().any(|c| {
            c.as_str()
                .unwrap_or("")
                .contains("not a statistical significance claim")
        }));
    }

    #[test]
    fn unavailable_metrics_stay_unavailable() {
        // Zero-CPU environment (sandbox): cpu_total Some(0) is present, but
        // a run with None stays honestly "unavailable".
        let mut no_cpu = snap(0, 0, 4096, 0, 0);
        no_cpu.cpu_user_ns = None;
        no_cpu.cpu_kernel_ns = None;
        let runs = vec![RunResult::from_snapshots(0, &[no_cpu])];
        let out = compare_repeated(&runs, &runs).unwrap();
        let cpu = out.get("metrics").and_then(|m| m.get("cpu_total")).unwrap();
        assert_eq!(
            cpu.get("baseline").and_then(|b| b.as_str()),
            Some("unavailable")
        );
        assert!(cpu.get("median_delta").and_then(|v| v.as_f64()).is_none());
    }

    #[test]
    fn empty_sides_are_rejected() {
        let r = vec![RunResult::from_snapshots(0, &[snap(0, 0, 1, 0, 0)])];
        assert!(compare_repeated(&[], &r).is_err());
        assert!(compare_repeated(&r, &[]).is_err());
    }
}
