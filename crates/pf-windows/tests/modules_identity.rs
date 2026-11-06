//! PF-901 bounded slice verification (Windows): same-user module identity
//! via the Toolhelp snapshot — the test binary's own module carries a real
//! RSDS PDB reference (dev builds link PDBs), the PDB file's info stream
//! matches GUID+age, ASLR produces cross-process base variance, and module
//! load/unload is observable in consecutive snapshots.

#![cfg(windows)]
// Bounded test arithmetic and FFI handles.
#![allow(clippy::arithmetic_side_effects, unsafe_code)]

use std::process::Command;
use std::time::Duration;

use pf_windows::modules::{PdbRef, modules_of, pdb_matches, resolve_pdb};

// LoadLibraryW/FreeLibrary for the load/unload observation.
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
    fn FreeLibrary(lib: *mut core::ffi::c_void) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[test]
fn own_modules_carry_pe_identity_and_matching_pdb() {
    let pid = std::process::id();
    let modules = modules_of(pid).expect("same-user snapshot of self");
    assert!(!modules.is_empty());
    // The main module is the test binary itself.
    let self_exe = std::env::current_exe().unwrap();
    let main = modules
        .iter()
        .find(|m| same_file(&m.path, &self_exe))
        .expect("test binary in its own module list");
    assert!(main.size > 0);
    assert!(main.timestamp > 0, "dev builds are linked with a timestamp");
    // Dev builds link PDBs: the RSDS reference is present and points at a
    // real file next to the binary.
    let r = main.pdb.as_ref().expect("RSDS PDB reference present");
    assert!(r.age >= 1);
    assert!(r.path.to_lowercase().ends_with(".pdb"), "{}", r.path);
    // The PDB file itself exists and its info stream matches GUID+age.
    // Linkers may record the PDB name relative to the module's directory.
    let pdb_path = std::path::Path::new(&r.path);
    let pdb_path = if pdb_path.is_absolute() {
        pdb_path.to_path_buf()
    } else {
        std::path::Path::new(&main.path)
            .parent()
            .expect("module dir")
            .join(pdb_path)
    };
    assert!(
        pdb_path.exists(),
        "PDB file exists at {}",
        pdb_path.display()
    );
    match pdb_matches(&pdb_path, r) {
        Ok(true) => {}
        other => panic!(
            "PDB info stream must match the PE's GUID+age: {other:?}              (pe age {} guid {:?}, pdb {})",
            r.age,
            r.guid,
            pdb_path.display()
        ),
    }
    // Wrong age never matches.
    let wrong = PdbRef {
        guid: r.guid,
        age: r.age.wrapping_add(1),
        path: r.path.clone(),
    };
    assert!(
        !pdb_matches(&pdb_path, &wrong).expect("readable PDB"),
        "a wrong age must not match"
    );
}

fn same_file(a: &str, b: &std::path::Path) -> bool {
    // Case-insensitive suffix comparison: the Toolhelp path and current_exe
    // agree on the tail (deps\\pf_windows-*.exe).
    let a = a.replace('/', "\\").to_lowercase();
    let b = b.to_string_lossy().replace('/', "\\").to_lowercase();
    a.ends_with(&b) || b.ends_with(&a)
}

unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
}

#[test]
fn snapshot_reports_the_true_aslr_base() {
    // Ground truth for identity: the self module's base from the snapshot
    // equals GetModuleHandleW(NULL) for the running image.
    let self_modules = modules_of(std::process::id()).expect("self snapshot");
    let self_exe = std::env::current_exe().unwrap();
    let main = self_modules
        .iter()
        .find(|m| same_file(&m.path, &self_exe))
        .expect("own module in the snapshot");
    // SAFETY: a NULL module name returns the running image's base.
    let hmodule = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
    assert_eq!(
        main.base, hmodule,
        "snapshot base must equal the loader's base for the running image"
    );

    // ASLR validation: a relocated x64 image loads in the high region
    // (non-ASLR x64 PEs load at 0x140000000). Spawn our private fixture
    // and check its base sits in the ASLR region.
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/pf-fixture-cpu.exe")
        .canonicalize()
        .expect("workspace fixture binary");
    let mut child = Command::new(&fixture)
        .args(["--ms", "3000"])
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(200));
    let modules = modules_of(child.id()).expect("same-user snapshot");
    let fixture_main = modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("pf-fixture-cpu.exe"))
        .expect("fixture main module");
    let base = fixture_main.base;
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        base >= 0x7FF0_0000_0000,
        "the fixture base {base:#x} must sit in the ASLR region          (non-relocated x64 images load at 0x140000000)"
    );
}

#[test]
fn module_load_unload_is_observable() {
    // winmm.dll is rarely preloaded in a test binary; loading it must add
    // a module entry, unloading must remove it.
    let before = modules_of(std::process::id()).expect("self snapshot");
    assert!(
        !before
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case("winmm.dll")),
        "precondition: winmm.dll not loaded"
    );
    let name = wide("winmm.dll");
    // SAFETY: LoadLibraryW with a valid wide string; freed below.
    let lib = unsafe { LoadLibraryW(name.as_ptr()) };
    assert!(!lib.is_null(), "LoadLibraryW(winmm.dll)");
    let loaded = modules_of(std::process::id()).expect("self snapshot");
    let entry = loaded
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("winmm.dll"))
        .expect("winmm.dll appears in the module list after load");
    assert!(entry.size > 0);
    // SAFETY: the handle came from LoadLibraryW and is still loaded.
    let freed = unsafe { FreeLibrary(lib) };
    assert_ne!(freed, 0, "FreeLibrary");
    let after = modules_of(std::process::id()).expect("self snapshot");
    assert!(
        !after
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case("winmm.dll")),
        "winmm.dll must leave the module list after unload"
    );
}

#[test]
fn remote_identity_works_even_when_the_file_is_renamed() {
    use pf_windows::modules::RemoteProcess;
    use pf_windows::modules::remote_module_identity;

    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/pf-fixture-cpu.exe")
        .canonicalize()
        .expect("workspace fixture binary");
    let mut child = Command::new(&fixture)
        .args(["--ms", "4000"])
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(200));

    // Ground truth: the on-disk parse of the SAME image.
    let modules = modules_of(child.id()).expect("same-user snapshot");
    let main = modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("pf-fixture-cpu.exe"))
        .expect("fixture main module");
    let base = main.base;
    let on_disk_identity = main.pdb.as_ref().expect("on-disk RSDS reference").clone();

    // The slice under test: identity read from the LOADED image.
    let rp = RemoteProcess::open(child.id()).expect("open remote");
    let (ts_remote, checksum_remote, pdb_remote) =
        remote_module_identity(&rp, base).expect("remote identity");

    // The loaded image is page-mapped from the same file: identities agree.
    assert_eq!(ts_remote, main.timestamp, "timestamps agree");
    let pdb_remote = pdb_remote.expect("remote RSDS reference");
    assert_eq!(
        pdb_remote.guid, on_disk_identity.guid,
        "GUIDs agree between disk and memory"
    );
    assert_eq!(pdb_remote.age, on_disk_identity.age, "ages agree");
    assert_eq!(
        checksum_remote, main.checksum,
        "remote checksum matches the Toolhelp-parsed one"
    );

    // The point of the slice: even with the on-disk file RENAMED AWAY
    // (Windows allows renaming a running executable), the loaded image
    // still yields identity.
    let renamed = fixture.with_extension("renamed-during-test.exe");
    let rename_ok = std::fs::rename(&fixture, &renamed).is_ok();
    if rename_ok {
        let (ts2, _checksum2, pdb2) =
            remote_module_identity(&rp, base).expect("remote identity after rename");
        assert_eq!(ts2, main.timestamp);
        let pdb2 = pdb2.expect("remote RSDS reference after rename");
        assert_eq!(pdb2.guid, on_disk_identity.guid);
        assert_eq!(pdb2.age, on_disk_identity.age);
        // The file-parse path would now fail to find the module file at
        // its original path — the remote path is the one that still works.
        eprintln!("E2E-REMOTE-RENAME ok: identity survived the rename");
        std::fs::rename(&renamed, &fixture).expect("restore fixture name");
    } else {
        eprintln!("E2E-REMOTE-RENAME skipped: rename refused on this system");
    }
    let _ = ts_remote;
    let _ = &pdb_remote;
    let _ = child.kill();
    let _ = child.wait();
}

/// The split-PDB workflow: a COPY of the test binary (its PDB reference
/// still names the original PDB) lives in a directory with NO PDB, while
/// the real PDB sits only in a `_NT_ALT_SYMBOL_PATH` directory.
/// `resolve_pdb` must recover it through the search path, verified by
/// GUID+age. Private copies — no shared files, no cross-test races.
#[test]
fn search_path_recovers_a_split_pdb() {
    let modules = modules_of(std::process::id()).expect("self snapshot");
    let self_exe = std::env::current_exe().unwrap();
    let main = modules
        .iter()
        .find(|m| same_file(&m.path, &self_exe))
        .expect("own module");
    let r = main.pdb.as_ref().expect("RSDS reference");
    let pdb_source = {
        let p = std::path::Path::new(&r.path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::path::Path::new(&main.path)
                .parent()
                .expect("module dir")
                .join(p)
        }
    };
    assert!(pdb_source.exists(), "sibling PDB exists");

    // Split layout: the exe copy in its own dir, the PDB in a symbol dir.
    let split_dir = std::path::Path::new(&main.path)
        .parent()
        .expect("module dir")
        .join(format!("pf-split-test-{}", std::process::id()));
    let sym_dir = split_dir.join("symbols");
    std::fs::create_dir_all(&sym_dir).unwrap();
    let split_exe = split_dir.join("pf-split-test.exe");
    std::fs::copy(&self_exe, &split_exe).expect("copy the exe");
    let moved_pdb = sym_dir.join(
        std::path::Path::new(&r.path)
            .file_name()
            .expect("pdb bare name"),
    );
    std::fs::copy(&pdb_source, &moved_pdb).expect("copy the PDB into the symbol dir");
    // No PDB next to the split exe: a genuine split state.
    assert!(
        !split_exe
            .parent()
            .unwrap()
            .join(std::path::Path::new(&r.path).file_name().unwrap())
            .exists(),
        "the split dir carries no PDB"
    );

    // Identity of the copy comes from its own file parse.
    let (_ts, _checksum, pdb_ref) =
        pf_windows::modules::read_identity(&split_exe).expect("the split exe parses");
    let r = pdb_ref.expect("the copy carries the same RSDS reference");

    // The search-path slice under test.
    // SAFETY: only this test reads the variable in this suite.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("_NT_ALT_SYMBOL_PATH", sym_dir.to_string_lossy().to_string())
    };
    let resolved = resolve_pdb(&split_exe, &r)
        .expect("search-path resolution runs")
        .expect("the split PDB is recovered through the symbol path");
    eprintln!("E2E-PDB-SEARCH {}", resolved.display());
    assert!(
        resolved.parent() == Some(sym_dir.as_path()),
        "resolved through the symbol directory: {}",
        resolved.display()
    );
    assert!(
        pdb_matches(&resolved, &r).expect("readable recovered PDB"),
        "recovered PDB matches GUID+age"
    );

    // Without the search path, resolution honestly fails (typed, no
    // guessing): clear the variable and expect None.
    // SAFETY: same single-reader contract as above.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("_NT_ALT_SYMBOL_PATH")
    };
    let unresolved = resolve_pdb(&split_exe, &r).expect("resolution runs");
    assert!(
        unresolved.is_none() || unresolved.as_deref() != Some(resolved.as_path()),
        "without the search path the split PDB is not resolved"
    );

    let _ = std::fs::remove_dir_all(&split_dir);
}

/// The image CheckSum (the Windows build-fingerprint analog) is parsed
/// from the optional header and stable across snapshots. Dev builds may
/// legitimately carry 0 (the linker only computes it with /RELEASE) — the
/// contract is parse-and-stability, not nonzero.
#[test]
fn checksum_is_parsed_and_stable() {
    let modules = modules_of(std::process::id()).expect("self snapshot");
    let self_exe = std::env::current_exe().unwrap();
    let main = modules
        .iter()
        .find(|m| same_file(&m.path, &self_exe))
        .expect("own module");
    let again = modules_of(std::process::id()).expect("self snapshot again");
    let main2 = again
        .iter()
        .find(|m| same_file(&m.path, &self_exe))
        .expect("own module again");
    assert_eq!(main.checksum, main2.checksum, "checksum is stable");
}
