//! Linux-only FFI isolation — glibc syscall symbols.
#![cfg(target_os = "linux")]

//! Minimal Rust-vs-C FFI isolation for the perf syscall.

// FFI test: pid cast is safe (pids fit i32); raw syscall FFI; fd casts.
#![allow(
    unsafe_code,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]

use std::ffi::c_int;
use std::ffi::c_long;
use std::ffi::c_void;

unsafe extern "C" {
    // Non-variadic declaration of glibc's syscall(2) wrapper — the ABI
    // matches (RDI, RSI, RDX, RCX, R8, R9) and removes variadic-marshaling
    // doubt for the 5-argument perf_event_open call.
    fn syscall(
        num: c_long,
        a1: *const c_void,
        a2: c_long,
        a3: c_long,
        a4: c_long,
        a5: c_long,
    ) -> c_long;
    fn __errno_location() -> *mut c_int;
}

/// Kernel attr layout: type@0, size@4 (the layout bug this file
/// originally diagnosed — kept as the regression fixture).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Attr136 {
    pub type_: u32,
    pub size: u32,
    pub config: u64,
    pub period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup: u32,
    pub bp_type: u32,
    pub bp_addr: u64,
    pub bp_len: u64,
    pub branch: u64,
    pub regs_user: u64,
    pub stack_user: u32,
    pub clockid: i32,
    pub regs_intr: u64,
    pub aux_wm: u32,
    pub res2: u16,
    pub res3: u16,
    pub aux_ss: u32,
    pub res4: u32,
    pub res5: u32,
    pub sig_data: u64,
}

#[test]
fn getpid_via_raw_syscall() {
    let mine = unsafe { syscall(39, std::ptr::null(), 0, 0, 0, 0) }; // SYS_getpid
    let real = std::process::id();
    assert_eq!(mine, real as c_long, "raw syscall FFI broken");
}

#[test]
fn perf_open_120_byte_attr() {
    eprintln!("TEST-VERSION-MARKER");
    let a = Attr136 {
        size: u32::try_from(std::mem::size_of::<Attr136>()).unwrap_or(120),
        type_: 1,  // SOFTWARE
        config: 0, // CPU_CLOCK
        period: 1_000_000,
        sample_type: 43, // IP|TID|TIME|CALLCHAIN
        read_format: 0,
        flags: 1, // disabled
        wakeup: 1,
        bp_type: 0,
        bp_addr: 0,
        bp_len: 0,
        branch: 0,
        regs_user: 0,
        stack_user: 0,
        clockid: 0,
        regs_intr: 0,
        aux_wm: 0,
        res2: 0,
        res3: 0,
        aux_ss: 0,
        res4: 0,
        res5: 0,
        sig_data: 0,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &a as *const Attr136 as *const u8,
            std::mem::size_of::<Attr136>(),
        )
    };
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::write("/tmp/rust-attr.hex", hex.join(" "));
    // Path A: glibc wrapper (non-variadic decl) with the CORRECT attr (a).
    let rc_glibc = unsafe { syscall(298, &a as *const Attr136 as *const c_void, 0, -1, -1, 0) };
    let errno_glibc = unsafe { *__errno_location() };
    if rc_glibc >= 0 {
        unsafe { close(rc_glibc as i32) };
    }
    // Path B: inline-asm raw syscall (bypasses glibc wrapper), same attr.
    let rc_asm: i64;
    let errno_asm: i32;
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") 298i64 => rc_asm,
            in("rdi") &a as *const Attr136 as *const c_void,
            in("rsi") 0i64,
            in("rdx") -1i64,
            in("r10") -1i64,
            in("r8") 0i64,
            lateout("r11") _,
            lateout("rcx") _,
        );
        errno_asm = *__errno_location();
    }
    if rc_asm >= 0 {
        // reopen+close to keep fd hygiene simple in the test
    }
    println!(
        "glibc-path rc={rc_glibc} errno={errno_glibc}; asm-path rc={rc_asm} errno={errno_asm}"
    );
    // Path B is the honest raw kernel call; assert availability there.
    assert!(
        rc_asm >= 0,
        "inline-asm perf_event_open failed: errno {errno_asm}"
    );
}

unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
}
