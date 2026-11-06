//! Windows symbolization (roadmap PF-902 core) via DbgHelp, with the
//! mandated serialization: **DbgHelp is single-threaded**, so every call
//! holds the process-wide worker lock (ADR-007).
//!
//! Scope of this slice: capture the *current process's* stack
//! (`RtlCaptureStackBackTrace`, documented) and resolve frames to
//! `module!function` using local PDBs only — no symbol servers, no network,
//! no elevation. Unresolved addresses keep `module + offset`; nothing is
//! ever invented.

#![allow(unsafe_code)]

use std::mem::size_of;
use std::sync::Mutex;

use pf_types::{ErrorCode, PfError};

use crate::ffi;

const MAX_SYM_NAME: usize = 2000;
const SYMOPT_UNDNAME: u32 = 0x0000_0002;
const SYMOPT_DEFERRED_LOADS: u32 = 0x0000_0004;
const SYMOPT_LOAD_LINES: u32 = 0x0000_0010;

/// SYMBOL_INFOW (variable-length tail) — x86-64 layout, compile-checked.
#[repr(C)]
struct SymbolInfoW {
    size_of_struct: u32,
    type_index: u32,
    reserved: [u64; 2],
    index: u32,
    size: u32,
    mod_base: u64,
    flags: u32,
    value: u64,
    address: u64,
    register: u32,
    scope: u32,
    tag: u32,
    name_len: u32,
    max_name_len: u32,
    name: [u16; 1],
}
const _: () = assert!(
    size_of::<SymbolInfoW>() == 88,
    "SYMBOL_INFOW head must be 88 bytes on x64"
);
const NAME_OFFSET: usize = 84; // offsetof(name)

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut ffi::Handle) -> i32;

    fn GetModuleFileNameW(module: ffi::Handle, filename: *mut u16, size: u32) -> u32;

    fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        back_trace: *mut *mut core::ffi::c_void,
        hash_value: *mut u64,
    ) -> u16;
}

const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

#[link(name = "dbghelp")]
unsafe extern "system" {
    fn SymInitializeW(
        process: ffi::Handle,
        user_search_path: *const u16,
        invade_process: i32,
    ) -> i32;

    fn SymSetOptions(sym_options: u32) -> u32;

    fn SymFromAddrW(
        process: ffi::Handle,
        address: u64,
        displacement: *mut u64,
        symbol: *mut SymbolInfoW,
    ) -> i32;

    fn SymCleanup(process: ffi::Handle) -> i32;
}

/// Map an address to its module's file name (e.g. `pf_windows-<hash>.exe`),
/// memoized per module base within the session.
fn module_name_of_address(address: u64) -> String {
    use std::collections::HashMap;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<usize, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));

    // Resolve the module handle from any address inside it.
    let mut module: ffi::Handle = std::ptr::null_mut();
    let addr_ptr = address as *const core::ffi::c_void as *const u16;
    // SAFETY: FROM_ADDRESS + UNCHANGED_REFCOUNT is the documented read-only
    // lookup pattern; the address is a live code address.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr_ptr,
            &mut module,
        )
    };
    if ok == 0 || module.is_null() {
        return String::new();
    }
    let key = module as usize;
    if let Some(name) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return name;
    }
    let mut buf = [0u16; 1024];
    // SAFETY: handle from the lookup above; buffer/size pair matches.
    let len = unsafe {
        GetModuleFileNameW(
            module,
            buf.as_mut_ptr(),
            u32::try_from(buf.len()).unwrap_or(1024),
        )
    };
    let name = if len == 0 {
        String::new()
    } else {
        let full = crate::ffi::widestr_to_string(&buf[..len as usize]);
        full.rsplit([0x5C as char, '/'])
            .next()
            .unwrap_or_default()
            .to_string()
    };
    if let Ok(mut c) = cache.lock() {
        c.insert(key, name.clone());
    }
    name
}

/// DbgHelp is single-process single-threaded: one global lock serializes
/// every symbol call in PulseForge. Held behind a `&'static` so session
/// guards can carry the `'static` lifetime.
static DBGHELP_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

fn dbghelp_lock() -> &'static Mutex<()> {
    DBGHELP_LOCK.get_or_init(|| Mutex::new(()))
}

/// One resolved frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolizedFrame {
    /// Module file name (e.g. `pulseforge.exe`) when known.
    pub module: String,
    /// Undecorated function name when resolved.
    pub function: String,
    /// Byte offset within the function.
    pub displacement: u64,
    /// Raw absolute address (kept — symbols attach later, ADR-007).
    pub address: u64,
}

/// Capture up to `max_frames` return addresses of the CURRENT thread's
/// stack (innermost first). Returns raw addresses; symbolization is a
/// separate, slower step.
pub fn capture_self_stack(max_frames: usize) -> Vec<u64> {
    let max = u32::try_from(max_frames.clamp(1, 128)).unwrap_or(32);
    let mut frames: Vec<*mut core::ffi::c_void> = vec![std::ptr::null_mut(); max as usize];
    // SAFETY: array/length pair matches; call is on the current thread.
    let captured =
        unsafe { RtlCaptureStackBackTrace(0, max, frames.as_mut_ptr(), std::ptr::null_mut()) };
    frames
        .iter()
        .take(captured as usize)
        .filter(|p| !p.is_null())
        .map(|p| *p as usize as u64)
        .collect()
}

/// A DbgHelp session bound to the current process. One at a time,
/// serialized by the global lock; `Drop` cleans up.
pub struct SymbolSession {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for SymbolSession {
    fn drop(&mut self) {
        // SAFETY: pseudo-handle; we initialized it in `open`.
        unsafe { SymCleanup(ffi::GetCurrentProcess()) };
    }
}

impl SymbolSession {
    /// Initialize with local symbol search only (no network, ever).
    pub fn open() -> Result<SymbolSession, PfError> {
        let guard = match dbghelp_lock().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Search path: the current executable's directory (our PDB sits
        // next to the binary in debug builds). Local files only — no
        // symbol servers, no network (ADR-007 / security model).
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let search_path: Vec<u16> = match &exe_dir {
            Some(dir) => dir
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect(),
            None => Vec::new(),
        };
        let path_ptr = if search_path.is_empty() {
            std::ptr::null()
        } else {
            search_path.as_ptr()
        };
        // SAFETY: pseudo-handle; NUL-terminated search path or NULL.
        let ok = unsafe { SymInitializeW(ffi::GetCurrentProcess(), path_ptr, 1) };
        if ok == 0 {
            return Err(PfError::new(
                ErrorCode::BackendFailure,
                "SymInitializeW failed; no local symbols will resolve",
            ));
        }
        // SAFETY: plain options call.
        unsafe { SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES) };
        Ok(SymbolSession { _guard: guard })
    }

    /// Resolve one address. Unresolved addresses return a frame with empty
    /// names and the raw address preserved — never a fabricated symbol.
    pub fn symbolize(&self, address: u64) -> SymbolizedFrame {
        // Fixed-size constants; no overflow at these magnitudes.
        #[allow(clippy::arithmetic_side_effects)]
        let buffer_len = size_of::<SymbolInfoW>() + (MAX_SYM_NAME + 16) * 2;
        let mut buffer = vec![0u8; buffer_len];
        let info: *mut SymbolInfoW = buffer.as_mut_ptr().cast();
        // SAFETY: buffer sized for the variable tail; all-headers written.
        unsafe {
            (*info).size_of_struct = u32::try_from(size_of::<SymbolInfoW>()).unwrap_or(88);
            (*info).max_name_len = u32::try_from(MAX_SYM_NAME).unwrap_or(2000);
        }
        let mut displacement: u64 = 0;
        // SAFETY: pointer/size pair as documented.
        let ok =
            unsafe { SymFromAddrW(ffi::GetCurrentProcess(), address, &mut displacement, info) };
        if ok == 0 {
            return SymbolizedFrame {
                module: String::new(),
                function: String::new(),
                displacement: 0,
                address,
            };
        }
        // SAFETY: reading the name written by DbgHelp within max_name_len.
        let (module, function) = unsafe {
            let name_ptr = buffer.as_ptr().add(NAME_OFFSET).cast::<u16>();
            let name_len = (*info).name_len as usize;
            let bounded = name_len.min(MAX_SYM_NAME);
            let mut name = String::new();
            for i in 0..bounded {
                let ch = *name_ptr.add(i);
                if ch == 0 {
                    break;
                }
                name.push(char::from_u32(u32::from(ch)).unwrap_or('?'));
            }
            // Export-table symbols come as "module!function"; PDB symbols
            // (Rust) come bare — map the module from the address instead.
            match name.split_once('!') {
                Some((m, f)) => (m.to_string(), f.to_string()),
                None => (module_name_of_address(address), name),
            }
        };
        SymbolizedFrame {
            module,
            function,
            displacement,
            address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_stack_capture_returns_frames() {
        let frames = capture_self_stack(32);
        assert!(!frames.is_empty(), "current stack has frames");
        assert!(
            frames.len() >= 4,
            "at least a few frames, got {}",
            frames.len()
        );
        assert!(frames.iter().all(|a| *a > 0));
    }

    #[test]
    fn symbolization_resolves_our_own_frames_with_local_pdb() {
        // Debug builds carry a PDB next to the test binary; frames from
        // this very call must resolve to real function names.
        let stack = capture_self_stack(32);
        let session = SymbolSession::open().expect("SymInitialize");
        let frames: Vec<SymbolizedFrame> = stack.iter().map(|a| session.symbolize(*a)).collect();
        let resolved = frames.iter().filter(|f| !f.function.is_empty()).count();
        assert!(
            resolved >= 2,
            "expected locally-resolvable frames (debug PDB), got {resolved}/{}: {:?}",
            frames.len(),
            frames
                .iter()
                .map(|f| f.function.clone())
                .collect::<Vec<_>>()
        );
        // Resolved names must contain this test's module prefix.
        let any_ours = frames.iter().any(|f| {
            f.module
                .to_lowercase()
                .replace("_", "-")
                .starts_with("pf-windows")
                || f.module.to_lowercase().contains("pf_windows")
        });
        assert!(any_ours, "our own module must appear: {frames:?}");
        // Unresolved frames keep their raw address and never invent names.
        for f in &frames {
            if f.function.is_empty() {
                assert_eq!(f.displacement, 0);
                assert!(f.address > 0);
            }
        }
    }

    #[test]
    fn garbage_addresses_do_not_panic_or_invent() {
        let session = SymbolSession::open().expect("SymInitialize");
        let f = session.symbolize(0x1);
        assert!(f.function.is_empty());
        assert_eq!(f.address, 0x1);
        let f = session.symbolize(u64::MAX);
        assert!(f.function.is_empty());
    }
}
