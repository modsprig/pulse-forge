//! Single-instance daemon lifecycle (roadmap PF-402).
//!
//! One daemon per user/login context, enforced by a lock file under the
//! user's private data root. The lock records the owner PID plus the
//! process creation identity so a stale lock from a dead (or rebooted-away)
//! daemon is detected and recovered — never trusted blindly, never removed
//! for a live daemon.

use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(test)]
use pf_types::Os;
use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, ProcessIdentity};

/// Held lock guard: releasing (drop) removes the lock file.
#[derive(Debug)]
pub struct SingleInstance {
    path: PathBuf,
    identity: ProcessIdentity,
}

#[derive(Debug)]
pub enum SingleInstanceError {
    Pf(PfError),
    /// Another daemon owns the lock and is verifiably alive.
    AlreadyRunning {
        path: PathBuf,
        owner_pid: u32,
    },
}

impl From<PfError> for SingleInstanceError {
    fn from(e: PfError) -> Self {
        SingleInstanceError::Pf(e)
    }
}

impl std::fmt::Display for SingleInstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SingleInstanceError::Pf(e) => write!(f, "{e}"),
            SingleInstanceError::AlreadyRunning { path, owner_pid } => write!(
                f,
                "another pulseforge daemon (pid {owner_pid}) owns {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SingleInstanceError {}

/// Resolve this process's own identity. OS-specific identity minting lives
/// behind the platform trait; for the lock we only need something stable
/// within a boot, so we accept an injected resolver (the daemon passes its
/// platform backend's identity()).
pub fn acquire(
    lock_dir: &Path,
    self_pid: u32,
    identity_of: &dyn Fn(u32) -> Result<ProcessIdentity, PfError>,
) -> Result<SingleInstance, SingleInstanceError> {
    std::fs::create_dir_all(lock_dir).map_err(|e| {
        SingleInstanceError::Pf(PfError::new(
            ErrorCode::StorageIo,
            format!("cannot create lock dir {}: {e}", lock_dir.display()),
        ))
    })?;
    let path = lock_dir.join("daemon.lock");
    let identity = identity_of(self_pid).map_err(SingleInstanceError::Pf)?;

    match read_lock(&path) {
        Ok(Some(existing)) if existing.get("pid").and_then(|v| v.as_u64()).is_some() => {
            let owner_pid = existing
                .get("pid")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            let owner_creation = existing
                .get("creation_identity")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if owner_alive(identity_of, owner_pid, owner_creation)? {
                return Err(SingleInstanceError::AlreadyRunning { path, owner_pid });
            }
            // Stale: the PID is gone or was reused by a different process.
            // Recover (overwrite) — the old daemon cannot come back.
        }
        Ok(None) | Ok(Some(_)) | Err(_) => {
            // No lock, or unreadable/corrupt lock (evidence, not fatal):
            // overwrite only if no live owner can be determined from it.
            if let Ok(Some(existing)) = read_lock(&path) {
                if let Some(owner_pid) = existing
                    .get("pid")
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u32::try_from(v).ok())
                {
                    let owner_creation = existing
                        .get("creation_identity")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if owner_alive(identity_of, owner_pid, owner_creation)? {
                        return Err(SingleInstanceError::AlreadyRunning { path, owner_pid });
                    }
                }
            }
        }
    }

    write_lock(&path, &identity)?;
    Ok(SingleInstance { path, identity })
}

fn owner_alive(
    identity_of: &dyn Fn(u32) -> Result<ProcessIdentity, PfError>,
    owner_pid: u32,
    owner_creation: u64,
) -> Result<bool, SingleInstanceError> {
    match identity_of(owner_pid) {
        Ok(current) => {
            if owner_creation == 0 {
                // Lock lacked identity info: conservatively treat PID-exists
                // as alive to never steal a live daemon's lock.
                Ok(true)
            } else {
                Ok(current.creation_identity == owner_creation)
            }
        }
        Err(e)
            if e.code == ErrorCode::InvalidArgument
                || e.code == ErrorCode::PermissionDenied
                || e.code == ErrorCode::TargetExited =>
        {
            Ok(false)
        }
        Err(e) => Err(SingleInstanceError::Pf(e)),
    }
}

fn read_lock(path: &Path) -> Result<Option<JsonValue>, PfError> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.len() <= 64 * 1024 => pf_types::json::parse(&text)
            .map(Some)
            .map_err(PfError::from),
        Ok(_) => Err(PfError::new(
            ErrorCode::ResourceExhausted,
            "daemon lock exceeds 64 KiB",
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PfError::from(e)),
    }
}

fn write_lock(path: &Path, identity: &ProcessIdentity) -> Result<(), SingleInstanceError> {
    use pf_types::json::JsonValue as J;
    let json = J::Object(vec![
        ("schema".into(), J::Str("pulseforge.lock/0".into())),
        ("pid".into(), J::UInt(u64::from(identity.pid))),
        ("os".into(), J::Str(identity.os.as_str().into())),
        (
            "creation_identity".into(),
            J::UInt(identity.creation_identity),
        ),
        (
            "boot_identity".into(),
            J::Str(identity.boot_identity.clone()),
        ),
        (
            "written_utc_ms".into(),
            J::UInt(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
                    .unwrap_or(0),
            ),
        ),
    ]);
    let tmp = path.with_extension("partial");
    let mut f = std::fs::File::create(&tmp).map_err(|e| {
        SingleInstanceError::Pf(PfError::new(
            ErrorCode::StorageIo,
            format!("cannot create lock: {e}"),
        ))
    })?;
    writeln!(f, "{}", json.to_json_string())
        .map_err(|e| SingleInstanceError::Pf(PfError::new(ErrorCode::StorageIo, format!("{e}"))))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| SingleInstanceError::Pf(PfError::new(ErrorCode::StorageIo, format!("{e}"))))?;
    Ok(())
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Only remove the lock if it is still OURS (never another's).
        if let Ok(Some(existing)) = read_lock(&self.path) {
            let same = existing.get("pid").and_then(|v| v.as_u64())
                == Some(u64::from(self.identity.pid))
                && existing.get("creation_identity").and_then(|v| v.as_u64())
                    == Some(self.identity.creation_identity);
            if same {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_identity_factory(
        creation_by_pid: std::collections::HashMap<u32, u64>,
    ) -> impl Fn(u32) -> Result<ProcessIdentity, PfError> {
        move |pid| {
            creation_by_pid
                .get(&pid)
                .map(|c| ProcessIdentity::new(Os::Windows, pid, *c, "boot-test"))
                .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, format!("no pid {pid}")))
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pf-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn first_acquire_succeeds_and_drop_releases() {
        let dir = temp_dir("first");
        let ids = fake_identity_factory(std::collections::HashMap::from([(7u32, 100u64)]));
        {
            let lock = acquire(&dir, 7, &ids).expect("acquire");
            assert!(dir.join("daemon.lock").is_file());
            drop(lock);
        }
        assert!(!dir.join("daemon.lock").exists(), "drop must release");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_owner_blocks_second_acquire() {
        let dir = temp_dir("live");
        let ids = fake_identity_factory(std::collections::HashMap::from([(7u32, 100u64)]));
        let _first = acquire(&dir, 7, &ids).expect("first");
        match acquire(&dir, 7, &ids) {
            Err(SingleInstanceError::AlreadyRunning { owner_pid, .. }) => {
                assert_eq!(owner_pid, 7);
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_from_dead_daemon_is_recovered() {
        let dir = temp_dir("stale");
        // PID 99 existed when the lock was written but is gone now.
        let ids = fake_identity_factory(std::collections::HashMap::from([(42u32, 500u64)]));
        std::fs::write(
            dir.join("daemon.lock"),
            r#"{"schema":"pulseforge.lock/0","pid":99,"os":"windows","creation_identity":12345,"boot_identity":"boot-test","written_utc_ms":1}"#,
        )
        .unwrap();
        let _recovered = acquire(&dir, 42, &ids).expect("stale lock recovered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_reuse_is_not_confused_with_a_live_owner() {
        let dir = temp_dir("reuse");
        // Lock says pid 42 created at 111, but pid 42 now exists as a
        // *different* process (creation 222): the old daemon is dead.
        let ids = fake_identity_factory(std::collections::HashMap::from([
            (42u32, 222u64),
            (55u32, 333u64),
        ]));
        std::fs::write(
            dir.join("daemon.lock"),
            r#"{"schema":"pulseforge.lock/0","pid":42,"os":"windows","creation_identity":111,"boot_identity":"boot-test","written_utc_ms":1}"#,
        )
        .unwrap();
        let _recovered = acquire(&dir, 55, &ids).expect("reused PID does not block");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_without_identity_is_never_stolen() {
        let dir = temp_dir("conservative");
        // PID exists but lock has no creation identity: conservative alive.
        let ids = fake_identity_factory(std::collections::HashMap::from([
            (42u32, 222u64),
            (55u32, 333u64),
        ]));
        std::fs::write(
            dir.join("daemon.lock"),
            r#"{"schema":"pulseforge.lock/0","pid":42,"os":"windows","creation_identity":0,"boot_identity":"","written_utc_ms":1}"#,
        )
        .unwrap();
        assert!(matches!(
            acquire(&dir, 55, &ids),
            Err(SingleInstanceError::AlreadyRunning { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
