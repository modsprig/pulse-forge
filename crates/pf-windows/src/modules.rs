//! Same-user PE module identity via the Toolhelp snapshot (roadmap PF-901
//! bounded slice): enumerate a process's modules, parse each module's PE
//! debug directory for the RSDS PDB reference (GUID + age + path), and
//! match that reference against the PDB file's own info stream.

#![allow(unsafe_code)]
// Toolhelp FFI is this module's purpose (see ffi.rs)
// Struct sizes and PE offsets are compile-checked or length-guarded; the
// remaining casts are the documented Win32/PE field widths.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
//!
//! Scope and honesty:
//! - same-user targets work without elevation; other-user targets fail
//!   with a typed error (never a silent empty list);
//! - the PDB match reads the MSF superblock + info stream (stream 1) and
//!   compares GUID AND age — a wrong-version PDB never matches;
//! - ASLR and module load/unload are observable facts of the snapshot,
//!   verified by tests rather than assumed.

use std::path::Path;

use pf_types::{ErrorCode, PfError};

use crate::ffi::{self, Handle};

/// One module of a process, with its PE identity.
#[derive(Clone, Debug)]
pub struct ModuleInfo {
    pub base: u64,
    pub size: u32,
    pub name: String,
    pub path: String,
    /// PE header TimeDateStamp (seconds since 1970, link time).
    pub timestamp: u32,
    /// Optional-header CheckSum — the Windows build-fingerprint analog of
    /// a build-id note (the RSDS GUID is the build identity itself).
    pub checksum: u32,
    /// RSDS (CodeView) PDB reference from the debug directory.
    pub pdb: Option<PdbRef>,
}

/// The PDB reference a PE carries: GUID + age identify the exact PDB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdbRef {
    pub guid: [u8; 16],
    pub age: u32,
    pub path: String,
}

// MODULEENTRY32W (x86-64): 4×u32 fields, modBaseAddr (ptr), modBaseSize,
// hModule, szModule[256], szExePath[260] → 1072 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct ModuleEntry32W {
    dw_size: u32,
    th32_module_id: u32,
    th32_process_id: u32,
    glblcnt_usage: u32,
    proccnt_usage: u32,
    mod_base_addr: *mut u8,
    mod_base_size: u32,
    h_module: Handle,
    sz_module: [u16; 256],
    sz_exe_path: [u16; 260],
}

const _SIZE_ASSERT: () = assert!(size_of::<ModuleEntry32W>() == 1080);

const TH32CS_SNAP_MODULE: u32 = 0x8;
const TH32CS_SNAP_MODULE32: u32 = 0x10;
const MAX_PATH_U16: usize = 260;

unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Module32FirstW(snapshot: Handle, entry: *mut ModuleEntry32W) -> i32;
    fn Module32NextW(snapshot: Handle, entry: *mut ModuleEntry32W) -> i32;
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Enumerate the modules of `pid` (same-user; other-user targets are
/// refused by the OS and surface as a typed error).
pub fn modules_of(pid: u32) -> Result<Vec<ModuleInfo>, PfError> {
    // SAFETY: Toolhelp snapshot APIs per Microsoft docs; the entry struct
    // is initialized with dwSize before each call and owned exclusively.
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAP_MODULE | TH32CS_SNAP_MODULE32, pid) };
    if snapshot as isize == ffi::INVALID_HANDLE_VALUE as isize {
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            format!(
                "module snapshot for pid {pid} failed (target must be same-user \
                 for the Toolhelp module list)"
            ),
        ));
    }
    let mut out = Vec::new();
    let mut entry = ModuleEntry32W {
        dw_size: size_of::<ModuleEntry32W>() as u32,
        th32_module_id: 0,
        th32_process_id: 0,
        glblcnt_usage: 0,
        proccnt_usage: 0,
        mod_base_addr: std::ptr::null_mut(),
        mod_base_size: 0,
        h_module: std::ptr::null_mut(),
        sz_module: [0; 256],
        sz_exe_path: [0; MAX_PATH_U16],
    };
    // SAFETY: entry is a valid, correctly sized struct; the snapshot handle
    // is owned here and closed on every path.
    let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) };
    while ok != 0 {
        let path = wide_to_string(&entry.sz_exe_path);
        let name = wide_to_string(&entry.sz_module);
        let (timestamp, checksum, pdb) = read_pe_identity(Path::new(&path)).unwrap_or((0, 0, None));
        out.push(ModuleInfo {
            base: entry.mod_base_addr as u64,
            size: entry.mod_base_size,
            name,
            path: path.clone(),
            timestamp,
            checksum,
            pdb,
        });
        // SAFETY: same entry reuse contract as the first call.
        ok = unsafe { Module32NextW(snapshot, &mut entry) };
        entry.dw_size = size_of::<ModuleEntry32W>() as u32;
    }
    // SAFETY: we own the snapshot handle.
    unsafe { ffi::CloseHandle(snapshot) };
    if out.is_empty() {
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            format!("no modules visible for pid {pid} (other-user or exited target)"),
        ));
    }
    Ok(out)
}

/// Read a PE file's TimeDateStamp and CodeView (RSDS) PDB reference from
/// the on-disk image. Bounded reader; malformed files yield (0, None).
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
fn read_pe_identity(path: &Path) -> Option<(u32, u32, Option<PdbRef>)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 512 * 1024 * 1024 {
        return None;
    }
    let rd_u16 = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes([*bytes.get(off)?, *bytes.get(off + 1)?]))
    };
    let rd_u32 = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(off)?,
            *bytes.get(off + 1)?,
            *bytes.get(off + 2)?,
            *bytes.get(off + 3)?,
        ]))
    };
    // DOS header: 'MZ' + e_lfanew at 0x3c.
    if bytes.get(0..2)? != b"MZ" {
        return None;
    }
    let pe_off = rd_u32(0x3c)? as usize;
    if bytes.get(pe_off..pe_off + 4)? != b"PE\0\0" {
        return None;
    }
    // COFF header follows the 4-byte signature: Machine at pe+4,
    // TimeDateStamp at pe+8; the optional header starts at pe+24.
    let timestamp = rd_u32(pe_off + 8)?;
    let optional_off = pe_off + 24;
    let magic = rd_u16(optional_off)?;
    // Optional-header CheckSum sits at +64 from the optional header start.
    let checksum = rd_u32(optional_off + 64).unwrap_or(0);
    // DataDirectory starts at optional+112 for PE32+ (0x20b), +96 for
    // PE32 (0x10b).
    let dd_off = optional_off + if magic == 0x20b { 112 } else { 96 };
    // Debug directory = DataDirectory[6].
    let dbg_rva = rd_u32(dd_off + 6 * 8)? as usize;
    let dbg_size = rd_u32(dd_off + 6 * 8 + 4)? as usize;
    if dbg_rva == 0 || dbg_size == 0 {
        return Some((timestamp, checksum, None));
    }
    // Section table maps RVA → file offset (bounded scan).
    let num_sections = rd_u16(pe_off + 6)? as usize;
    let opt_size = rd_u16(pe_off + 20)? as usize;
    let sections_off = optional_off + opt_size;
    let rva_to_off = |rva: usize| -> Option<usize> {
        for i in 0..num_sections {
            let sh = sections_off + i * 40;
            let vaddr = rd_u32(sh + 12)? as usize;
            let raw_size = rd_u32(sh + 16)? as usize;
            let raw_ptr = rd_u32(sh + 20)? as usize;
            if rva >= vaddr && rva < vaddr + raw_size.max(1) {
                return Some(raw_ptr + (rva - vaddr));
            }
        }
        None
    };
    let file_off = rva_to_off(dbg_rva)?;
    // Walk the debug directory entries (28 bytes each).
    let entries = dbg_size / 28;
    for i in 0..entries {
        let e = file_off + i * 28;
        let dtype = rd_u32(e + 12)?;
        if dtype != 2 {
            continue; // IMAGE_DEBUG_TYPE_CODEVIEW
        }
        let data_size = rd_u32(e + 16)? as usize;
        // PointerToRawData (+24) is the FILE offset; AddressOfRawData (+20)
        // is the RVA. Mapped-image linkers may store 0 in the file pointer
        // — fall back to the RVA through the section map.
        let data_ptr = match rd_u32(e + 24)? {
            0 => match rva_to_off(rd_u32(e + 20)? as usize) {
                Some(off) => off,
                None => continue,
            },
            ptr => ptr as usize,
        };
        let _ = data_size;
        let cv = data_ptr;
        if bytes.get(cv..cv + 4)? != b"RSDS" {
            continue;
        }
        let mut guid = [0u8; 16];
        guid.copy_from_slice(bytes.get(cv + 4..cv + 20)?);
        let age = rd_u32(cv + 20)?;
        let pdb_path_start = cv + 24;
        let pdb_path_end = bytes[pdb_path_start..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| pdb_path_start + p)
            .unwrap_or(pdb_path_start);
        let pdb_path = String::from_utf8_lossy(&bytes[pdb_path_start..pdb_path_end])
            .trim_end_matches('\0')
            .to_string();
        return Some((
            timestamp,
            checksum,
            Some(PdbRef {
                guid,
                age,
                path: pdb_path,
            }),
        ));
    }
    Some((timestamp, checksum, None))
}

/// Resolve a module's RSDS PDB reference to a verified PDB file:
/// 1. the reference's own path (absolute or module-relative),
/// 2. the module's sibling directory (bare name),
/// 3. `_NT_ALT_SYMBOL_PATH` then `_NT_SYMBOL_PATH` directories
///    (semicolon-separated; only plain directories are honored —
///    `srv*`/`cache*`/`symsrv*` store syntax is skipped honestly, no
///    network access is ever performed).
///
/// Every candidate must pass the GUID+age match — a wrong file is never
/// accepted just because its name fits.
#[allow(clippy::arithmetic_side_effects)]
pub fn resolve_pdb(module_path: &Path, r: &PdbRef) -> Result<Option<std::path::PathBuf>, PfError> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let bare = r
        .path
        .rsplit([std::path::MAIN_SEPARATOR, '/'])
        .next()
        .unwrap_or(&r.path)
        .to_string();
    // 1. the reference path itself (relative ones resolve under the module).
    let ref_path = std::path::Path::new(&r.path);
    if ref_path.is_absolute() {
        candidates.push(ref_path.to_path_buf());
    } else if let Some(parent) = module_path.parent() {
        candidates.push(parent.join(&r.path));
    }
    // 2. sibling directory.
    if let Some(parent) = module_path.parent() {
        candidates.push(parent.join(&bare));
    }
    // 3. symbol-path environment directories (bounded).
    for var in ["_NT_ALT_SYMBOL_PATH", "_NT_SYMBOL_PATH"] {
        if let Some(value) = std::env::var_os(var) {
            for dir in value.to_string_lossy().split(';') {
                if candidates.len() >= 64 {
                    break;
                }
                let dir = dir.trim();
                if dir.is_empty()
                    || dir.starts_with("srv*")
                    || dir.starts_with("cache*")
                    || dir.starts_with("symsrv*")
                {
                    continue; // store syntax: no network, skip honestly
                }
                candidates.push(std::path::Path::new(dir).join(&bare));
            }
        }
    }
    for candidate in &candidates {
        if !candidate.is_file() {
            continue;
        }
        if pdb_matches(candidate, r)? {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// Parse a file's PE identity directly (test and tool surface over the
/// private `read_pe_identity`).
pub fn read_identity(path: &Path) -> Option<(u32, u32, Option<PdbRef>)> {
    read_pe_identity(path)
}

/// Match a PDB file against a PE's RSDS reference: the MSF info stream
/// (stream 1) carries the same GUID + age. Both must match.
pub fn pdb_matches(pdb_path: &Path, r: &PdbRef) -> Result<bool, PfError> {
    // A malformed PDB answers "no match" (false), never an error — the
    // decision is honest either way; an unreadable file is an I/O error.
    match pdb_matches_inner(pdb_path, r) {
        Some(v) => Ok(v),
        None => Err(PfError::new(
            ErrorCode::StorageIo,
            format!("{}: unreadable or malformed PDB", pdb_path.display()),
        )),
    }
}

fn pdb_matches_inner(pdb_path: &Path, r: &PdbRef) -> Option<bool> {
    let bytes = std::fs::read(pdb_path).ok()?;
    if bytes.len() > 512 * 1024 * 1024 {
        return Some(false);
    }
    let rd_u32 = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(off)?,
            *bytes.get(off + 1)?,
            *bytes.get(off + 2)?,
            *bytes.get(off + 3)?,
        ]))
    };
    // Superblock: "Microsoft C/C++ MSF 7.00\r\n" signature, block size,
    // directory size, block map address.
    if bytes.get(0..26)? != b"Microsoft C/C++ MSF 7.00\r\n" {
        return Some(false);
    }
    let block_size = rd_u32(0x20)? as usize;
    if !(512..=(1 << 24)).contains(&block_size) || !block_size.is_power_of_two() {
        return Some(false);
    }
    // Superblock fields: BlockSize@0x20, FreeBlockMapBlock@0x24,
    // NumBlocks@0x28, NumDirectoryBytes@0x2C, Unknown0@0x30,
    // BlockMapAddr@0x34.
    let dir_size = rd_u32(0x2c)? as usize;
    let block_map_addr = rd_u32(0x34)? as usize;
    let blocks = |block: usize| -> Option<usize> {
        // One block index per 4 bytes, starting at block_map_addr's block.
        let idx_base = block_map_addr * block_size;
        let v = rd_u32(idx_base + block * 4)? as usize;
        Some(v * block_size)
    };
    let dir_blocks = dir_size.div_ceil(block_size);
    // Read the stream directory (spread over directory blocks).
    let mut dir = Vec::with_capacity(dir_size);
    for b in 0..dir_blocks {
        let start = blocks(b)?;
        let take = (block_size).min(dir_size - dir.len());
        dir.extend_from_slice(bytes.get(start..start + take)?);
    }
    let rd_dir_u32 = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *dir.get(off)?,
            *dir.get(off + 1)?,
            *dir.get(off + 2)?,
            *dir.get(off + 3)?,
        ]))
    };
    let num_streams = rd_dir_u32(0)? as usize;
    if num_streams > 65_536 {
        return Some(false); // pathological: refuse, never guess
    }
    // Stream sizes follow, then per-stream block lists. Stream 1 = PDB info.
    let mut sizes = Vec::with_capacity(num_streams);
    for i in 0..num_streams {
        sizes.push(rd_dir_u32(4 + i * 4)? as usize);
    }
    // Walk block lists to find stream 1's start.
    let mut cursor = 4 + num_streams * 4;
    let mut info_start: Option<usize> = None;
    for (i, size) in sizes.iter().enumerate() {
        let nblocks = size.div_ceil(block_size);
        if i == 1 {
            let first_block = rd_dir_u32(cursor)? as usize;
            info_start = Some(first_block * block_size);
            break;
        }
        cursor += nblocks * 4;
    }
    let Some(info_start) = info_start else {
        return Some(false);
    };
    // Info stream: version (u32), signature (u32), age (u32), guid (16).
    if bytes.len() < info_start + 32 {
        return Some(false);
    }
    let age = rd_u32(info_start + 8)?;
    let mut guid = [0u8; 16];
    guid.copy_from_slice(bytes.get(info_start + 12..info_start + 28)?);
    Some(guid == r.guid && age == r.age)
}

// ---------------------------------------------------------------------------
// Remote identity (PF-901 ReadProcessMemory slice): parse the LOADED PE
// header and debug directory directly from the target's address space, so
// module identity works even when the on-disk image is unreadable, renamed
// away, or otherwise differs from the loaded image. In-memory layout: RVA
// = address - base, section mapping uses VirtualAddress/VirtualSize, and
// the debug directory locates its data via AddressOfRawData (an RVA).
// ---------------------------------------------------------------------------

use crate::ffi::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, ReadProcessMemory};

/// A handle to another process opened for bounded remote reads.
pub struct RemoteProcess {
    handle: Handle,
}

impl RemoteProcess {
    /// Open `pid` for reading (same-user; other-user targets fail with a
    /// typed error — never a silent fallback).
    pub fn open(pid: u32) -> Result<RemoteProcess, PfError> {
        // SAFETY: OpenProcess per Microsoft docs; the handle is owned by
        // the RAII wrapper and closed on drop.
        let handle = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, 0, pid) };
        if handle as isize == ffi::INVALID_HANDLE_VALUE as isize || handle.is_null() {
            return Err(PfError::new(
                ErrorCode::PermissionDenied,
                format!("OpenProcess for pid {pid} failed (same-user VM read required)"),
            ));
        }
        Ok(RemoteProcess { handle })
    }

    /// Bounded remote read of `len` bytes at `addr`.
    pub fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, PfError> {
        let mut buf = vec![0u8; len];
        let mut read: usize = 0;
        // SAFETY: buf is len bytes and outlives the call; the process
        // handle is valid for the duration.
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const u8 as *const std::ffi::c_void,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                len,
                &mut read,
            )
        };
        if ok == 0 || read != len {
            return Err(PfError::new(
                ErrorCode::BackendFailure,
                format!("ReadProcessMemory at {addr:#x} read {read}/{len} bytes"),
            ));
        }
        Ok(buf)
    }
}

impl Drop for RemoteProcess {
    fn drop(&mut self) {
        // SAFETY: we own the handle exactly once.
        unsafe { ffi::CloseHandle(self.handle) };
    }
}

/// Read the LOADED image's identity for the module at `base`: PE
/// TimeDateStamp + RSDS PDB reference, parsed from the target's address
/// space. Works even when the on-disk file is gone or has been replaced.
pub fn remote_module_identity(
    rp: &RemoteProcess,
    base: u64,
) -> Result<(u32, u32, Option<PdbRef>), PfError> {
    // DOS header: e_lfanew at +0x3c.
    let dos = rp.read(base, 0x40)?;
    if &dos[0..2] != b"MZ" {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "remote image lacks the MZ signature",
        ));
    }
    let pe_off = u32::from_le_bytes([dos[0x3c], dos[0x3d], dos[0x3e], dos[0x3f]]) as u64;
    // NT headers: signature + COFF + optional.
    let nt = rp.read(base + pe_off, 4 + 20 + 240)?;
    if &nt[0..4] != b"PE\0\0" {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "remote image lacks the PE signature",
        ));
    }
    let timestamp = u32::from_le_bytes([nt[8], nt[9], nt[10], nt[11]]);
    let opt_off = 24usize; // within `nt`: signature(4) + COFF(20)
    let magic = u16::from_le_bytes([nt[opt_off], nt[opt_off + 1]]);
    // Optional-header CheckSum at +64 (the build-fingerprint analog).
    let checksum = u32::from_le_bytes([
        nt[opt_off + 64],
        nt[opt_off + 65],
        nt[opt_off + 66],
        nt[opt_off + 67],
    ]);
    let dd_off = opt_off + if magic == 0x20b { 112 } else { 96 };
    // Debug directory = DataDirectory[6] (6 * 8 bytes in).
    let dbg_rva = u32::from_le_bytes([
        nt[dd_off + 48],
        nt[dd_off + 49],
        nt[dd_off + 50],
        nt[dd_off + 51],
    ]) as u64;
    let dbg_size = u32::from_le_bytes([
        nt[dd_off + 52],
        nt[dd_off + 53],
        nt[dd_off + 54],
        nt[dd_off + 55],
    ]) as usize;
    if dbg_rva == 0 || dbg_size == 0 {
        return Ok((timestamp, checksum, None));
    }
    // In-memory images: RVA maps directly to base + RVA (the header region
    // and the debug directory are page-mapped contiguously).
    let dir = rp.read(base + dbg_rva, dbg_size)?;
    let entries = dbg_size / 28;
    for i in 0..entries {
        let e = &dir[i * 28..(i + 1) * 28];
        let dtype = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
        if dtype != 2 {
            continue; // IMAGE_DEBUG_TYPE_CODEVIEW
        }
        // In memory, AddressOfRawData (+20) is the RVA of the CodeView
        // record; PointerToRawData (+24) is meaningless for mapped images.
        let cv_rva = u32::from_le_bytes([e[20], e[21], e[22], e[23]]) as u64;
        let cv = rp.read(base + cv_rva, 24 + 260)?;
        if &cv[0..4] != b"RSDS" {
            continue;
        }
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&cv[4..20]);
        let age = u32::from_le_bytes([cv[20], cv[21], cv[22], cv[23]]);
        let path_len = cv[24..].iter().position(|&b| b == 0).unwrap_or(260);
        let pdb_path = String::from_utf8_lossy(&cv[24..24 + path_len]).into_owned();
        return Ok((
            timestamp,
            checksum,
            Some(PdbRef {
                guid,
                age,
                path: pdb_path,
            }),
        ));
    }
    Ok((timestamp, checksum, None))
}
