//! On-demand daemon auto-start: when the GUI finds the daemon pipe
//! unreachable, it launches the sibling `pulseforge` binary's `daemon`
//! subcommand (detached) and waits for the pipe to come up — the same
//! parity the CLI's `record --run` has for workloads. The daemon is the
//! product's own local named-pipe/UDS server; nothing network-facing is
//! created here.

use std::path::{Path, PathBuf};
use std::process::Command;

use pf_types::{ErrorCode, PfError};

/// The daemon binary that ships next to the GUI executable.
pub fn daemon_exe_in(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "pulseforge.exe";
    #[cfg(not(windows))]
    let name = "pulseforge";
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// Resolve the daemon binary: sibling of the running GUI executable.
/// (The bare-name PATH fallback is deliberately absent — spawning an
/// unrelated `pulseforge` from PATH would be surprising.)
pub fn daemon_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .and_then(|dir| {
            // Under `cargo test`, test binaries run inside `target/.../deps/`.
            // Any `pulseforge.exe` placed there is a transient build artifact,
            // not the deployed sibling CLI binary.
            if dir.file_name().is_some_and(|n| n == "deps") {
                return None;
            }
            daemon_exe_in(&dir)
        })
}

/// Spawn the daemon DETACHED: it must outlive the GUI and never flash a
/// console window or die with one. The returned child is dropped — std
/// does not kill children on drop.
pub fn spawn_detached(exe: &Path) -> Result<(), PfError> {
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (no console flash) | CREATE_NEW_PROCESS_GROUP.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: survives the GUI's exit, no job-control
        // coupling either direction.
        cmd.process_group(0);
    }
    cmd.spawn().map(|_| ()).map_err(|e| {
        PfError::new(
            ErrorCode::BackendFailure,
            format!("daemon auto-start spawn: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_exe_resolution() {
        let dir = std::env::temp_dir().join(format!("pf-ds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let name = "pulseforge.exe";
        #[cfg(not(windows))]
        let name = "pulseforge";
        // Absent -> None (this is the path every headless test takes).
        assert!(!daemon_exe_in(&dir).is_some_and(|p| p.parent().unwrap() == dir));
        // Present -> Some(the file).
        std::fs::write(dir.join(name), b"stub").unwrap();
        let found = daemon_exe_in(&dir).expect("exe resolved");
        assert_eq!(found.parent().unwrap(), dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
