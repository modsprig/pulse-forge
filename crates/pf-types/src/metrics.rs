//! Metric taxonomy, units, availability semantics and rate derivation.
//!
//! ## Contract (architecture §5.3, roadmap PF-106)
//!
//! - Raw counters are sampled as exact integers; only derived rates are
//!   floats.
//! - `process.cpu_core_pct` = ΔCPU time / Δmonotonic time × 100, where 100 %
//!   means **one logical core** (a multithreaded process can exceed 100 %).
//! - The first sample of any derivative is *unavailable*, never zero.
//! - Counter resets/decreases, clock gaps and process exit produce explicit
//!   `unavailable` samples — the pipeline never interpolates through a gap.
//! - Windows "working set" and Linux "RSS" keep their native names; logical
//!   I/O and storage I/O are never conflated.

use crate::json::JsonValue;

/// Canonical metric identity. Names are a public contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetricId {
    /// CPU time delta / wall-time delta × 100. 100 = one logical core.
    CpuCorePct,
    /// CPU time delta / wall-time delta × 100 / logical CPU count.
    CpuMachinePct,
    /// Cumulative user-mode CPU time (OS accounting).
    CpuUserNs,
    /// Cumulative kernel-mode CPU time (OS accounting).
    CpuKernelNs,
    /// Cumulative user+kernel CPU time. Preferred for before/after totals.
    CpuTotalNs,
    /// Windows working set / Linux RSS. Native meaning; not equated.
    ResidentBytes,
    /// Peak working set (Windows) / high-water RSS where available.
    ResidentPeakBytes,
    /// Windows PrivateUsage (private commit). Linux: unavailable by default.
    PrivateCommitBytes,
    /// Peak private commit (Windows).
    PrivateCommitPeakBytes,
    /// Read operations completed (OS logical I/O; Windows includes more than
    /// storage — documented caveat).
    IoReadOps,
    IoWriteOps,
    /// Bytes transferred through logical read/write (not physical disk I/O).
    IoReadBytes,
    IoWriteBytes,
    /// Observed live thread count (may miss very short-lived threads).
    ThreadCount,
    /// Open handle count (Windows).
    HandleCount,
    /// OS-defined page-fault count (aggregate; Windows ≠ Linux definitions).
    PageFaults,
}

impl MetricId {
    /// Canonical dotted name, stable across CLI/GUI/MCP/storage.
    pub fn name(self) -> &'static str {
        match self {
            MetricId::CpuCorePct => "process.cpu_core_pct",
            MetricId::CpuMachinePct => "process.cpu_machine_pct",
            MetricId::CpuUserNs => "process.cpu_user_ns",
            MetricId::CpuKernelNs => "process.cpu_kernel_ns",
            MetricId::CpuTotalNs => "process.cpu_total_ns",
            MetricId::ResidentBytes => "process.resident_bytes",
            MetricId::ResidentPeakBytes => "process.resident_peak_bytes",
            MetricId::PrivateCommitBytes => "process.private_commit_bytes",
            MetricId::PrivateCommitPeakBytes => "process.private_commit_peak_bytes",
            MetricId::IoReadOps => "process.io_read_ops",
            MetricId::IoWriteOps => "process.io_write_ops",
            MetricId::IoReadBytes => "process.io_transfer_read_bytes",
            MetricId::IoWriteBytes => "process.io_transfer_write_bytes",
            MetricId::ThreadCount => "process.thread_count",
            MetricId::HandleCount => "process.handle_count",
            MetricId::PageFaults => "process.page_faults",
        }
    }

    pub fn unit(self) -> Unit {
        match self {
            MetricId::CpuCorePct | MetricId::CpuMachinePct => Unit::Percent,
            MetricId::CpuUserNs | MetricId::CpuKernelNs | MetricId::CpuTotalNs => Unit::Nanoseconds,
            MetricId::ResidentBytes
            | MetricId::ResidentPeakBytes
            | MetricId::PrivateCommitBytes
            | MetricId::PrivateCommitPeakBytes
            | MetricId::IoReadBytes
            | MetricId::IoWriteBytes => Unit::Bytes,
            MetricId::IoReadOps
            | MetricId::IoWriteOps
            | MetricId::ThreadCount
            | MetricId::HandleCount
            | MetricId::PageFaults => Unit::Count,
        }
    }

    /// True when the value is an exact cumulative integer counter.
    pub fn is_cumulative(self) -> bool {
        matches!(
            self,
            MetricId::CpuUserNs
                | MetricId::CpuKernelNs
                | MetricId::CpuTotalNs
                | MetricId::IoReadOps
                | MetricId::IoWriteOps
                | MetricId::IoReadBytes
                | MetricId::IoWriteBytes
                | MetricId::PageFaults
        )
    }
}

/// Units with exact display names (never bare "units").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    Percent,
    Bytes,
    Count,
    Nanoseconds,
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Percent => "percent",
            Unit::Bytes => "bytes",
            Unit::Count => "count",
            Unit::Nanoseconds => "nanoseconds",
        }
    }
}

/// Why a metric value is missing at this sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnavailableCode {
    /// First sample of a derivative: no baseline to difference against.
    FirstSample,
    /// Counter decreased/reset or monotonic clock went backwards: gap.
    CounterResetOrGap,
    /// The OS denied this specific counter for this target.
    PermissionDenied,
    /// The backend does not expose this metric on this platform.
    NotCollected,
}

impl UnavailableCode {
    pub fn as_str(self) -> &'static str {
        match self {
            UnavailableCode::FirstSample => "first_sample_derivative",
            UnavailableCode::CounterResetOrGap => "counter_reset_or_gap",
            UnavailableCode::PermissionDenied => "permission_denied",
            UnavailableCode::NotCollected => "not_collected",
        }
    }
}

/// Per-sample metric status: a value, or a structured unavailability.
#[derive(Clone, Debug, PartialEq)]
pub enum Availability {
    Available(f64),
    Unavailable {
        code: UnavailableCode,
        detail: String,
    },
}

impl Availability {
    pub fn unavailable(code: UnavailableCode, detail: impl Into<String>) -> Self {
        Availability::Unavailable {
            code,
            detail: detail.into(),
        }
    }
}

/// One raw counter snapshot for a single process, taken atomically enough by
/// the backend. All integers are exact OS counter values; `mono_ns` is the
/// session-clock reading at acquisition (not wall time).
///
/// Backends that cannot provide a field use `None`; derivation surfaces that
/// as `not_collected` rather than zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CounterSnapshot {
    pub mono_ns: u64,
    pub cpu_user_ns: Option<u64>,
    pub cpu_kernel_ns: Option<u64>,
    pub resident_bytes: Option<u64>,
    pub resident_peak_bytes: Option<u64>,
    pub private_commit_bytes: Option<u64>,
    pub private_commit_peak_bytes: Option<u64>,
    pub io_read_ops: Option<u64>,
    pub io_write_ops: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub thread_count: Option<u32>,
    pub handle_count: Option<u32>,
    pub page_faults: Option<u64>,
}

impl CounterSnapshot {
    /// Serialize to JSON for schema-0 session files.
    pub fn to_json(&self) -> JsonValue {
        use JsonValue as J;
        let opt_u64 = |v: &Option<u64>| match v {
            Some(x) => J::UInt(*x),
            None => J::Null,
        };
        let opt_u32 = |v: &Option<u32>| match v {
            Some(x) => J::UInt(u64::from(*x)),
            None => J::Null,
        };
        J::Object(vec![
            ("mono_ns".into(), J::UInt(self.mono_ns)),
            ("cpu_user_ns".into(), opt_u64(&self.cpu_user_ns)),
            ("cpu_kernel_ns".into(), opt_u64(&self.cpu_kernel_ns)),
            ("resident_bytes".into(), opt_u64(&self.resident_bytes)),
            (
                "resident_peak_bytes".into(),
                opt_u64(&self.resident_peak_bytes),
            ),
            (
                "private_commit_bytes".into(),
                opt_u64(&self.private_commit_bytes),
            ),
            (
                "private_commit_peak_bytes".into(),
                opt_u64(&self.private_commit_peak_bytes),
            ),
            ("io_read_ops".into(), opt_u64(&self.io_read_ops)),
            ("io_write_ops".into(), opt_u64(&self.io_write_ops)),
            ("io_read_bytes".into(), opt_u64(&self.io_read_bytes)),
            ("io_write_bytes".into(), opt_u64(&self.io_write_bytes)),
            ("thread_count".into(), opt_u32(&self.thread_count)),
            ("handle_count".into(), opt_u32(&self.handle_count)),
            ("page_faults".into(), opt_u64(&self.page_faults)),
        ])
    }

    /// Parse from schema-0 JSON. Unknown fields are ignored (forward
    /// compatible); wrong types are rejected, not coerced.
    pub fn from_json(v: &JsonValue) -> Result<Self, String> {
        let mut s = CounterSnapshot::default();
        let Some(obj) = v.as_object_pairs() else {
            return Err("snapshot must be a JSON object".into());
        };
        let get_u64 = |key: &str| -> Result<Option<u64>, String> {
            match obj.iter().find(|(k, _)| k == key) {
                None => Ok(None),
                Some((_, JsonValue::Null)) => Ok(None),
                Some((_, value)) => value
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| format!("field `{key}` must be a non-negative integer")),
            }
        };
        let get_u32 = |key: &str| -> Result<Option<u32>, String> {
            get_u64(key)?
                .map(|v| u32::try_from(v).map_err(|_| format!("field `{key}` exceeds u32")))
                .transpose()
        };
        s.mono_ns = get_u64("mono_ns")?.unwrap_or(0);
        s.cpu_user_ns = get_u64("cpu_user_ns")?;
        s.cpu_kernel_ns = get_u64("cpu_kernel_ns")?;
        s.resident_bytes = get_u64("resident_bytes")?;
        s.resident_peak_bytes = get_u64("resident_peak_bytes")?;
        s.private_commit_bytes = get_u64("private_commit_bytes")?;
        s.private_commit_peak_bytes = get_u64("private_commit_peak_bytes")?;
        s.io_read_ops = get_u64("io_read_ops")?;
        s.io_write_ops = get_u64("io_write_ops")?;
        s.io_read_bytes = get_u64("io_read_bytes")?;
        s.io_write_bytes = get_u64("io_write_bytes")?;
        s.thread_count = get_u32("thread_count")?;
        s.handle_count = get_u32("handle_count")?;
        s.page_faults = get_u64("page_faults")?;
        Ok(s)
    }
}

/// One derived metric within a [`DerivedSample`].
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedMetric {
    pub id: MetricId,
    pub availability: Availability,
}

/// A derived sample: raw snapshot metrics mapped onto the public taxonomy,
/// with explicit availability for everything missing.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedSample {
    pub mono_ns: u64,
    pub metrics: Vec<DerivedMetric>,
}

fn pair_opt(id: MetricId, v: Option<u64>) -> Option<(MetricId, Availability)> {
    v.map(|x| (id, Availability::Available(x as f64)))
}

/// Derive public metrics for `current`, given the previous snapshot (if any).
///
/// Guarantees:
/// - `cpu_core_pct`/`cpu_machine_pct` are `FirstSample`-unavailable on the
///   first sample and `CounterResetOrGap`-unavailable if any input counter
///   decreased or the monotonic clock did not advance.
/// - Gauges (resident bytes, thread count, …) pass through directly.
/// - Absent counters surface as `not_collected`, never as zero.
///
/// `logical_cpus` is used only for `cpu_machine_pct`; `None` leaves that
/// metric `not_collected` (annotated elsewhere with the reason).
pub fn derive_one(
    prev: Option<&CounterSnapshot>,
    current: &CounterSnapshot,
    logical_cpus: Option<u32>,
) -> DerivedSample {
    let mut metrics = Vec::with_capacity(16);
    let cpu_total =
        |s: &CounterSnapshot| -> Option<u64> { s.cpu_user_ns?.checked_add(s.cpu_kernel_ns?) };

    // --- derivative rates ---
    let core_pct = match prev {
        None => Availability::unavailable(
            UnavailableCode::FirstSample,
            "cpu rate needs a previous sample",
        ),
        Some(p) => {
            match (cpu_total(p), cpu_total(current)) {
                (None, _) | (_, None) => Availability::unavailable(
                    UnavailableCode::NotCollected,
                    "cpu time counters not present",
                ),
                (Some(a), Some(b)) => {
                    let dt = current.mono_ns.checked_sub(p.mono_ns).filter(|dt| *dt > 0);
                    // A decreasing cumulative counter means reset/reuse: a
                    // gap, not a negative rate.
                    match (dt, b.checked_sub(a)) {
                        (Some(dt), Some(dcpu)) => {
                            Availability::Available(dcpu as f64 / dt as f64 * 100.0)
                        }
                        (Some(_), None) => Availability::unavailable(
                            UnavailableCode::CounterResetOrGap,
                            "cumulative CPU counter decreased or reset",
                        ),
                        (None, _) => Availability::unavailable(
                            UnavailableCode::CounterResetOrGap,
                            "monotonic clock did not advance",
                        ),
                    }
                }
            }
        }
    };
    let machine_pct = match &core_pct {
        Availability::Available(v) => match logical_cpus {
            Some(cpus @ 1..) => Availability::Available(*v / f64::from(cpus)),
            Some(0) => Availability::unavailable(
                UnavailableCode::NotCollected,
                "logical CPU count reported as zero",
            ),
            None => Availability::unavailable(
                UnavailableCode::NotCollected,
                "logical CPU count unknown",
            ),
        },
        // Machine percent is unavailable for exactly the reason core percent
        // is; normalization never invents a value.
        other => other.clone(),
    };
    metrics.push(DerivedMetric {
        id: MetricId::CpuCorePct,
        availability: core_pct,
    });
    metrics.push(DerivedMetric {
        id: MetricId::CpuMachinePct,
        availability: machine_pct,
    });

    // --- cumulative counters (exact pass-through) ---
    for (id, field) in [
        (MetricId::CpuUserNs, current.cpu_user_ns),
        (MetricId::CpuKernelNs, current.cpu_kernel_ns),
        (MetricId::IoReadOps, current.io_read_ops),
        (MetricId::IoWriteOps, current.io_write_ops),
        (MetricId::IoReadBytes, current.io_read_bytes),
        (MetricId::IoWriteBytes, current.io_write_bytes),
        (MetricId::PageFaults, current.page_faults),
    ] {
        metrics.push(DerivedMetric {
            id,
            availability: match field {
                Some(v) => Availability::Available(v as f64),
                None => Availability::unavailable(UnavailableCode::NotCollected, "counter absent"),
            },
        });
    }
    if let Some((id, a)) = pair_opt(MetricId::CpuTotalNs, cpu_total(current)) {
        metrics.push(DerivedMetric {
            id,
            availability: a,
        });
    } else {
        metrics.push(DerivedMetric {
            id: MetricId::CpuTotalNs,
            availability: Availability::unavailable(
                UnavailableCode::NotCollected,
                "counter absent",
            ),
        });
    }

    // --- gauges ---
    for (id, field) in [
        (MetricId::ResidentBytes, current.resident_bytes),
        (MetricId::ResidentPeakBytes, current.resident_peak_bytes),
        (MetricId::PrivateCommitBytes, current.private_commit_bytes),
        (
            MetricId::PrivateCommitPeakBytes,
            current.private_commit_peak_bytes,
        ),
    ] {
        if let Some((id, a)) = pair_opt(id, field) {
            metrics.push(DerivedMetric {
                id,
                availability: a,
            });
        } else {
            metrics.push(DerivedMetric {
                id,
                availability: Availability::unavailable(
                    UnavailableCode::NotCollected,
                    "counter absent",
                ),
            });
        }
    }
    let gauge_u32 = |id, field: Option<u32>| -> DerivedMetric {
        match field {
            Some(v) => DerivedMetric {
                id,
                availability: Availability::Available(f64::from(v)),
            },
            None => DerivedMetric {
                id,
                availability: Availability::unavailable(
                    UnavailableCode::NotCollected,
                    "counter absent",
                ),
            },
        }
    };
    metrics.push(gauge_u32(MetricId::ThreadCount, current.thread_count));
    metrics.push(gauge_u32(MetricId::HandleCount, current.handle_count));

    DerivedSample {
        mono_ns: current.mono_ns,
        metrics,
    }
}

/// Derive a full series. `logical_cpus` as in [`derive_one`].
pub fn derive_samples(
    snapshots: &[CounterSnapshot],
    logical_cpus: Option<u32>,
) -> Vec<DerivedSample> {
    let mut prev: Option<&CounterSnapshot> = None;
    snapshots
        .iter()
        .map(|cur| {
            let out = derive_one(prev, cur, logical_cpus);
            prev = Some(cur);
            out
        })
        .collect()
}

/// Aggregate statistics for one metric across a derived series.
#[derive(Clone, Debug)]
pub struct MetricSummary {
    pub id: MetricId,
    pub unit: Unit,
    pub availability: &'static str,
    pub samples: usize,
    pub available_samples: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub first: Option<f64>,
    pub last: Option<f64>,
    pub sum: f64,
    /// Mean of available samples (NaN only when none available; callers
    /// report availability instead of the number).
    pub mean: f64,
}

impl MetricSummary {
    // Counters here are bounded by the series length; usize overflow would
    // require an impossibly large input.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn summarize(id: MetricId, series: &[DerivedSample]) -> Self {
        let mut s = MetricSummary {
            id,
            unit: id.unit(),
            availability: "",
            samples: 0,
            available_samples: 0,
            min: None,
            max: None,
            first: None,
            last: None,
            sum: 0.0,
            mean: f64::NAN,
        };
        for sample in series {
            if let Some(m) = sample.metrics.iter().find(|m| m.id == id) {
                s.samples += 1;
                match m.availability {
                    Availability::Available(v) => {
                        s.available_samples += 1;
                        s.sum += v;
                        s.min = s.min.map_or(Some(v), |x| Some(x.min(v)));
                        s.max = s.max.map_or(Some(v), |x| Some(x.max(v)));
                        if s.first.is_none() {
                            s.first = Some(v);
                        }
                        s.last = Some(v);
                    }
                    Availability::Unavailable { code, .. } => {
                        // Keep the *latest* reason: it is the most
                        // representative for the end state of the series.
                        s.availability = code.as_str();
                    }
                }
            }
        }
        if s.available_samples > 0 {
            s.mean = s.sum / s.available_samples as f64;
        }
        s
    }

    pub fn to_json(&self) -> JsonValue {
        use JsonValue as J;
        let opt = |v: Option<f64>| match v {
            Some(x) => J::Float(x),
            None => J::Null,
        };
        J::Object(vec![
            ("unit".into(), J::Str(self.unit.as_str().into())),
            (
                "availability".into(),
                if s_available(self) {
                    J::Str("available".into())
                } else {
                    J::Str(self.availability.into())
                },
            ),
            ("samples".into(), J::UInt(self.samples as u64)),
            (
                "available_samples".into(),
                J::UInt(self.available_samples as u64),
            ),
            ("min".into(), opt(self.min)),
            ("max".into(), opt(self.max)),
            ("first".into(), opt(self.first)),
            ("last".into(), opt(self.last)),
            ("mean".into(), opt(self.mean_or_null())),
        ])
    }

    fn mean_or_null(&self) -> Option<f64> {
        (self.available_samples > 0).then_some(self.mean)
    }
}

fn s_available(s: &MetricSummary) -> bool {
    s.available_samples > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mono: u64, user: u64, kernel: u64, ws: u64) -> CounterSnapshot {
        CounterSnapshot {
            mono_ns: mono,
            cpu_user_ns: Some(user),
            cpu_kernel_ns: Some(kernel),
            resident_bytes: Some(ws),
            thread_count: Some(1),
            ..CounterSnapshot::default()
        }
    }

    fn metric(s: &DerivedSample, id: MetricId) -> &DerivedMetric {
        s.metrics
            .iter()
            .find(|m| m.id == id)
            .expect("metric present")
    }

    fn avail(d: &DerivedSample, id: MetricId) -> Availability {
        metric(d, id).availability.clone()
    }

    #[test]
    fn first_derivative_is_unavailable_not_zero() {
        let s = snap(0, 1_000_000, 500_000, 4_096);
        let d = derive_one(None, &s, Some(8));
        assert_eq!(
            avail(&d, MetricId::CpuCorePct),
            Availability::unavailable(
                UnavailableCode::FirstSample,
                "cpu rate needs a previous sample"
            ),
        );
    }

    #[test]
    fn cpu_core_pct_100_means_one_core() {
        let prev = snap(0, 0, 0, 4_096);
        // 250 ms wall, 250 ms CPU -> 100% of one core.
        let cur = snap(250_000_000, 200_000_000, 50_000_000, 4_096);
        let d = derive_one(Some(&prev), &cur, Some(8));
        let Availability::Available(v) = avail(&d, MetricId::CpuCorePct) else {
            panic!("expected value");
        };
        assert!((v - 100.0).abs() < 1e-9, "got {v}");
        let Availability::Available(m) = avail(&d, MetricId::CpuMachinePct) else {
            panic!("expected value");
        };
        assert!((m - 12.5).abs() < 1e-9, "got {m}");
    }

    #[test]
    fn multithreaded_can_exceed_100_percent() {
        let prev = snap(0, 0, 0, 4_096);
        // 250 ms wall, 500 ms CPU across threads -> 200%.
        let cur = snap(250_000_000, 400_000_000, 100_000_000, 4_096);
        let d = derive_one(Some(&prev), &cur, Some(8));
        match avail(&d, MetricId::CpuCorePct) {
            Availability::Available(v) => assert!((v - 200.0).abs() < 1e-9),
            other => panic!("expected 200%, got {other:?}"),
        }
    }

    #[test]
    fn counter_decrease_is_explicit_gap() {
        let prev = snap(0, 900_000_000, 0, 4_096);
        let cur = snap(250_000_000, 100_000_000, 0, 4_096); // reset
        let d = derive_one(Some(&prev), &cur, Some(8));
        match avail(&d, MetricId::CpuCorePct) {
            Availability::Unavailable { code, .. } => {
                assert_eq!(code, UnavailableCode::CounterResetOrGap);
            }
            other => panic!("expected gap, got {other:?}"),
        }
    }

    #[test]
    fn non_advancing_clock_is_explicit_gap() {
        let prev = snap(500, 1_000, 0, 4_096);
        let cur = snap(500, 2_000, 0, 4_096);
        let d = derive_one(Some(&prev), &cur, Some(8));
        match avail(&d, MetricId::CpuCorePct) {
            Availability::Unavailable { code, .. } => {
                assert_eq!(code, UnavailableCode::CounterResetOrGap);
            }
            other => panic!("expected gap, got {other:?}"),
        }
    }

    #[test]
    fn absent_counters_are_not_collected_never_zero() {
        let s = CounterSnapshot {
            mono_ns: 1,
            ..CounterSnapshot::default()
        };
        let d = derive_one(None, &s, None);
        assert_eq!(
            avail(&d, MetricId::ResidentBytes),
            Availability::unavailable(UnavailableCode::NotCollected, "counter absent"),
        );
        // No previous sample: the rate itself is first-sample unavailable.
        assert_eq!(
            avail(&d, MetricId::CpuMachinePct),
            Availability::unavailable(
                UnavailableCode::FirstSample,
                "cpu rate needs a previous sample"
            ),
        );
        // With a previous sample but absent CPU counters, both rates surface
        // as not collected — never as zero.
        let prev = CounterSnapshot {
            mono_ns: 0,
            ..CounterSnapshot::default()
        };
        let d = derive_one(Some(&prev), &s, None);
        assert_eq!(
            avail(&d, MetricId::CpuCorePct),
            Availability::unavailable(
                UnavailableCode::NotCollected,
                "cpu time counters not present"
            ),
        );
        assert_eq!(
            avail(&d, MetricId::CpuMachinePct),
            Availability::unavailable(
                UnavailableCode::NotCollected,
                "cpu time counters not present"
            ),
        );
    }

    #[test]
    fn series_derivation_marks_only_first_as_first_sample() {
        let series = [
            snap(0, 0, 0, 1024),
            snap(250_000_000, 100_000_000, 0, 2048),
            snap(500_000_000, 200_000_000, 0, 2048),
        ];
        let derived = derive_samples(&series, Some(4));
        assert_eq!(derived.len(), 3);
        assert!(matches!(
            avail(&derived[0], MetricId::CpuCorePct),
            Availability::Unavailable {
                code: UnavailableCode::FirstSample,
                ..
            }
        ));
        for d in &derived[1..] {
            match avail(d, MetricId::CpuCorePct) {
                Availability::Available(v) => assert!((v - 40.0).abs() < 1e-9, "{v}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn json_roundtrip_preserves_counters_and_nulls() {
        let s = CounterSnapshot {
            mono_ns: 42,
            cpu_user_ns: Some(7),
            cpu_kernel_ns: None,
            resident_bytes: Some(4096),
            thread_count: Some(3),
            ..CounterSnapshot::default()
        };
        let parsed = CounterSnapshot::from_json(&s.to_json()).unwrap();
        assert_eq!(s, parsed);
    }

    #[test]
    fn json_rejects_wrong_types() {
        let bad = crate::json::parse(r#"{"mono_ns": 5, "cpu_user_ns": "lots"}"#).unwrap();
        assert!(CounterSnapshot::from_json(&bad).is_err());
    }

    /// Property-style test (PF-106): random non-decreasing counter sequences
    /// must always produce either a non-negative rate or a structured gap —
    /// never a negative or invented value.
    #[test]
    fn property_rates_never_negative_or_invented() {
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut xorshift64 = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2_000 {
            let user_a = xorshift64() % 4_000_000_000;
            let user_b = user_a + xorshift64() % 4_000_000_000;
            let t_a = 1 + xorshift64() % 1_000_000_000;
            let t_b = t_a + 1 + xorshift64() % 1_000_000_000;
            let prev = snap(t_a, user_a, 0, 4096);
            let cur = snap(t_b, user_b, 0, 4096);
            let d = derive_one(Some(&prev), &cur, Some(2));
            match avail(&d, MetricId::CpuCorePct) {
                Availability::Available(v) => {
                    assert!(v >= 0.0, "negative rate {v}");
                    assert!(v.is_finite());
                }
                Availability::Unavailable { .. } => {}
            }
        }
    }

    #[test]
    fn summary_reports_availability_and_stats() {
        let series = [
            snap(0, 0, 0, 1024),
            snap(250_000_000, 100_000_000, 0, 2048),
            snap(500_000_000, 200_000_000, 0, 3072),
        ];
        let derived = derive_samples(&series, Some(4));
        let s = MetricSummary::summarize(MetricId::CpuCorePct, &derived);
        assert_eq!(s.samples, 3);
        assert_eq!(s.available_samples, 2);
        assert_eq!(s.first.unwrap(), 40.0);
        assert_eq!(s.last.unwrap(), 40.0);
        assert!((s.mean - 40.0).abs() < 1e-9);

        let s = MetricSummary::summarize(MetricId::ResidentBytes, &derived);
        assert_eq!(s.min, Some(1024.0));
        assert_eq!(s.max, Some(3072.0));
        let j = s.to_json();
        assert_eq!(j.get("unit").and_then(|v| v.as_str()), Some("bytes"));
    }

    #[test]
    fn metric_names_are_canonical() {
        assert_eq!(MetricId::CpuCorePct.name(), "process.cpu_core_pct");
        assert_eq!(MetricId::ResidentBytes.name(), "process.resident_bytes");
        assert_eq!(
            MetricId::IoReadBytes.name(),
            "process.io_transfer_read_bytes"
        );
    }

    #[test]
    fn cpu_u64_overflow_in_total_is_not_a_panic() {
        let prev = snap(0, u64::MAX - 10, u64::MAX - 10, 0);
        let cur = snap(1_000, u64::MAX - 5, u64::MAX - 5, 0);
        // cpu_total overflows u64 -> treated as not computable, no panic.
        let d = derive_one(Some(&prev), &cur, Some(4));
        match avail(&d, MetricId::CpuCorePct) {
            Availability::Unavailable { .. } => {}
            other => panic!("expected structured gap, got {other:?}"),
        }
    }
}
