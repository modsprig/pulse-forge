//! Collector contracts (roadmap PF-103).
//!
//! OS handles live strictly inside the implementing backend. Consumers get
//! typed data and typed errors. Capability reporting is explicit: a backend
//! that cannot stack-sample says so with a reason, and a backend that was not
//! compiled for this OS reports `counters: unavailable` rather than
//! pretending.

use pf_types::{CounterSnapshot, Os, PfError, ProcessIdentity};

/// One capability and, when absent, why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeStatus {
    pub available: bool,
    /// Stable machine-readable reason when `!available`.
    pub reason_code: String,
    pub detail: String,
}

impl ProbeStatus {
    pub fn ok() -> Self {
        ProbeStatus {
            available: true,
            reason_code: String::new(),
            detail: String::new(),
        }
    }

    pub fn unavailable(code: &str, detail: impl Into<String>) -> Self {
        ProbeStatus {
            available: false,
            reason_code: code.to_string(),
            detail: detail.into(),
        }
    }
}

/// What a backend can do on this machine right now.
#[derive(Clone, Debug)]
pub struct PlatformCapabilities {
    pub os: Os,
    /// OS version/build string as reported by the backend (evidence-grade).
    pub os_version: String,
    pub logical_cpus: Option<u32>,
    /// Low-privilege process counters.
    pub counters: ProbeStatus,
    /// Native on-CPU stack sampling (ETW / perf_event_open).
    pub cpu_stack_sampling: ProbeStatus,
}

impl PlatformCapabilities {
    pub fn empty(os: Os) -> Self {
        PlatformCapabilities {
            os,
            os_version: String::new(),
            logical_cpus: None,
            counters: ProbeStatus::unavailable(
                "NO_BACKEND",
                "no collector backend compiled for this platform",
            ),
            cpu_stack_sampling: ProbeStatus::unavailable(
                "NO_BACKEND",
                "no collector backend compiled for this platform",
            ),
        }
    }
}

/// A live process row as returned by enumeration. Image names may be
/// redacted per privacy policy by the caller; the backend returns what the OS
/// provides.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    /// Image/executable file name without path where the OS reports it so.
    pub image_name: String,
    /// Percent of ONE logical core over the interval since the previous
    /// enumeration (backend-computed delta, Task-Manager semantics).
    /// `None` = no baseline yet, PID reuse detected, or the OS denied the
    /// query — absence is honest, never a zero guess.
    pub cpu_pct: Option<f64>,
    /// Resident set size in bytes when the OS reports it (`None` when the
    /// query is denied or unsupported).
    pub rss_bytes: Option<u64>,
}

/// A opened counting target. One instance == one least-privilege OS handle
/// bound to one [`ProcessIdentity`]; every [`CounterSource::snapshot`]
/// rechecks identity and fails with `IDENTITY_MISMATCH` on PID reuse.
pub trait CounterSource: Send {
    fn identity(&self) -> &ProcessIdentity;
    /// Human-facing image name (backend-defined precision).
    fn image_name(&self) -> &str;

    /// Take one raw counter snapshot. `mono_ns` is filled by the recorder
    /// clock, not the backend; backends fill counters only.
    fn snapshot(&mut self, mono_ns: u64) -> Result<CounterSnapshot, PfError>;
}

/// Platform backend: enumeration, identity, capability probing and target
/// acquisition. Implementations hold no per-target state.
pub trait PlatformBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> PlatformCapabilities;

    /// Enumerate processes visible to the current user. Backends bound the
    /// result (see backend docs for the cap) and report truncation via error
    /// detail rather than returning unbounded data.
    fn enumerate(&self, limit: usize) -> Result<Vec<ProcessEntry>, PfError>;

    /// Resolve the current identity for a PID.
    fn identity(&self, pid: u32) -> Result<ProcessIdentity, PfError>;

    /// Open a counting target with the least privilege the OS allows.
    /// Permission failures return `PERMISSION_DENIED` with remediation —
    /// never a panic, never a zeroed snapshot channel.
    fn open(&self, pid: u32) -> Result<Box<dyn CounterSource>, PfError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_capabilities_are_explicitly_unavailable() {
        let caps = PlatformCapabilities::empty(Os::Linux);
        assert!(!caps.counters.available);
        assert_eq!(caps.counters.reason_code, "NO_BACKEND");
        assert!(!caps.cpu_stack_sampling.available);
    }

    #[test]
    fn probe_status_shapes() {
        let ok = ProbeStatus::ok();
        assert!(ok.available && ok.reason_code.is_empty());
        let no = ProbeStatus::unavailable("CAPABILITY_UNAVAILABLE", "needs elevation");
        assert!(!no.available);
        assert_eq!(no.reason_code, "CAPABILITY_UNAVAILABLE");
    }
}
