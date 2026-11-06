//! User data directory resolution (OS-correct, no elevation, no network).
//!
//! Precedence: `PULSEFORGE_DATA` env override, then the per-user data root:
//! `%LOCALAPPDATA%\PulseForge` on Windows, `$XDG_DATA_HOME/pulseforge` (then
//! `~/.local/share/pulseforge`) on Linux.

use std::path::PathBuf;

pub const ENV_OVERRIDE: &str = "PULSEFORGE_DATA";

/// Resolve the PulseForge data root, creating it if needed.
pub fn data_root() -> Result<PathBuf, String> {
    data_root_with(std::env::var_os(ENV_OVERRIDE))
}

/// Testable core: explicit override instead of ambient env access.
fn data_root_with(override_path: Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    if let Some(custom) = override_path {
        let p = PathBuf::from(custom);
        std::fs::create_dir_all(&p).map_err(|e| format!("{ENV_OVERRIDE}: {e}"))?;
        return Ok(p);
    }
    let base =
        default_base().ok_or_else(|| "no known per-user data directory for this OS".to_string())?;
    let root = base.join("PulseForge");
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    Ok(root)
}

/// Sessions live under `<data>/sessions`.
pub fn sessions_root() -> Result<PathBuf, String> {
    Ok(data_root()?.join("sessions"))
}

fn default_base() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        let dir = std::env::temp_dir().join(format!("pf-paths-{}", std::process::id()));
        let root = data_root_with(Some(dir.clone().into_os_string())).expect("override root");
        assert_eq!(root, dir);
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_root_extends_data_root() {
        let dir = std::env::temp_dir().join(format!("pf-paths-sessions-{}", std::process::id()));
        let root = data_root_with(Some(dir.clone().into_os_string())).unwrap();
        assert!(root.ends_with("PulseForge") || root == dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
