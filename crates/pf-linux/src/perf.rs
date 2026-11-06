//! Linux `perf_event_open` user-space sampling (roadmap P10: PF-1001/1002
//! bounded slice) — the profiler-on-profiler increment verified against a
//! known-hotspot fixture.
//!
//! Scope of this slice (honest):
//! - one event descriptor, self-process (`pid = 0`), any CPU (`cpu = -1`);
//! - `PERF_SAMPLE_IP | TID | TIME | CALLCHAIN` — user-space callchains only
//!   (`perf_event_paranoid = 2` excludes kernel frames by policy; PulseForge
//!   never changes kernel policy);
//! - mmap ring buffer with the standard metadata page (data_head/data_tail),
//!   bounded reads, lost-record accounting (PERF_RECORD_LOST is counted and
//!   surfaced, never normalized away);
//! - event probing: hardware CPU-cycles first, software CPU-clock fallback
//!   (WSL2 kernels commonly lack a working hardware PMU — the probe reports
//!   which source served the samples);
//! - no symbolization here (PF-902 is the Windows twin; Linux ELF/DWARF
//!   symbolization is the remaining P10 slice after this).

#![allow(unsafe_code, clippy::arithmetic_side_effects)]

use std::sync::atomic::{AtomicBool, Ordering};

use pf_types::{ErrorCode, PfError};

// ---------------------------------------------------------------------------
// Constants (linux/perf_event.h, x86-64)
// ---------------------------------------------------------------------------

const SYS_PERF_EVENT_OPEN: i64 = 298; // x86-64 syscall number
const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;

const PERF_SAMPLE_IP: u64 = 1 << 0;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;

const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_LOST: u32 = 2;

const PERF_EVENT_IOC_ENABLE: u64 = 0x2400; // _IO('$', 0)
const PERF_EVENT_IOC_DISABLE: u64 = 0x2401; // _IO('$', 1)

const PAGE_SIZE: usize = 4096;
const RING_PAGES: usize = 64; // 256 KiB sample buffer
const MAX_CALLCHAIN_FRAMES: usize = 64;

/// perf_event_attr — the fields PulseForge sets, laid out exactly as the
/// kernel expects on x86-64 (compile-checked sizes below).
/// NOTE: the kernel layout is `type` at offset 0 and `size` at offset 4 —
/// the opposite of the Windows "size-first" convention. Verified against
/// linux/perf_event.h offsets (type@0, size@4) and by strace.
#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period_or_freq: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    bp_addr: u64,
    bp_len: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    __reserved_2: u16,
    __reserved_3: u16,
    // @112: union { u64 aux_sample_size; u32 __reserved_4; } — 8 bytes.
    aux_sample_size: u64,
    __reserved_5: u32,
    __pad: u32,
    // @128: union { u64 sig_data; ... }.
    sig_data: u64,
}
const _: () = assert!(std::mem::size_of::<PerfEventAttr>() == 136);

#[repr(C)]
struct PerfEventMmapPage {
    // Only the fields PulseForge reads; offsets are fixed by the ABI:
    // data_head @ 0, data_tail @ 8, data_offset @ 16, data_size @ 24.
    _header: [u8; 0], // (field placement via explicit offsets below)
}
const _: () = assert!(std::mem::size_of::<PerfEventMmapPage>() == 0);

unsafe extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn __errno_location() -> *mut i32;
    fn ioctl(fd: i32, request: u64, ...) -> i64;
    fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, length: usize) -> i32;
    fn close(fd: i32) -> i32;
}

const PROT_READ_WRITE: i32 = 0b11;
const MAP_SHARED: i32 = 0x01;
const MAP_FAILED: *mut u8 = usize::MAX as *mut u8;

#[allow(clippy::cast_possible_truncation)]
fn perf_event_open(attr: &PerfEventAttr, pid: i32, cpu: i32, group_fd: i32) -> Result<i32, u32> {
    let rc = unsafe {
        syscall(
            SYS_PERF_EVENT_OPEN,
            attr as *const PerfEventAttr as u64,
            i64::from(pid),
            i64::from(cpu),
            i64::from(group_fd),
            0u64,
        )
    };
    if rc < 0 {
        // SAFETY: __errno_location is thread-local and always valid.
        let errno = unsafe { *__errno_location() };
        Err(u32::try_from(errno).unwrap_or(u32::MAX))
    } else {
        Ok(rc as i32)
    }
}

fn build_attr(event_type: u32, config: u64, sample_period: u64) -> PerfEventAttr {
    let mut attr = PerfEventAttr {
        type_: event_type,
        size: u32::try_from(std::mem::size_of::<PerfEventAttr>()).unwrap_or(144),
        config,
        sample_period_or_freq: sample_period,
        sample_type: PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CALLCHAIN,
        read_format: 0,
        flags: 0,
        wakeup_events: 1,
        bp_type: 0,
        bp_addr: 0,
        bp_len: 0,
        branch_sample_type: 0,
        sample_regs_user: 0,
        sample_stack_user: 0,
        clockid: 0,
        sample_regs_intr: 0,
        aux_watermark: 0,
        __reserved_2: 0,
        __reserved_3: 0,
        aux_sample_size: 0,
        __reserved_5: 0,
        __pad: 0,
        sig_data: 0,
    };
    attr.flags |= 1 << 0; // disabled at open; enabled via ioctl
    attr
}

/// One parsed perf sample.
#[derive(Clone, Copy, Debug)]
pub struct PerfSample {
    pub time_ns: u64,
    pub tid: u32,
    /// CPU the sample was taken on (PERF_SAMPLE_CPU).
    pub cpu: u32,
    /// Callchain IPs, innermost (current IP) first. User-space only under
    /// paranoid=2.
    pub ips: [u64; MAX_CALLCHAIN_FRAMES],
    pub ip_count: usize,
}

/// Which event source served the samples (probed; reported verbatim).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventSource {
    HardwareCycles,
    SoftwareCpuClock,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSource::HardwareCycles => "hw-cpu-cycles",
            EventSource::SoftwareCpuClock => "sw-cpu-clock",
        }
    }
}

/// A closed sampling session: open → mmap → (caller waits) → collect.
struct MappedEvent {
    fd: i32,
    base: *mut u8,
    map_len: usize,
    data_offset: usize,
    data_size: usize,
}

impl MappedEvent {
    fn open_for_pid(
        event_type: u32,
        config: u64,
        sample_period: u64,
        pid: i32,
    ) -> Result<MappedEvent, PfError> {
        let attr = build_attr(event_type, config, sample_period);
        let fd = perf_event_open(&attr, pid, -1, -1).map_err(|err| {
            PfError::new(
                ErrorCode::CapabilityUnavailable,
                format!(
                    "perf_event_open failed (errno {err}): this kernel/policy \
                     rejects the event"
                ),
            )
        })?;
        let map_len = (1 + RING_PAGES) * PAGE_SIZE;
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                map_len,
                PROT_READ_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };
        if base == MAP_FAILED || base.is_null() {
            unsafe { close(fd) };
            return Err(PfError::new(
                ErrorCode::CapabilityUnavailable,
                "perf mmap failed",
            ));
        }
        // Ring layout (non-AUX): data_head @1024, data_tail @1032 (both
        // u64 in the metadata page), data area starts at the page boundary
        // (4096) and spans mmap_len - 4096 bytes.
        Ok(MappedEvent {
            fd,
            base,
            map_len,
            data_offset: PAGE_SIZE,
            data_size: map_len - PAGE_SIZE,
        })
    }

    fn enable(&self) {
        unsafe { ioctl(self.fd, PERF_EVENT_IOC_ENABLE, 0) };
    }

    fn disable(&self) {
        unsafe { ioctl(self.fd, PERF_EVENT_IOC_DISABLE, 0) };
    }

    /// Read data_head (kernel-owned write cursor) — offset 1024 in the
    /// metadata page (offset 0 is version/compat/lock/index/time fields).
    pub(crate) fn data_head(&self) -> u64 {
        unsafe {
            let p = self.base.add(1024) as *const u64;
            p.read_volatile()
        }
    }

    /// Publish data_tail (user-owned read cursor) — offset 1032 in the
    /// metadata page.
    fn publish_tail(&self, tail: u64) {
        unsafe {
            let p = self.base.add(1032) as *mut u64;
            p.write_volatile(tail);
        }
    }

    /// Copy the unconsumed ring region into a caller-owned buffer (bounded;
    /// the buffer is resized to the available byte count). Reuses the
    /// sampler's scratch allocation across ticks.
    // Ring offsets are 64-bit; x86-64 usize is 64-bit — no truncation.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn drain_into(&self, tail: u64, head: u64, buf: &mut Vec<u8>) {
        let avail = (head - tail) as usize;
        let capped = avail.min(self.data_size);
        buf.clear();
        buf.resize(capped, 0);
        let start = self.data_offset + (tail % self.data_size as u64) as usize;
        let first = capped.min(self.data_size - (start - self.data_offset));
        unsafe {
            let src = self
                .base
                .add(self.data_offset + (tail % self.data_size as u64) as usize);
            std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), first);
            if capped > first {
                let src2 = self.base.add(self.data_offset);
                std::ptr::copy_nonoverlapping(src2, buf.as_mut_ptr().add(first), capped - first);
            }
        }
    }

    fn close(&mut self) {
        unsafe {
            munmap(self.base, self.map_len);
            close(self.fd);
        }
    }
}

/// One perf sampling session over the current process.
pub struct PerfSampler {
    event: MappedEvent,
    pub source: EventSource,
    pub lost_records: u64,
    /// Kernel-space callchain frames filtered during parse (evidence the
    /// user-space-only contract is enforced, not assumed).
    pub kernel_frames_skipped: u64,
    tail: u64,
    done: AtomicBool,
    /// Reused ring-drain scratch (allocated once, resized per tick).
    scratch: Vec<u8>,
}

impl PerfSampler {
    /// Open a sampler for the current process: hardware CPU-cycles first,
    /// software CPU-clock fallback. Sample period is in event units (ns for
    /// the clock event).
    pub fn open(sample_period: u64) -> Result<PerfSampler, PfError> {
        Self::open_for_pid(0, sample_period)
    }

    /// Open a sampler for a specific pid (0 = calling process/thread
    /// set). Under perf_event_paranoid=2 this requires the same user —
    /// which same-user children and fixtures are.
    pub fn open_for_pid(pid: i32, sample_period: u64) -> Result<PerfSampler, PfError> {
        // Hardware first (better attribution); WSL2 kernels commonly reject
        // it — fall back to the always-available software clock.
        match MappedEvent::open_for_pid(
            PERF_TYPE_HARDWARE,
            PERF_COUNT_HW_CPU_CYCLES,
            sample_period,
            pid,
        ) {
            Ok(event) => Ok(PerfSampler {
                event,
                source: EventSource::HardwareCycles,
                lost_records: 0,
                kernel_frames_skipped: 0,
                tail: 0,
                done: AtomicBool::new(false),
                scratch: Vec::new(),
            }),
            Err(_) => {
                let event = MappedEvent::open_for_pid(
                    PERF_TYPE_SOFTWARE,
                    PERF_COUNT_SW_CPU_CLOCK,
                    sample_period,
                    pid,
                )?;
                Ok(PerfSampler {
                    event,
                    source: EventSource::SoftwareCpuClock,
                    lost_records: 0,
                    kernel_frames_skipped: 0,
                    tail: 0,
                    done: AtomicBool::new(false),
                    scratch: Vec::new(),
                })
            }
        }
    }

    pub fn start(&self) {
        self.event.enable();
    }

    pub fn stop(&mut self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            self.event.disable();
        }
    }

    /// Diagnostics: (data_head, user tail, data_offset, data_size).
    pub fn debug_ring(&self) -> (u64, u64, usize, usize) {
        (
            self.event.data_head(),
            self.tail,
            self.event.data_offset,
            self.event.data_size,
        )
    }

    /// Drain the ring and parse samples + lost-record accounting. The
    /// drain scratch and the sample vec's backing memory are reused across
    /// ticks; parsing is delegated to the pure [`parse_ring`].
    // Ring offsets/parses bounded by data_size and record headers.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::collapsible_if,
        clippy::collapsible_match
    )]
    pub fn collect(&mut self) -> Vec<PerfSample> {
        let head = self.event.data_head();
        if head == self.tail {
            return Vec::new();
        }
        self.event.drain_into(self.tail, head, &mut self.scratch);
        self.event.publish_tail(head);
        self.tail = head;

        let mut samples = Vec::with_capacity(self.scratch.len() / 64);
        let mut lost = 0u64;
        let mut kernel_skipped = 0u64;
        parse_ring(
            &self.scratch,
            &mut samples,
            &mut lost,
            &mut kernel_skipped,
            pf_perf_debug(),
        );
        self.lost_records += lost;
        self.kernel_frames_skipped += kernel_skipped;
        samples
    }
}

impl Drop for PerfSampler {
    fn drop(&mut self) {
        self.stop();
        self.event.close();
    }
}

/// PF_PERF_DEBUG is read ONCE per process: collect() runs every drain
/// tick, and env lookups take a global lock each call.
fn pf_perf_debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("PF_PERF_DEBUG").is_some())
}

/// Single-bounds-check little-endian readers for the record parser (the
/// per-byte `from_le_bytes` form bounds-checks every byte; these check
/// once per read and make truncated records return None instead of
/// panicking on a malformed ring).
#[inline]
fn rd_u64(buf: &[u8], p: usize) -> Option<u64> {
    let s = buf.get(p..p + 8)?;
    Some(u64::from_le_bytes(s.try_into().expect("8-byte slice")))
}

#[inline]
fn rd_u32(buf: &[u8], p: usize) -> Option<u32> {
    let s = buf.get(p..p + 4)?;
    Some(u32::from_le_bytes(s.try_into().expect("4-byte slice")))
}

/// Parse one drained ring region into samples, folding LOST and
/// kernel-frame-skip counters. Pure over `buf` — testable and
/// benchmarkable without a live perf event (public for the hostile-input
/// fuzz suite; bounded in, bounded out). `out` is appended to (callers
/// reuse capacity). Bounded: records are walked by their own header sizes;
/// a truncated tail keeps what parsed.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn parse_ring(
    buf: &[u8],
    out: &mut Vec<PerfSample>,
    lost: &mut u64,
    kernel_skipped: &mut u64,
    debug: bool,
) {
    let mut pos = 0usize;
    let mut dbg_records: Vec<(u32, usize)> = Vec::new();
    while pos + 8 <= buf.len() {
        let rec_type = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        // perf_event_header: u32 type, u16 misc, u16 size — size is at
        // pos+6 (the earlier pos+4 read was the misc field, which made
        // every record parse as size=2).
        let size = u16::from_le_bytes([buf[pos + 6], buf[pos + 7]]) as usize;
        if debug && dbg_records.len() < 4 {
            dbg_records.push((rec_type, size));
        }
        if size < 8 || pos + size > buf.len() {
            break; // truncated tail: keep what parsed (bounded read)
        }
        match rec_type {
            PERF_RECORD_SAMPLE => {
                let mut p = pos + 8;
                let mut sample = PerfSample {
                    time_ns: 0,
                    tid: 0,
                    cpu: 0,
                    ips: [0u64; MAX_CALLCHAIN_FRAMES],
                    ip_count: 0,
                };
                let mut complete = true;
                if sample_type_has(PERF_SAMPLE_IP) {
                    match rd_u64(buf, p) {
                        Some(ip) => {
                            p += 8;
                            let _ = ip; // the user-space chain below supersedes it
                        }
                        None => complete = false,
                    }
                }
                if complete && sample_type_has(PERF_SAMPLE_TID) {
                    // PERF_SAMPLE_TID packs pid (low u32) then tid (high
                    // u32) on little-endian — the THREAD id is the second
                    // field.
                    match (rd_u32(buf, p), rd_u32(buf, p + 4)) {
                        (Some(_pid), Some(tid)) => {
                            p += 8;
                            sample.tid = tid;
                        }
                        _ => complete = false,
                    }
                }
                if complete && sample_type_has(PERF_SAMPLE_TIME) {
                    match rd_u64(buf, p) {
                        Some(t) => {
                            p += 8;
                            sample.time_ns = t;
                        }
                        None => complete = false,
                    }
                }
                if complete && sample_type_has(PERF_SAMPLE_CALLCHAIN) {
                    match rd_u64(buf, p) {
                        Some(nr_raw) => {
                            p += 8;
                            let nr = (nr_raw as usize)
                                .min(MAX_CALLCHAIN_FRAMES)
                                .min((buf.len() - p) / 8);
                            for _i in 0..nr {
                                let Some(ip) = rd_u64(buf, p) else {
                                    complete = false;
                                    break;
                                };
                                p += 8;
                                // Kernel-space frames (x86-64: >= 0xffff8000_...)
                                // are filtered with explicit accounting — under
                                // paranoid=2 + exclude_kernel they should not
                                // appear, but sentinels can; never silently
                                // mixed into user-space callchains.
                                if ip >= 0xffff_8000_0000_0000 {
                                    *kernel_skipped += 1;
                                } else if sample.ip_count < MAX_CALLCHAIN_FRAMES {
                                    sample.ips[sample.ip_count] = ip;
                                    sample.ip_count += 1;
                                }
                            }
                        }
                        None => complete = false,
                    }
                }
                if !complete {
                    break; // truncated record: keep what parsed
                }
                out.push(sample);
            }
            PERF_RECORD_LOST => {
                // header + 2 u64 (count, id)
                if let Some(count) = rd_u64(buf, pos + 8) {
                    *lost += count;
                }
            }
            _ => {}
        }
        pos += size;
    }
    if debug {
        eprintln!(
            "perf parse: records={dbg_records:?} buf_len={} samples={}",
            buf.len(),
            out.len()
        );
    }
}

fn sample_type_has(bit: u64) -> bool {
    // The sampler always sets IP|TID|TIME|CALLCHAIN.
    bit & (PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CALLCHAIN) != 0
}

/// Probe whether perf sampling is available at all (open+close, no mmap).
pub fn probe() -> Result<EventSource, PfError> {
    let attr = build_attr(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 1_000_000);
    let hw_errno = match perf_event_open(&attr, 0, -1, -1) {
        Ok(fd) => {
            unsafe { close(fd) };
            return Ok(EventSource::HardwareCycles);
        }
        Err(errno) => errno,
    };
    let attr = build_attr(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, 1_000_000);
    let sw_errno = match perf_event_open(&attr, 0, -1, -1) {
        Ok(fd) => {
            unsafe { close(fd) };
            return Ok(EventSource::SoftwareCpuClock);
        }
        Err(errno) => errno,
    };
    Err(PfError::new(
        ErrorCode::CapabilityUnavailable,
        format!(
            "perf_event_open rejected: hw-cycles errno {hw_errno}, \
             sw-cpu-clock errno {sw_errno} — kernel policy or WSL2 limitation"
        ),
    )
    .with_remediation(
        "sampling requires perf_event_paranoid <= 2 and kernel PERF_EVENTS support; \
         PulseForge never changes kernel policy",
    ))
}

/// Maximum threads one process sampler covers (bounded fds and rings).
pub const MAX_THREADS: usize = 32;

/// Whole-process sampling (PF-1003): `perf_event_open(pid=tgid)` measures
/// only the group-leader thread, so a process sampler owns ONE event per
/// discovered thread (bounded by [`MAX_THREADS`]) and merges their rings.
/// Thread lifecycle is covered by [`ProcessSampler::rescan`]: threads born
/// after open are picked up on the next scan; threads that exited are
/// drained and dropped. Threads shorter than one rescan interval may be
/// missed — reported honestly via the open-failure counter, never
/// fabricated.
pub struct ProcessSampler {
    pid: i32,
    period: u64,
    /// Probed event source (from the first successfully opened thread).
    pub source: EventSource,
    children: Vec<Child>,
    consumed_lost: Vec<u64>,
    consumed_kernel: Vec<u64>,
    /// Cumulative LOST records across all threads.
    pub lost_records: u64,
    /// Cumulative kernel-frame skips across all threads.
    pub kernel_frames_skipped: u64,
    /// Threads seen in /proc but not sampled (open refused, or the
    /// [`MAX_THREADS`] bound reached) — honest coverage accounting.
    pub threads_unsampled: u64,
}

struct Child {
    tid: u32,
    sampler: PerfSampler,
}

// Child offset arithmetic is index-bounded by construction; tid casts
// are bounded by /proc's u32 pid space.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
impl ProcessSampler {
    /// Open per-thread samplers for every thread currently in `pid`'s
    /// process (bounded). Fails only when NO thread can be sampled.
    pub fn open_for_process(pid: i32, sample_period: u64) -> Result<ProcessSampler, PfError> {
        let tids = list_task_tids(pid);
        if tids.is_empty() {
            return Err(pf_types::PfError::new(
                pf_types::ErrorCode::TargetExited,
                format!("no threads found for pid {pid} (target already exited?)"),
            ));
        }
        let mut out = ProcessSampler {
            pid,
            period: sample_period,
            source: EventSource::SoftwareCpuClock,
            children: Vec::new(),
            consumed_lost: Vec::new(),
            consumed_kernel: Vec::new(),
            lost_records: 0,
            kernel_frames_skipped: 0,
            threads_unsampled: 0,
        };
        for tid in tids.iter().take(MAX_THREADS) {
            out.open_child(*tid);
        }
        out.threads_unsampled += u64::from(tids.len().saturating_sub(MAX_THREADS) as u32);
        if let Some(first) = out.children.first() {
            out.source = first.sampler.source;
        }
        if out.children.is_empty() {
            return Err(pf_types::PfError::new(
                pf_types::ErrorCode::CapabilityUnavailable,
                "no thread of the target could be opened for sampling                  (perf_event_paranoid or permission refused)",
            ));
        }
        Ok(out)
    }

    fn open_child(&mut self, tid: u32) {
        match PerfSampler::open_for_pid(tid as i32, self.period) {
            Ok(s) => {
                self.children.push(Child { tid, sampler: s });
                self.consumed_lost.push(0);
                self.consumed_kernel.push(0);
            }
            Err(_) => self.threads_unsampled = self.threads_unsampled.saturating_add(1),
        }
    }

    /// Enable every thread's event.
    pub fn start(&self) {
        for c in &self.children {
            c.sampler.start();
        }
    }

    /// Disable every thread's event.
    pub fn stop(&mut self) {
        for c in self.children.iter_mut() {
            c.sampler.stop();
        }
    }

    /// Re-scan /proc for thread lifecycle: new threads are opened and
    /// started (bounded), exited threads are drained and dropped. Call
    /// periodically (~1 s); threads born and gone between scans are missed
    /// and counted honestly if they were observed at all.
    pub fn rescan(&mut self) {
        let tids = list_task_tids(self.pid);
        // Drain+drop exited threads, folding their final counter deltas.
        let mut i = 0;
        while i < self.children.len() {
            let tid = self.children[i].tid;
            if !tids.contains(&tid) {
                let mut child = self.children.swap_remove(i);
                let cl = self.consumed_lost.swap_remove(i);
                let ck = self.consumed_kernel.swap_remove(i);
                let dl = child.sampler.lost_records.saturating_sub(cl);
                let dk = child.sampler.kernel_frames_skipped.saturating_sub(ck);
                let _ = child.sampler.collect(); // drain the dying ring
                self.lost_records = self.lost_records.saturating_add(dl);
                self.kernel_frames_skipped = self.kernel_frames_skipped.saturating_add(dk);
            } else {
                i += 1;
            }
        }
        // Open unseen threads (bounded).
        for tid in &tids {
            if self.children.len() >= MAX_THREADS {
                break;
            }
            if self.children.iter().any(|c| c.tid == *tid) {
                continue;
            }
            self.open_child(*tid);
            if let Some(c) = self.children.last_mut() {
                if c.tid == *tid {
                    c.sampler.start();
                }
            }
        }
        self.threads_unsampled = self
            .threads_unsampled
            .saturating_add(u64::from(tids.len().saturating_sub(MAX_THREADS) as u32));
    }

    /// Merge the rings of every thread into one sample batch and fold the
    /// counter deltas into the cumulative process totals.
    pub fn collect(&mut self) -> Vec<PerfSample> {
        let mut out = Vec::new();
        for i in 0..self.children.len() {
            let batch = self.children[i].sampler.collect();
            let child_lost = self.children[i].sampler.lost_records;
            let child_kernel = self.children[i].sampler.kernel_frames_skipped;
            let dl = child_lost.saturating_sub(self.consumed_lost[i]);
            let dk = child_kernel.saturating_sub(self.consumed_kernel[i]);
            self.consumed_lost[i] = child_lost;
            self.consumed_kernel[i] = child_kernel;
            self.lost_records = self.lost_records.saturating_add(dl);
            self.kernel_frames_skipped = self.kernel_frames_skipped.saturating_add(dk);
            out.extend(batch);
        }
        out
    }

    /// Number of threads currently sampled.
    pub fn threads_sampled(&self) -> usize {
        self.children.len()
    }
}

/// List the thread ids of a process from /proc (bounded read_dir).
fn list_task_tids(pid: i32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return out;
    };
    for e in entries.flatten() {
        if out.len() >= MAX_THREADS * 2 {
            break; // bounded scan
        }
        if let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() {
            out.push(tid);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one SAMPLE record exactly as the kernel would for the
    /// sampler's sample_type (IP|TID|TIME|CALLCHAIN).
    fn encode_sample(ips: &[u64], tid: u32, time_ns: u64, out: &mut Vec<u8>) {
        let body = 8 + 8 + 8 + 8 + ips.len() * 8; // ip + pid/tid + time + nr + ips
        let size = 8u16 + u16::try_from(body).unwrap_or(u16::MAX);
        out.extend_from_slice(&PERF_RECORD_SAMPLE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // misc
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&0x400000u64.to_le_bytes()); // IP
        out.extend_from_slice(&4242u32.to_le_bytes()); // pid
        out.extend_from_slice(&tid.to_le_bytes());
        out.extend_from_slice(&time_ns.to_le_bytes());
        out.extend_from_slice(&(ips.len() as u64).to_le_bytes()); // nr
        for ip in ips {
            out.extend_from_slice(&ip.to_le_bytes());
        }
    }

    #[test]
    fn parse_ring_decodes_samples_and_counts_kernel_frames() {
        let mut buf = Vec::new();
        encode_sample(&[0x1000, 0x2000, 0xffff_8000_dead_beef], 7, 1_000, &mut buf);
        encode_sample(&[0x3000], 8, 2_000, &mut buf);
        // A LOST record: header + count + id.
        buf.extend_from_slice(&PERF_RECORD_LOST.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&24u16.to_le_bytes());
        buf.extend_from_slice(&5u64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());

        let mut samples = Vec::new();
        let mut lost = 0u64;
        let mut kern = 0u64;
        parse_ring(&buf, &mut samples, &mut lost, &mut kern, false);

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].tid, 7);
        assert_eq!(samples[0].time_ns, 1_000);
        assert_eq!(samples[0].ip_count, 2, "kernel frame filtered");
        assert_eq!(samples[0].ips[0], 0x1000);
        assert_eq!(samples[0].ips[1], 0x2000);
        assert_eq!(samples[1].tid, 8);
        assert_eq!(kern, 1, "kernel skips counted, never hidden");
        assert_eq!(lost, 5);
    }

    #[test]
    fn parse_ring_survives_truncated_and_malformed_records() {
        // A record whose header claims more bytes than the buffer holds —
        // the old byte-indexing parser would PANIC here; the slice reader
        // must keep what parsed instead.
        let mut buf = Vec::new();
        encode_sample(&[0x1000], 7, 1_000, &mut buf);
        buf.truncate(buf.len() - 8); // cut the callchain tail off
        let mut samples = Vec::new();
        let mut lost = 0u64;
        let mut kern = 0u64;
        parse_ring(&buf, &mut samples, &mut lost, &mut kern, false);
        assert!(samples.is_empty(), "truncated sample is not fabricated");
    }

    /// Timing bench (roadmap §16: medians over repeats, never single-run
    /// gates): parse throughput over a synthetic 18k-sample ring region.
    #[test]
    #[ignore = "timing bench: run with `cargo test -p pf-linux parse_ring_throughput -- --ignored --nocapture`"]
    fn parse_ring_throughput() {
        let mut buf = Vec::with_capacity(18_000 * 200);
        for i in 0..18_000u64 {
            let base = 0x400000 + (i % 256) * 0x40;
            let ips: Vec<u64> = (0..20).map(|d| base + d * 0x10).collect();
            encode_sample(&ips, (i % 4) as u32, i * 250_000, &mut buf);
        }
        let mut samples = Vec::new();
        let mut lost = 0u64;
        let mut kern = 0u64;
        let mut medians = Vec::new();
        for _ in 0..5 {
            samples.clear();
            let t0 = std::time::Instant::now();
            parse_ring(&buf, &mut samples, &mut lost, &mut kern, false);
            medians.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        medians.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        eprintln!(
            "parse_ring: {} bytes, {} samples, median {:.2} ms ({:.0}k samples/s)",
            buf.len(),
            samples.len(),
            medians[2],
            samples.len() as f64 / (medians[2] / 1000.0) / 1000.0
        );
        assert_eq!(samples.len(), 18_000);
    }
}
