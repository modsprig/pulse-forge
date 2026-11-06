//! Windows counter collection: least-privilege handles, per-call identity
//! recheck, exact unit mapping (roadmap PF-201..PF-205).
//!
//! Access model: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` only.
//! Permission failures map to typed errors with remediation; nothing here
//! elevates, injects or writes to the target. Handles are closed on Drop.

#![allow(unsafe_code)]

use pf_platform::contracts::{CounterSource, PlatformBackend, PlatformCapabilities, ProbeStatus};
#[cfg(test)]
use pf_types::metrics::{UnavailableCode, derive_one};
use pf_types::{CounterSnapshot, ErrorCode, Os, PfError, ProcessIdentity};

use crate::enumerate;
use crate::ffi;

/// Owning wrapper for a Win32 HANDLE.
///
/// `Send` is sound because a HANDLE is an opaque integer-like value: moving
/// it between threads does not touch the referenced kernel object, and
/// single ownership (enforced here + Drop) prevents racing CloseHandle.
#[derive(Clone, Copy)]
struct RawHandle(ffi::Handle);

unsafe impl Send for RawHandle {}

impl RawHandle {
    fn is_invalid(self) -> bool {
        self.0.is_null() || self.0 as isize == -1
    }
}

/// FILETIME (100 ns ticks since 1601-01-01) for "now".
fn now_filetime_u64() -> u64 {
    let mut ft = ffi::FileTime::default();
    // SAFETY: valid pointer to a local.
    unsafe { ffi::GetSystemTimeAsFileTime(&mut ft) };
    ft.to_u64()
}

fn tick64_ms() -> u64 {
    // SAFETY: trivial.
    unsafe { ffi::GetTickCount64() }
}

/// Approximate boot context: wall now − GetTickCount64, in FILETIME units.
///
/// Coarse (second-level) on purpose — it only needs to differ across boots.
/// Documented caveat: tick counters may include suspend time on some
/// configurations; identity still discriminates reboots correctly.
pub fn boot_identity_approx() -> String {
    // Computed once per process and cached: the value combines two clocks
    // whose jitter would otherwise make consecutive calls differ by
    // milliseconds and break identity comparisons within a session. A boot
    // discriminator only needs to be stable within a boot and different
    // across boots — caching per-process gives exactly that.
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let boot_ft = now_filetime_u64().saturating_sub(tick64_ms().saturating_mul(10_000));
            format!("boot~{boot_ft}")
        })
        .clone()
}

fn logical_cpus() -> u32 {
    let mut info: ffi::SystemInfo = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a zeroed struct of matching layout.
    unsafe { ffi::GetSystemInfo(&mut info) };
    info.number_of_processors
}

fn os_version_string() -> String {
    // RtlGetVersion is undocumented-stable; simpler evidence-grade string:
    // build number via the product policy is overkill for v0.1 — report the
    // pointer size and let `doctor` add `cmd /c ver` output from probes.
    format!("windows x86_64 ({} logical CPUs)", logical_cpus())
}

fn map_open_error(pid: u32, err: u32) -> PfError {
    match err {
        ffi::ERROR_ACCESS_DENIED => PfError::new(
            ErrorCode::PermissionDenied,
            format!("OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) denied for pid {pid}"),
        )
        .with_remediation(
            "the process may be protected, elevated or owned by another session; \
             PulseForge does not elevate",
        ),
        ffi::ERROR_INVALID_PARAMETER => PfError::new(
            ErrorCode::InvalidArgument,
            format!("pid {pid} does not exist"),
        ),
        other => PfError::new(
            ErrorCode::BackendFailure,
            format!(
                "OpenProcess failed for pid {pid}: {}",
                ffi::win32_message(other)
            ),
        ),
    }
}

fn handle_is_invalid(h: ffi::Handle) -> bool {
    h.is_null() || h as isize == -1
}

/// Opened counting target. Owns one least-privilege handle.
pub struct WindowsCounterSource {
    process: RawHandle,
    identity: ProcessIdentity,
    image_name: String,
}

impl Drop for WindowsCounterSource {
    fn drop(&mut self) {
        if !self.process.is_invalid() {
            // SAFETY: we own this handle and never reuse it after.
            unsafe { ffi::CloseHandle(self.process.0) };
        }
    }
}

/// Read cumulative CPU times + creation/exit stamps from a handle. Returns
/// `(creation_ft, exit_ft, kernel_100ns, user_100ns)`.
fn process_times(process: ffi::Handle) -> Result<(u64, u64, u64, u64), PfError> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        ffi::FileTime::default(),
        ffi::FileTime::default(),
        ffi::FileTime::default(),
        ffi::FileTime::default(),
    );
    // SAFETY: five valid pointers to locals; caller owns the handle.
    let ok =
        unsafe { ffi::GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        let err = unsafe { ffi::GetLastError() };
        return Err(PfError::new(
            ErrorCode::TargetExited,
            format!("GetProcessTimes failed: {}", ffi::win32_message(err)),
        ));
    }
    Ok((
        creation.to_u64(),
        exit.to_u64(),
        kernel.to_u64(),
        user.to_u64(),
    ))
}

fn memory_info(process: ffi::Handle) -> Result<ffi::ProcessMemoryCountersEx, PfError> {
    let mut mem: ffi::ProcessMemoryCountersEx = unsafe { std::mem::zeroed() };
    mem.cb = u32::try_from(std::mem::size_of::<ffi::ProcessMemoryCountersEx>()).unwrap_or(80);
    // SAFETY: pointer/size pair to a local of matching layout.
    let ok = unsafe { ffi::K32GetProcessMemoryInfo(process, &mut mem, mem.cb) };
    if ok == 0 {
        let err = unsafe { ffi::GetLastError() };
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            format!("GetProcessMemoryInfo failed: {}", ffi::win32_message(err)),
        ));
    }
    Ok(mem)
}

fn io_counters(process: ffi::Handle) -> Result<ffi::IoCounters, PfError> {
    let mut io: ffi::IoCounters = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a local.
    let ok = unsafe { ffi::GetProcessIoCounters(process, &mut io) };
    if ok == 0 {
        let err = unsafe { ffi::GetLastError() };
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            format!("GetProcessIoCounters failed: {}", ffi::win32_message(err)),
        ));
    }
    Ok(io)
}

fn handle_count(process: ffi::Handle) -> Result<u32, PfError> {
    let mut count: u32 = 0;
    // SAFETY: valid pointer to a local.
    let ok = unsafe { ffi::GetProcessHandleCount(process, &mut count) };
    if ok == 0 {
        let err = unsafe { ffi::GetLastError() };
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            format!("GetProcessHandleCount failed: {}", ffi::win32_message(err)),
        ));
    }
    Ok(count)
}

impl CounterSource for WindowsCounterSource {
    fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    fn image_name(&self) -> &str {
        &self.image_name
    }

    fn snapshot(&mut self, mono_ns: u64) -> Result<CounterSnapshot, PfError> {
        let (creation, exit, kernel_100ns, user_100ns) = process_times(self.process.0)?;
        // Identity recheck on every sample: a different creation stamp for
        // the same PID means the original process exited and the PID was
        // reused — refuse to feed the new process into this session.
        if creation != self.identity.creation_identity {
            return Err(PfError::new(
                ErrorCode::IdentityMismatch,
                format!(
                    "pid {} now maps to a process created at {creation}, expected {} \
                     (PID reuse or wrong process)",
                    self.identity.pid, self.identity.creation_identity
                ),
            ));
        }
        if exit != 0 {
            return Err(PfError::new(
                ErrorCode::TargetExited,
                format!("pid {} has exited", self.identity.pid),
            ));
        }
        let filetime_to_ns = |ft: u64| ft.saturating_mul(100);
        let mem = memory_info(self.process.0)?;
        let io = io_counters(self.process.0)?;
        let handles = handle_count(self.process.0).ok();
        let threads = enumerate::thread_count_of(self.identity.pid);
        Ok(CounterSnapshot {
            mono_ns,
            cpu_user_ns: Some(filetime_to_ns(user_100ns)),
            cpu_kernel_ns: Some(filetime_to_ns(kernel_100ns)),
            resident_bytes: Some(mem.working_set_size as u64),
            resident_peak_bytes: Some(mem.peak_working_set_size as u64),
            private_commit_bytes: Some(mem.private_usage as u64),
            private_commit_peak_bytes: Some(mem.peak_pagefile_usage as u64),
            io_read_ops: Some(io.read_operation_count),
            io_write_ops: Some(io.write_operation_count),
            io_read_bytes: Some(io.read_transfer_count),
            io_write_bytes: Some(io.write_transfer_count),
            thread_count: threads,
            handle_count: handles,
            page_faults: Some(u64::from(mem.page_fault_count)),
        })
    }
}

/// Windows backend implementation.
#[derive(Default)]
pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        WindowsPlatform
    }
}

impl PlatformBackend for WindowsPlatform {
    fn name(&self) -> &'static str {
        "pf-windows"
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            os: Os::Windows,
            os_version: os_version_string(),
            logical_cpus: Some(logical_cpus()).filter(|n| *n > 0),
            counters: {
                // Feature-probe on ourselves: if we can query our own
                // process, the counters path is live for same-user targets.
                let pid = std::process::id();
                match self.open(pid) {
                    Ok(_) => ProbeStatus::ok(),
                    Err(e) => ProbeStatus::unavailable("BACKEND_FAILURE", e.to_string()),
                }
            },
            cpu_stack_sampling: ProbeStatus::unavailable(
                "NOT_IMPLEMENTED",
                "ETW stack sampling ships in milestone v0.6; \
                 see docs/feasibility-results.md for the privilege probe",
            ),
        }
    }

    fn enumerate(
        &self,
        limit: usize,
    ) -> Result<Vec<pf_platform::contracts::ProcessEntry>, PfError> {
        enumerate::processes(limit)
    }

    fn identity(&self, pid: u32) -> Result<ProcessIdentity, PfError> {
        // SAFETY-of-usage note: handle closed by Drop of the source we open
        // internally; reuse open() to keep one code path for access mapping.
        let src = self.open(pid)?;
        Ok(src.identity().clone())
    }

    fn open(&self, pid: u32) -> Result<Box<dyn CounterSource>, PfError> {
        // SAFETY: plain call with a numeric pid.
        let handle = unsafe { ffi::OpenProcess(ffi::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle_is_invalid(handle) {
            let err = unsafe { ffi::GetLastError() };
            return Err(map_open_error(pid, err));
        }
        // Resolve creation time through the freshly opened handle; on
        // failure close the handle before returning the typed error.
        // A non-zero exit time means the process object still exists (some
        // handle lingers) but the process is DEAD — callers like the
        // single-instance lock must not treat it as a live owner.
        let creation = match process_times(handle) {
            Ok((creation, exit, _, _)) => {
                if exit != 0 {
                    // SAFETY: we own this handle and never reuse it after.
                    unsafe { ffi::CloseHandle(handle) };
                    return Err(PfError::new(
                        ErrorCode::TargetExited,
                        format!("pid {pid} has exited (zombie handle)"),
                    ));
                }
                creation
            }
            Err(e) => {
                // SAFETY: we own this handle and never reuse it after.
                unsafe { ffi::CloseHandle(handle) };
                return Err(e);
            }
        };
        Ok(Box::new(WindowsCounterSource {
            process: RawHandle(handle),
            identity: ProcessIdentity::new(Os::Windows, pid, creation, boot_identity_approx()),
            image_name: image_name_of(handle).unwrap_or_default(),
        }))
    }
}

fn image_name_of(handle: ffi::Handle) -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: pointer/size pair to a local buffer.
    let len = unsafe {
        ffi::K32GetProcessImageFileNameW(
            handle,
            buf.as_mut_ptr(),
            u32::try_from(buf.len()).unwrap_or(1024),
        )
    };
    if len == 0 {
        return None;
    }
    Some(ffi::widestr_to_string(&buf[..len as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_counters_are_available() {
        let backend = WindowsPlatform::new();
        let caps = backend.capabilities();
        assert!(caps.counters.available, "{:?}", caps.counters);
        assert_eq!(caps.os, Os::Windows);
        assert!(caps.logical_cpus.unwrap_or(0) > 0);
    }

    #[test]
    fn open_self_snapshot_has_exact_counters() {
        let backend = WindowsPlatform::new();
        let mut src = backend.open(std::process::id()).unwrap();
        let s = src.snapshot(0).unwrap();
        // CPU fields must always be present (Some) — an environment that
        // zeroes CPU accounting still reports the counter; it just reports
        // zeros (see docs/feasibility-results.md: sandbox CPU accounting).
        assert!(s.cpu_user_ns.is_some());
        assert!(s.cpu_kernel_ns.is_some());
        assert!(s.resident_bytes.unwrap_or(0) > 0);
        assert!(s.resident_peak_bytes.unwrap_or(0) >= s.resident_bytes.unwrap_or(0));
        assert!(s.private_commit_bytes.unwrap_or(0) > 0);
        assert!(s.handle_count.unwrap_or(0) > 0);
        assert!(s.thread_count.unwrap_or(0) >= 1);
        // Working set and commit are distinct semantics; both must be sane.
        assert!(s.resident_bytes.unwrap() < 1 << 40, "ws < 1 TiB");
        assert!(s.private_commit_bytes.unwrap() < 1 << 40, "commit < 1 TiB");
    }

    #[test]
    fn cpu_counters_monotonic_and_plausible_when_accounted() {
        let backend = WindowsPlatform::new();
        let mut src = backend.open(std::process::id()).unwrap();
        // Burn a little CPU so an accounting environment has something to
        // report.
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(100) {
            std::hint::spin_loop();
        }
        let a = src.snapshot(0).unwrap();
        // Burn CPU on THIS thread between the two snapshots: the original
        // relied on sibling tests running in parallel, which made the
        // strict-increase assertion a scheduler-timing flake under varying
        // load. 100 ms of own-thread spin is deterministic.
        let t1 = std::time::Instant::now();
        while t1.elapsed() < std::time::Duration::from_millis(100) {
            std::hint::spin_loop();
        }
        let b = src.snapshot(50_000_000).unwrap();
        let total = |s: &CounterSnapshot| {
            s.cpu_user_ns
                .unwrap_or(0)
                .saturating_add(s.cpu_kernel_ns.unwrap_or(0))
        };
        assert!(total(&b) >= total(&a), "cumulative CPU must not decrease");
        // When the environment accounts CPU at all, 100 ms of spin must show
        // up as positive CPU time between the two samples.
        if total(&a) > 0 || total(&b) > 0 {
            assert!(total(&b) > total(&a), "spin must accumulate CPU time");
        }
    }

    #[test]
    fn identity_of_self_is_stable_across_calls() {
        let backend = WindowsPlatform::new();
        let a = backend.identity(std::process::id()).unwrap();
        let b = backend.identity(std::process::id()).unwrap();
        assert_eq!(a.check(&b), pf_types::IdentityCheck::Match);
        assert!(a.creation_identity > 0);
    }

    #[test]
    fn nonexistent_pid_is_invalid_argument() {
        let backend = WindowsPlatform::new();
        // 0xFFFFFFF0 is beyond normal PID space and not a live process.
        match backend.open(0xFFFF_FFF0) {
            Err(e) => assert!(
                e.code == ErrorCode::InvalidArgument || e.code == ErrorCode::PermissionDenied,
                "unexpected code for missing pid: {e}"
            ),
            Ok(_) => panic!("0xFFFFFFF0 should never open"),
        }
    }

    #[test]
    fn pid_zero_is_typed_error_not_panic() {
        let backend = WindowsPlatform::new();
        match backend.open(0) {
            Err(e) => assert!(
                e.code == ErrorCode::InvalidArgument || e.code == ErrorCode::PermissionDenied,
                "pid 0 must produce a typed error, got {e}"
            ),
            Ok(_) => panic!("pid 0 should never open"),
        }
    }

    #[test]
    fn protected_process_denial_is_typed_when_denied() {
        // PID 4 (System) is protected for non-admin callers. On elevated CI
        // runners the open may succeed; both outcomes must be *well-formed*.
        let backend = WindowsPlatform::new();
        match backend.open(4) {
            Ok(mut src) => {
                // Elevated context: snapshot must still be consistent.
                let s = src.snapshot(1).unwrap();
                assert!(s.resident_bytes.unwrap_or(0) > 0);
            }
            Err(e) => {
                assert_eq!(e.code, ErrorCode::PermissionDenied, "{e}");
                assert!(e.remediation.is_some(), "remediation required");
            }
        }
    }

    #[test]
    fn snapshots_are_monotonic_in_cumulative_cpu() {
        let backend = WindowsPlatform::new();
        let mut src = backend.open(std::process::id()).unwrap();
        let a = src.snapshot(0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let b = src.snapshot(50_000_000).unwrap();
        let total = |s: &CounterSnapshot| {
            s.cpu_user_ns
                .unwrap_or(0)
                .saturating_add(s.cpu_kernel_ns.unwrap_or(0))
        };
        assert!(total(&b) >= total(&a), "cumulative CPU must not decrease");
    }

    #[test]
    fn boot_identity_is_populated() {
        let backend = WindowsPlatform::new();
        let id = backend.identity(std::process::id()).unwrap();
        assert!(id.boot_identity.starts_with("boot~"));
    }

    #[test]
    fn availability_enum_is_used_for_missing_thread_data() {
        // A snapshot without thread data must derive `not_collected`, never
        // a zero that would read as "no threads".
        let snap = CounterSnapshot {
            mono_ns: 1,
            thread_count: None,
            ..CounterSnapshot::default()
        };
        let d = derive_one(None, &snap, Some(4));
        let tc = d
            .metrics
            .iter()
            .find(|m| m.id == pf_types::MetricId::ThreadCount)
            .unwrap();
        match &tc.availability {
            pf_types::Availability::Unavailable { code, .. } => {
                assert_eq!(*code, UnavailableCode::NotCollected);
            }
            pf_types::Availability::Available(_) => panic!("None must not become a value"),
        }
        // Conversely, an enumerated self always yields a positive count.
        let live = CounterSnapshot {
            mono_ns: 2,
            thread_count: enumerate::thread_count_of(std::process::id()),
            ..CounterSnapshot::default()
        };
        let d = derive_one(None, &live, Some(4));
        let tc = d
            .metrics
            .iter()
            .find(|m| m.id == pf_types::MetricId::ThreadCount)
            .unwrap();
        match &tc.availability {
            pf_types::Availability::Available(v) => assert!(*v >= 1.0),
            pf_types::Availability::Unavailable { code, .. } => {
                panic!("self threads must be countable, got {code:?}")
            }
        }
    }
}
