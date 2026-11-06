//! Raw Win32 declarations for the Windows backend (zero-dependency FFI).
//!
//! Every struct carries a compile-time size assertion pinned to the documented
//! x86-64 ABI so a layout mistake cannot reach runtime. Only the handful of
//! functions PulseForge needs are declared; signatures follow the current
//! Microsoft docs (see `docs/handoff/01_ARCHITECTURE.md` §21 references).

#![allow(unsafe_code)] // raw FFI is this module's purpose; see assertions below

use std::ffi::c_void;
use std::mem::size_of;

pub type Handle = *mut c_void;
pub type TraceHandle = u64;

// ---------------------------------------------------------------------------
// Structures (x86-64 layouts, compile-time checked)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileTime {
    pub low: u32,
    pub high: u32,
}
impl FileTime {
    pub fn to_u64(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}
const _: () = assert!(size_of::<FileTime>() == 8);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}
const _: () = assert!(size_of::<Guid>() == 16);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IoCounters {
    pub read_operation_count: u64,
    pub write_operation_count: u64,
    pub other_operation_count: u64,
    pub read_transfer_count: u64,
    pub write_transfer_count: u64,
    pub other_transfer_count: u64,
}
const _: () = assert!(size_of::<IoCounters>() == 48);

/// PROCESS_MEMORY_COUNTERS_EX (cb must be set to 80 on x64).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ProcessMemoryCountersEx {
    pub cb: u32,
    pub page_fault_count: u32,
    pub peak_working_set_size: usize,
    pub working_set_size: usize,
    pub quota_peak_paged_pool_usage: usize,
    pub quota_paged_pool_usage: usize,
    pub quota_peak_non_paged_pool_usage: usize,
    pub quota_non_paged_pool_usage: usize,
    pub pagefile_usage: usize,
    pub peak_pagefile_usage: usize,
    pub private_usage: usize,
}
const _: () = assert!(size_of::<ProcessMemoryCountersEx>() == 80);

#[repr(C)]
pub struct ProcessEntry32W {
    pub dw_size: u32,
    pub cnt_usage: u32,
    pub th32_process_id: u32,
    pub th32_default_heap_id: usize,
    pub th32_module_id: u32,
    pub cnt_threads: u32,
    pub th32_parent_process_id: u32,
    pub pc_pri_class_base: i32,
    pub dw_flags: u32,
    pub sz_exe_file: [u16; 260],
}
const _: () = assert!(size_of::<ProcessEntry32W>() == 568);

#[repr(C)]
pub struct ThreadEntry32 {
    pub dw_size: u32,
    pub cnt_usage: u32,
    pub th32_thread_id: u32,
    pub th32_owner_process_id: u32,
    pub tp_base_pri: i32,
    pub tp_delta_pri: i32,
    pub dw_flags: u32,
}
const _: () = assert!(size_of::<ThreadEntry32>() == 28);

#[repr(C)]
pub struct SystemInfo {
    pub processor_architecture: u16,
    pub reserved: u16,
    pub page_size: u32,
    pub minimum_application_address: *mut c_void,
    pub maximum_application_address: *mut c_void,
    pub active_processor_mask: usize,
    pub number_of_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u16,
    pub processor_level: u16,
    pub processor_revision: u16,
}
const _: () = assert!(size_of::<SystemInfo>() == 48);

/// WNODE_HEADER (48 bytes on x86-64).
#[repr(C)]
pub struct WnodeHeader {
    pub buffer_size: u32,
    pub provider_id: u32,
    pub historical_context: u32,
    pub time_stamp: i64,
    pub guid: Guid,
    pub client_context: u32,
    pub flags: u32,
}
const _: () = assert!(size_of::<WnodeHeader>() == 48);

/// EVENT_TRACE_PROPERTIES (112 bytes on x86-64). Logger/session-name string
/// buffers are appended after the struct by the caller.
#[repr(C)]
pub struct EventTraceProperties {
    pub wnode: WnodeHeader,
    pub buffer_size: u32,
    pub minimum_buffers: u32,
    pub maximum_buffers: u32,
    pub maximum_file_size: u32,
    pub log_file_mode: u32,
    pub flush_timer: u32,
    pub enable_flags: u32,
    pub age_control: u32, // union with union-padding on 32-bit; u32 on x64
    pub number_of_buffers: u32,
    pub free_buffers: u32,
    pub events_lost: u32,
    pub buffers_written: u32,
    pub log_buffers_lost: u32,
    pub real_time_buffers_lost: u32,
    pub logger_thread_id: isize,
}
const _: () = assert!(size_of::<EventTraceProperties>() == 112);

// ---------------------------------------------------------------------------
// kernel32
// ---------------------------------------------------------------------------

#[link(name = "kernel32")]
unsafe extern "system" {
    // OpenProcess has no A/W variant — the name is neutral.
    pub fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;

    pub fn CloseHandle(object: Handle) -> i32;

    pub fn GetProcessTimes(
        process: Handle,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;

    pub fn GetProcessIoCounters(process: Handle, io_counters: *mut IoCounters) -> i32;

    pub fn K32GetProcessMemoryInfo(
        process: Handle,
        mem_counters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> i32;

    pub fn K32GetProcessImageFileNameW(
        process: Handle,
        image_file_name: *mut u16,
        size: u32,
    ) -> u32;

    pub fn GetProcessHandleCount(process: Handle, handle_count: *mut u32) -> i32;

    pub fn GetCurrentProcess() -> Handle;

    pub fn GetLastError() -> u32;

    pub fn GetTickCount64() -> u64;

    pub fn GetSystemTimeAsFileTime(system_time_as_file_time: *mut FileTime);

    pub fn GetSystemInfo(system_info: *mut SystemInfo);

    pub fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;

    pub fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;

    pub fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;

    pub fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;

    pub fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;

    pub fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        message_id: u32,
        language_id: u32,
        buffer: *mut u16,
        size: u32,
        arguments: *const c_void,
    ) -> u32;
}

// ---------------------------------------------------------------------------
// advapi32 (ETW session control for the feasibility probe)
// ---------------------------------------------------------------------------

#[link(name = "advapi32")]
unsafe extern "system" {
    /// Read a registry value (used for the per-user light/dark theme
    /// toggle; `RRF_RT_REG_DWORD` restricts the accepted type).
    pub fn RegGetValueW(
        hkey: Handle,
        lp_sub_key: *const u16,
        lp_value: *const u16,
        dw_flags: u32,
        pdw_type: *mut u32,
        pv_data: *mut c_void,
        pcb_data: *mut u32,
    ) -> u32;

    pub fn StartTraceA(
        session_handle: *mut TraceHandle,
        session_name: *const u8,
        properties: *mut EventTraceProperties,
    ) -> u32;

    pub fn ControlTraceA(
        session_handle: TraceHandle,
        session_name: *const u8,
        properties: *mut EventTraceProperties,
        control_code: u32,
    ) -> u32;

    pub fn QueryAllTracesA(
        logger_array: *mut TraceHandle,
        logger_count: u32,
        sessions_count: *mut u32,
    ) -> u32;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
pub const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
pub const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

pub const ERROR_SUCCESS: u32 = 0;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_NO_SUCH_PRIVILEGE: u32 = 1313;
pub const ERROR_NOT_FOUND: u32 = 1168;
pub const ERROR_WMI_INSTANCE_NOT_FOUND: u32 = 4201;

/// EVENT_TRACE_REAL_TIME_MODE.
pub const EVENT_TRACE_REAL_TIME_MODE: u32 = 0x0000_0100;
/// WNODE_FLAG_TRACED_GUID.
pub const WNODE_FLAG_TRACED_GUID: u32 = 0x0002_0000;
/// EVENT_TRACE_CONTROL_STOP.
pub const EVENT_TRACE_CONTROL_STOP: u32 = 1;

pub const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_0800;
pub const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort system message for a Win32 error code (truncated, bounded).
pub fn win32_message(code: u32) -> String {
    if code == 0 {
        return "the operation completed successfully".into();
    }
    let mut buffer = [0u16; 512];
    // SAFETY: buffer/length match; no varargs.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0, // neutral language
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            std::ptr::null(),
        )
    };
    if len == 0 {
        return format!("win32 error {code}");
    }
    let text: String = buffer[..len as usize]
        .iter()
        .map(|&c| char::from_u32(u32::from(c)).unwrap_or('?'))
        .collect();
    text.trim().to_string()
}

/// Decode a NUL-terminated UTF-16 buffer into a String (lossy, bounded).
pub fn widestr_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// Toolhelp/remote-identity FFI (PF-901): process handle acquisition and
// bounded remote reads.
pub const PROCESS_VM_READ: u32 = 0x0010;
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;

unsafe extern "system" {
    pub fn ReadProcessMemory(
        process: Handle,
        base_address: *const c_void,
        buffer: *mut c_void,
        size: usize,
        read: *mut usize,
    ) -> i32;
}
