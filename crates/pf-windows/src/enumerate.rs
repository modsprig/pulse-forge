//! Process and thread enumeration via Toolhelp32 (roadmap PF-201).
//!
//! Live-resource enrichment: every enumeration also carries per-process
//! CPU% (delta since the previous enumeration — Task-Manager semantics)
//! and resident set size, so the GUI picker shows which process is
//! actually hot. Absence (`None`) is honest: no baseline yet, PID reuse,
//! or an access-denied handle — never a zero guess.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use pf_platform::contracts::ProcessEntry;
use pf_types::{ErrorCode, PfError};

use crate::ffi;

/// Default bound on enumerated processes (bounded by contract).
pub const DEFAULT_ENUM_LIMIT: usize = 1024;

/// CPU-time baseline from the previous enumeration: (kernel, user) in
/// 100 ns units per pid, plus the instant that baseline was taken.
///
/// Process-global by design: `DaemonService` constructs a fresh platform
/// value per request while the daemon itself is single-instance per user
/// and data root, so the baseline must outlive individual platform values.
/// Merged per-pid (never wholesale-replaced) so a bounded enumeration
/// cannot clobber the baseline of rows it did not fetch.
type CpuBaselineMap = HashMap<u32, (u64, u64, Instant)>;
static CPU_BASELINE: std::sync::LazyLock<Mutex<CpuBaselineMap>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Enumerate processes visible in the current session, up to `limit`.
/// Result is sorted by PID for stable output.
pub fn processes(limit: usize) -> Result<Vec<ProcessEntry>, PfError> {
    // SAFETY: plain call with documented flags.
    let snapshot = unsafe { ffi::CreateToolhelp32Snapshot(ffi::TH32CS_SNAPPROCESS, 0) };
    if snapshot == ffi::INVALID_HANDLE_VALUE {
        let err = unsafe { ffi::GetLastError() };
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            format!(
                "CreateToolhelp32Snapshot failed: {}",
                ffi::win32_message(err)
            ),
        ));
    }
    // RAII-ish guard: close on every path.
    struct Snap(ffi::Handle);
    impl Drop for Snap {
        fn drop(&mut self) {
            // SAFETY: we own this handle.
            unsafe { ffi::CloseHandle(self.0) };
        }
    }
    let _snap = Snap(snapshot);

    let mut entry: ffi::ProcessEntry32W = unsafe { std::mem::zeroed() };
    entry.dw_size = u32::try_from(std::mem::size_of::<ffi::ProcessEntry32W>()).unwrap_or(568);
    let mut out = Vec::new();
    // SAFETY: snapshot owned; pointer to a correctly sized local.
    let mut ok = unsafe { ffi::Process32FirstW(snapshot, &mut entry) };
    while ok != 0 {
        out.push(ProcessEntry {
            pid: entry.th32_process_id,
            parent_pid: (entry.th32_parent_process_id != 0).then_some(entry.th32_parent_process_id),
            image_name: ffi::widestr_to_string(&entry.sz_exe_file),
            cpu_pct: None,
            rss_bytes: None,
        });
        if out.len() >= limit {
            break;
        }
        // SAFETY: same as above.
        ok = unsafe { ffi::Process32NextW(snapshot, &mut entry) };
    }
    out.sort_by_key(|p| p.pid);
    enrich_live_resources(&mut out);
    Ok(out)
}

/// Fill in `cpu_pct` (delta vs. the previous enumeration) and `rss_bytes`
/// for as many rows as the OS allows. Access-denied rows keep `None`.
fn enrich_live_resources(entries: &mut [ProcessEntry]) {
    struct HandleGuard(ffi::Handle);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // SAFETY: we own this handle.
            unsafe { ffi::CloseHandle(self.0) };
        }
    }

    let mut ticks: HashMap<u32, (u64, u64)> = HashMap::with_capacity(entries.len());
    for entry in entries.iter_mut() {
        // SAFETY: plain call with documented access rights.
        let handle =
            unsafe { ffi::OpenProcess(ffi::PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.pid) };
        if handle.is_null() || handle == ffi::INVALID_HANDLE_VALUE {
            continue; // denied (protected/system process): honest absence
        }
        let _guard = HandleGuard(handle);
        if let Some((k, u)) = cpu_times_100ns(handle) {
            ticks.insert(entry.pid, (k, u));
        }
        entry.rss_bytes = resident_bytes(handle);
    }

    let now = Instant::now();
    let Ok(mut guard) = CPU_BASELINE.lock() else {
        return; // poisoned baseline: skip this cycle, rebuild next call
    };
    for e in entries.iter_mut() {
        let Some(&(ck, cu)) = ticks.get(&e.pid) else {
            continue; // handle denied this cycle
        };
        let Some(&(pk, pu, at)) = guard.get(&e.pid) else {
            // First sighting: prime the baseline, stay honestly absent.
            guard.insert(e.pid, (ck, cu, now));
            continue;
        };
        // PID reuse can make the delta wrap — a wrapped 100 ns count is
        // larger than any sane value, so `try_from` reports absence.
        let d_kernel = ck.wrapping_sub(pk);
        let d_user = cu.wrapping_sub(pu);
        let d_total = d_kernel.saturating_add(d_user);
        let Ok(total_i) = i64::try_from(d_total) else {
            continue;
        };
        let total = total_i as f64;
        let elapsed = now.duration_since(at).as_secs_f64();
        if total >= 0.0 && elapsed > 0.0 {
            let pct = total / (elapsed * 10_000_000.0_f64) * 100.0;
            if pct.is_finite() {
                e.cpu_pct = Some(pct);
            }
        }
        guard.insert(e.pid, (ck, cu, now));
    }
}

/// (kernel, user) CPU time in 100 ns units for an open process handle.
fn cpu_times_100ns(handle: ffi::Handle) -> Option<(u64, u64)> {
    let mut creation = ffi::FileTime::default();
    let mut exit = ffi::FileTime::default();
    let mut kernel = ffi::FileTime::default();
    let mut user = ffi::FileTime::default();
    // SAFETY: pointer/size pairs to correctly sized locals.
    let ok =
        unsafe { ffi::GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    Some((kernel.to_u64(), user.to_u64()))
}

/// Working-set size in bytes for an open process handle (QLI is
/// sufficient for `GetProcessMemoryInfo` on supported Windows versions).
fn resident_bytes(handle: ffi::Handle) -> Option<u64> {
    let Ok(cb) = u32::try_from(std::mem::size_of::<ffi::ProcessMemoryCountersEx>()) else {
        return None;
    };
    let mut mc = ffi::ProcessMemoryCountersEx {
        cb,
        ..ffi::ProcessMemoryCountersEx::default()
    };
    // SAFETY: pointer/size pair to a local with cb set to the documented size.
    let ok = unsafe { ffi::K32GetProcessMemoryInfo(handle, &mut mc, mc.cb) };
    if ok == 0 {
        return None;
    }
    Some(mc.working_set_size as u64)
}

/// Count live threads owned by `pid` (system-wide thread snapshot; misses
/// threads that exit mid-enumeration — documented gauge semantics).
pub fn thread_count_of(pid: u32) -> Option<u32> {
    // SAFETY: plain call with documented flags.
    let snapshot = unsafe { ffi::CreateToolhelp32Snapshot(ffi::TH32CS_SNAPTHREAD, 0) };
    if snapshot == ffi::INVALID_HANDLE_VALUE {
        return None;
    }
    struct Snap(ffi::Handle);
    impl Drop for Snap {
        fn drop(&mut self) {
            // SAFETY: we own this handle.
            unsafe { ffi::CloseHandle(self.0) };
        }
    }
    let _snap = Snap(snapshot);

    let mut entry: ffi::ThreadEntry32 = unsafe { std::mem::zeroed() };
    entry.dw_size = u32::try_from(std::mem::size_of::<ffi::ThreadEntry32>()).unwrap_or(28);
    let mut count: u32 = 0;
    // SAFETY: snapshot owned; pointer to a correctly sized local.
    let mut ok = unsafe { ffi::Thread32First(snapshot, &mut entry) };
    while ok != 0 {
        if entry.th32_owner_process_id == pid {
            count = count.saturating_add(1);
        }
        // SAFETY: same as above.
        ok = unsafe { ffi::Thread32Next(snapshot, &mut entry) };
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_enumerable_with_metadata() {
        let list = processes(DEFAULT_ENUM_LIMIT).unwrap();
        assert!(!list.is_empty());
        let me = std::process::id();
        let myself = list.iter().find(|p| p.pid == me).expect("self listed");
        assert!(!myself.image_name.is_empty());
        assert!(myself.image_name.to_lowercase().ends_with(".exe"));
    }

    #[test]
    fn enumeration_is_bounded() {
        let list = processes(16).unwrap();
        assert!(list.len() <= 16);
    }

    #[test]
    fn cpu_pct_and_rss_appear_after_two_enumerations() {
        // Other tests in this binary enumerate concurrently and share the
        // process-global baseline, so a single two-call cycle can race.
        // Converge instead: within a few cycles the own pid must carry a
        // baseline and a delta (the loop itself burns CPU).
        let me = std::process::id();
        let mut pct = None;
        let mut rss = None;
        for _ in 0..10 {
            let list = processes(DEFAULT_ENUM_LIMIT).unwrap();
            if let Some(row) = list.iter().find(|p| p.pid == me) {
                rss = rss.or(row.rss_bytes);
                pct = pct.or(row.cpu_pct);
            }
            if pct.is_some() && rss.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        let rss = rss.expect("own rss present");
        assert!(rss > 0, "working set must be positive");
        let pct = pct.expect("own cpu_pct present within a few cycles");
        assert!((0.0..1000.0).contains(&pct), "sane one-core percent: {pct}");
    }

    #[test]
    fn self_thread_count_is_positive() {
        let n = thread_count_of(std::process::id()).expect("threads countable");
        assert!(n >= 1);
    }
}
