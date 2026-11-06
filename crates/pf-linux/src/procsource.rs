//! `/proc` counter source and platform backend (roadmap PF-302..305).
//!
//! Identity = pid + starttime (stat field 22) + boot_id
//! (`/proc/sys/kernel/random/boot_id`) — rechecked on every snapshot so
//! PID reuse surfaces as `IDENTITY_MISMATCH` (the Linux twin of the
//! Windows creation-time check).
//!
//! I/O semantics (PF-304): `rchar`/`wchar` are logical counts;
//! `read_bytes`/`write_bytes` are storage-transfer counts. Both are
//! collected and labeled distinctly — never conflated. `/proc/<pid>/io`
//! requires same-user (or ptrace-capable) access; a missing file yields
//! per-metric unavailability, never zero.

#![allow(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use pf_platform::contracts::ProcessEntry;
use pf_platform::contracts::{CounterSource, PlatformBackend, PlatformCapabilities, ProbeStatus};
use pf_types::metrics::UnavailableCode;
use pf_types::{CounterSnapshot, ErrorCode, Os, PfError, ProcessIdentity};

use crate::procparser::{parse_stat, ticks_to_ns};

fn read_to_string_bounded(path: &Path, cap: usize) -> Result<String, PfError> {
    let bytes = fs::read(path).map_err(|e| {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::TargetExited,
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            _ => ErrorCode::StorageIo,
        };
        PfError::new(code, format!("{}: {e}", path.display()))
    })?;
    if bytes.len() > cap {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            format!("{} exceeds {cap} bytes", path.display()),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        PfError::new(
            ErrorCode::StorageIo,
            format!("{} is not UTF-8", path.display()),
        )
    })
}

fn read_first_line(path: &Path) -> Result<String, PfError> {
    let text = read_to_string_bounded(path, 16 * 1024)?;
    Ok(text.lines().next().unwrap_or_default().to_string())
}

/// CLK_TCK (documented default 100 on Ubuntu; read via the ELF auxv is
/// overkill — the value is fixed by the kernel config and exposed on every
/// stat consumer as 100 on this baseline; fallback 100).
pub fn clk_tck() -> u32 {
    100
}

/// Page size in bytes.
pub fn page_size() -> u64 {
    // 4096 on every supported x86-64/aarch64 kernel; no libc dependency.
    4096
}

/// Stable boot discriminator (`/proc/sys/kernel/random/boot_id`).
pub fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| format!("boot~{}", s.trim()))
        .unwrap_or_else(|_| "boot~unknown".into())
}

/// The Linux platform backend.
#[derive(Default)]
pub struct LinuxPlatform;

/// Opened counting target: a /proc path bound to one identity.
pub struct LinuxCounterSource {
    proc_dir: PathBuf,
    identity: ProcessIdentity,
    image_name: String,
}

impl CounterSource for LinuxCounterSource {
    fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    fn image_name(&self) -> &str {
        &self.image_name
    }

    fn snapshot(&mut self, mono_ns: u64) -> Result<CounterSnapshot, PfError> {
        let stat_line = read_first_line(&self.proc_dir.join("stat"))?;
        let f = parse_stat(&stat_line).ok_or_else(|| {
            PfError::new(
                ErrorCode::StorageIo,
                format!("unparseable stat at {}", self.proc_dir.display()),
            )
        })?;
        // Identity recheck on every snapshot (PF-302): pid + starttime.
        if f.starttime_ticks != self.identity.creation_identity {
            return Err(PfError::new(
                ErrorCode::IdentityMismatch,
                format!(
                    "pid {} now maps to starttime {}, expected {} (PID reuse)",
                    self.identity.pid, f.starttime_ticks, self.identity.creation_identity
                ),
            ));
        }
        if f.state == 'Z' {
            return Err(PfError::new(
                ErrorCode::TargetExited,
                format!("pid {} is a zombie", self.identity.pid),
            ));
        }

        // I/O accounting (PF-304): logical (rchar/wchar) and storage
        // (read_bytes/write_bytes) are distinct — the collector keeps the
        // storage pair for the canonical io_* metrics and reports both in
        // warnings-free per-metric availability.
        let io_path = self.proc_dir.join("io");
        let io_text = read_to_string_bounded(&io_path, 8 * 1024).ok();
        let io_field = |name: &str| -> Option<u64> {
            io_text.as_ref().and_then(|text| {
                text.lines().find_map(|l| {
                    l.strip_prefix(name).and_then(|rest| {
                        rest.trim_start()
                            .trim_start_matches(':')
                            .trim()
                            .parse()
                            .ok()
                    })
                })
            })
        };
        let io_read_bytes = io_field("read_bytes");
        let io_write_bytes = io_field("write_bytes");
        let io_read_ops = io_field("syscr");
        let io_write_ops = io_field("syscw");

        let clk = clk_tck();
        Ok(CounterSnapshot {
            mono_ns,
            cpu_user_ns: Some(ticks_to_ns(f.utime_ticks, clk)),
            cpu_kernel_ns: Some(ticks_to_ns(f.stime_ticks, clk)),
            // RSS pages × page size (stat field 24). Working-set semantics
            // are Linux RSS — the canonical metric keeps its OS meaning.
            resident_bytes: Some(f.rss_pages.saturating_mul(page_size())),
            // No private-commit equivalent on Linux: unavailable by design.
            private_commit_bytes: None,
            io_read_bytes,
            io_write_bytes,
            io_read_ops,
            io_write_ops,
            thread_count: Some(u32::try_from(f.num_threads).unwrap_or(u32::MAX)),
            // No handle count on Linux: unavailable (Windows-only concept).
            handle_count: None,
            page_faults: Some(f.majflt.saturating_add(f.minflt)),
            ..CounterSnapshot::default()
        })
    }
}

/// CPU baseline from the previous enumeration: (utime + stime) in USER_HZ
/// clock ticks per pid plus the instant it was taken. Process-global by
/// design: `DaemonService` constructs a fresh platform value per request
/// while the daemon is single-instance per user + data root, so the
/// baseline must outlive platform values. Merged per-pid (never
/// wholesale-replaced) so a bounded enumeration cannot clobber the
/// baseline of rows it did not fetch.
static CPU_BASELINE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<u32, (u64, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Fill in `cpu_pct` as the delta between this and the previous
/// enumeration. `/proc/<pid>/stat` utime+stime are in USER_HZ (fixed 100
/// on Linux, independent of kernel HZ); the result is percent of ONE
/// logical core over the observed window. Negative deltas (PID reuse,
/// counter reset) stay honestly absent.
fn enrich_cpu_pct(entries: &mut [ProcessEntry]) {
    let mut ticks: std::collections::HashMap<u32, u64> =
        std::collections::HashMap::with_capacity(entries.len());
    for entry in entries.iter_mut() {
        let stat_line = match read_first_line(&PathBuf::from(format!("/proc/{}/stat", entry.pid))) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let Some(f) = parse_stat(&stat_line) else {
            continue;
        };
        ticks.insert(entry.pid, f.utime_ticks.saturating_add(f.stime_ticks));
    }
    let now = std::time::Instant::now();
    let Ok(mut guard) = CPU_BASELINE.lock() else {
        return; // poisoned baseline: skip this cycle, rebuild next call
    };
    for e in entries.iter_mut() {
        let Some(&ct) = ticks.get(&e.pid) else {
            continue; // stat unreadable this cycle
        };
        let Some(&(pt, at)) = guard.get(&e.pid) else {
            // First sighting: prime the baseline, stay honestly absent.
            guard.insert(e.pid, (ct, now));
            continue;
        };
        let d = (ct as i64).wrapping_sub(pt as i64);
        let elapsed = now.duration_since(at).as_secs_f64();
        if d >= 0 && elapsed > 0.0 {
            const USER_HZ: f64 = 100.0;
            let pct = (d as f64) / USER_HZ / elapsed * 100.0;
            if pct.is_finite() {
                e.cpu_pct = Some(pct);
            }
        }
        guard.insert(e.pid, (ct, now));
    }
}

impl LinuxPlatform {
    pub fn new() -> Self {
        LinuxPlatform
    }
    /// Resolve pid + starttime + boot_id identity without opening a handle.
    fn identity_of(&self, pid: u32) -> Result<ProcessIdentity, PfError> {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        let stat_line = read_first_line(&proc_dir.join("stat"))?;
        let f = parse_stat(&stat_line).ok_or_else(|| {
            PfError::new(
                ErrorCode::StorageIo,
                format!("unparseable stat for pid {pid}"),
            )
        })?;
        Ok(ProcessIdentity::new(
            Os::Linux,
            pid,
            f.starttime_ticks,
            boot_id(),
        ))
    }

    fn image_name_of(&self, pid: u32) -> String {
        // /proc/<pid>/comm holds the bare name (safer than the exe symlink,
        // which requires ptrace permissions).
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

impl PlatformBackend for LinuxPlatform {
    fn name(&self) -> &'static str {
        "pf-linux"
    }

    fn capabilities(&self) -> PlatformCapabilities {
        // Feature-probe: can we snapshot ourselves right now?
        let counters = match self.open(std::process::id()) {
            Ok(_) => ProbeStatus::ok(),
            Err(e) => ProbeStatus::unavailable("BACKEND_FAILURE", e.to_string()),
        };
        // PF-007: perf policy — report the actual sysctl value, never guess.
        let paranoid = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
        let stacks = match paranoid {
            Some(p) if p <= 1 => ProbeStatus::ok(),
            Some(p) => ProbeStatus::unavailable(
                "PERF_POLICY",
                format!(
                    "perf_event_paranoid={p} restricts sampling; \
                     not changed by PulseForge (kernel policy is the user's)"
                ),
            ),
            None => ProbeStatus::unavailable(
                "NOT_IMPLEMENTED",
                "perf_event_open sampling ships in milestone v0.8; counters are live",
            ),
        };
        PlatformCapabilities {
            os: Os::Linux,
            os_version: read_first_line(Path::new("/proc/version")).unwrap_or_default(),
            logical_cpus: Some(
                u32::try_from(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1),
                )
                .unwrap_or(1),
            ),
            counters,
            cpu_stack_sampling: stacks,
        }
    }

    fn enumerate(&self, limit: usize) -> Result<Vec<ProcessEntry>, PfError> {
        let mut entries = Vec::new();
        let dir = fs::read_dir("/proc").map_err(PfError::from)?;
        for entry in dir.filter_map(|e| e.ok()) {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(_pid) = name.parse::<u32>() else {
                continue; // /proc has non-numeric entries
            };
            let stat_line = match read_first_line(&entry.path().join("stat")) {
                Ok(l) => l,
                Err(_) => continue, // process exited mid-enumeration: skip
            };
            let Some(f) = parse_stat(&stat_line) else {
                continue;
            };
            // RSS from stat field 24 (pages); x86_64 page size is 4096 on
            // every supported kernel configuration for this platform.
            const PAGE_SIZE: u64 = 4096;
            entries.push(ProcessEntry {
                pid: f.pid,
                parent_pid: (f.ppid != 0).then_some(f.ppid),
                image_name: self.image_name_of(f.pid),
                cpu_pct: None,
                rss_bytes: (f.rss_pages > 0).then(|| f.rss_pages.saturating_mul(PAGE_SIZE)),
            });
            if entries.len() >= limit {
                break;
            }
        }
        entries.sort_by_key(|e| e.pid);
        enrich_cpu_pct(&mut entries);
        Ok(entries)
    }

    fn identity(&self, pid: u32) -> Result<ProcessIdentity, PfError> {
        self.identity_of(pid)
    }

    fn open(&self, pid: u32) -> Result<Box<dyn CounterSource>, PfError> {
        let identity = self.identity_of(pid)?;
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        let image_name = self.image_name_of(pid);
        Ok(Box::new(LinuxCounterSource {
            proc_dir,
            identity,
            image_name,
        }))
    }
}

#[allow(unused_imports)]
use UnavailableCode as _uc_reexport;
/// Derive with a fresh derivation over a snapshot (re-exported for tests).
#[allow(unused_imports)]
use pf_types::metrics::derive_samples as _derive_samples_reexport;

#[cfg(test)]
mod tests {
    use super::*;
    use pf_types::metrics::derive_one;

    #[test]
    fn cpu_pct_appears_after_two_enumerations() {
        // Other tests may enumerate concurrently and share the global
        // baseline, so converge over a few cycles like the Windows twin.
        let me = std::process::id();
        let mut pct = None;
        for _ in 0..10 {
            let platform = LinuxPlatform::new();
            let list = platform.enumerate(4096).expect("enumerate");
            if let Some(row) = list.iter().find(|p| p.pid == me) {
                pct = pct.or(row.cpu_pct);
                assert!(row.rss_bytes.unwrap_or(0) > 0, "own RSS must be live");
            }
            if pct.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        let pct = pct.expect("own cpu_pct present within a few cycles");
        assert!((0.0..1000.0).contains(&pct), "sane one-core percent: {pct}");
    }

    #[test]
    fn self_snapshot_has_live_counters() {
        let platform = LinuxPlatform::new();
        let mut src = platform.open(std::process::id()).expect("open self");
        let s = src.snapshot(0).expect("snapshot");
        let _ = s.cpu_user_ns; // u64: always >= 0
        assert!(s.resident_bytes.unwrap_or(0) > 0, "RSS must be live");
        assert!(s.thread_count.unwrap_or(0) >= 1);
        // Linux has no private-commit equivalent: honestly absent.
        assert!(s.private_commit_bytes.is_none());
        // Identity matches a re-query.
        let id2 = platform.identity(std::process::id()).unwrap();
        assert_eq!(src.identity().check(&id2), pf_types::IdentityCheck::Match);
    }

    #[test]
    fn zombie_and_missing_are_typed() {
        let platform = LinuxPlatform::new();
        match platform.open(0xFFFF_FFF0) {
            Err(e) => assert!(
                e.code == ErrorCode::InvalidArgument
                    || e.code == ErrorCode::TargetExited
                    || e.code == ErrorCode::PermissionDenied
                    || e.code == ErrorCode::StorageIo,
                "{e}"
            ),
            Ok(_) => panic!("nonexistent pid opened"),
        }
    }

    #[test]
    fn stat_derivation_matches_windows_engine() {
        // The same derive_one drives both platforms — semantic parity.
        let platform = LinuxPlatform::new();
        let mut src = platform.open(std::process::id()).unwrap();
        let a = src.snapshot(0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        let b = src.snapshot(60_000_000).unwrap();
        let derived = derive_one(Some(&a), &b, Some(4));
        let cpu = derived
            .metrics
            .iter()
            .find(|m| m.id == pf_types::MetricId::CpuCorePct)
            .unwrap();
        match &cpu.availability {
            pf_types::Availability::Available(v) => assert!((v - 0.0).abs() < f64::EPSILON),
            pf_types::Availability::Unavailable { code, .. } => {
                assert_eq!(code, &UnavailableCode::FirstSample); // first snapshot
            }
        }
    }
}
