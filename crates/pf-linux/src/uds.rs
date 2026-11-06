//! Linux Unix-domain-socket transport for the daemon IPC endpoint
//! (roadmap PF-404) — the twin of `pf-windows::pipe`.
//!
//! Security model: the socket lives in `$XDG_RUNTIME_DIR` (a
//! per-user tmpfs only the owning user can traverse) or the data root when
//! that is unset; the file mode is forced to 0600 after bind, so connect
//! requires owning the file. Endpoint name = user + hash(data root),
//! mirroring the Windows pipe scheme.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use pf_types::{ErrorCode, PfError};

/// Canonical daemon endpoint path for this user **and data root** — the
/// same scheme as the Windows pipe name.
pub fn daemon_uds_path(data_root: &Path) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let root_hash = fnv1a(data_root.to_string_lossy().to_lowercase().as_bytes());
    runtime_dir().join(format!("pulseforge-daemon-{user}-{root_hash:08x}.sock"))
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// `$XDG_RUNTIME_DIR` or the data root (created by the caller).
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    // Fallback: the data root itself is user-private on Linux.
    pf_platform::paths::data_root().unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn force_owner_mode(path: &Path) -> Result<(), PfError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| {
        PfError::new(
            ErrorCode::PermissionDenied,
            format!("cannot force 0600 on {}: {e}", path.display()),
        )
    })
}

fn remove_stale_socket(path: &Path) {
    // A socket left by a dead daemon blocks bind; the single-instance lock
    // (PF-402) guarantees no live owner, so removing is safe here.
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

/// The connected stream handed to the daemon service / agent client.
pub struct UdsStream {
    inner: UnixStream,
    live: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl Read for UdsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for UdsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Drop for UdsStream {
    fn drop(&mut self) {
        if let Some(live) = &self.live {
            live.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The listening side.
pub struct UdsListener {
    listener: UnixListener,
    path: PathBuf,
    max_clients: usize,
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl UdsListener {
    /// Bind the endpoint (owner-only mode, stale socket removed).
    pub fn bind(path: &Path, max_clients: usize) -> Result<UdsListener, PfError> {
        remove_stale_socket(path);
        let listener = UnixListener::bind(path)
            .map_err(|e| PfError::new(ErrorCode::BackendFailure, format!("UDS bind: {e}")))?;
        force_owner_mode(path)?;
        Ok(UdsListener {
            listener,
            path: path.to_path_buf(),
            max_clients,
            live: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Accept one client, bounded by the concurrent-client cap.
    // Deadline arithmetic on bounded durations.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn accept(&mut self) -> Result<UdsStream, PfError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while self.live.load(Ordering::Relaxed) >= self.max_clients {
            if std::time::Instant::now() >= deadline {
                return Err(PfError::new(
                    ErrorCode::ResourceExhausted,
                    format!("{} concurrent UDS clients exceeded", self.max_clients),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let (stream, _peer) = self
            .listener
            .accept()
            .map_err(|e| PfError::new(ErrorCode::BackendFailure, format!("UDS accept: {e}")))?;
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(UdsStream {
            inner: stream,
            live: Some(std::sync::Arc::clone(&self.live)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UdsListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Connect a client to the daemon socket (bounded retry while the daemon
/// is still setting up). The absence budget is kept small (~0.6 s): a
/// missing socket is the common "daemon not running" case, and UI callers
/// must never sit here long enough to trip "not responding" heuristics.
pub fn connect(path: &Path) -> Result<UdsStream, PfError> {
    for _ in 0..12 {
        match UnixStream::connect(path) {
            Ok(stream) => {
                return Ok(UdsStream {
                    inner: stream,
                    live: None,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(PfError::new(
                    ErrorCode::PermissionDenied,
                    format!("daemon socket exists but access was denied: {e}"),
                )
                .with_remediation(
                    "the socket is restricted to its owner; connect as the user running the daemon",
                ));
            }
            Err(e) => {
                return Err(PfError::new(
                    ErrorCode::BackendFailure,
                    format!("UDS connect: {e}"),
                ));
            }
        }
    }
    Err(PfError::new(
        ErrorCode::Timeout,
        "daemon socket not reachable (is the daemon running?)",
    )
    .with_remediation("start it with `pulseforge daemon`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_name_is_per_user_and_data_root() {
        let a = daemon_uds_path(Path::new("/data/a"));
        assert!(a.to_string_lossy().contains("pulseforge-daemon-"));
        assert!(a.to_string_lossy().ends_with(".sock"));
        let b = daemon_uds_path(Path::new("/data/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn bind_accept_connect_echo_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pf-uds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");

        let mut listener = UdsListener::bind(&path, 4).expect("bind");
        assert!(path.exists());
        // Owner-only mode enforced.
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be 0600");

        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut c = connect(&client_path).expect("connect");
            c.write_all(b"ping").expect("write");
            let mut buf = [0u8; 4];
            c.read_exact(&mut buf).expect("read");
            buf.to_vec()
        });

        let mut server = listener.accept().expect("accept");
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).expect("server read");
        server.write_all(&buf).expect("server write");
        assert_eq!(client.join().expect("client thread"), b"ping");
        drop(server);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_removes_stale_socket() {
        let dir = std::env::temp_dir().join(format!("pf-uds-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stale.sock");
        fs::write(&path, b"garbage").unwrap(); // stale non-socket file

        let listener = UdsListener::bind(&path, 2).expect("stale removed, bound");
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }
}
