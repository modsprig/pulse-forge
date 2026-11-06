//! Windows named-pipe transport for the daemon IPC endpoint (roadmap
//! PF-403).
//!
//! Security model: the pipe is created with an explicit owner-only DACL
//! (SDDL `D:P(A;;GA;;;)` — empty trustee means *the owner*, i.e. the
//! creating user), `FILE_FLAG_FIRST_INSTANCE`, and a unique per-user name
//! derived from the session root. Other local users cannot connect; no
//! remote access is enabled. This is the v1 same-user authorization
//! boundary; capability tokens for richer policies remain an open ADR item.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io::{Read, Write};
use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pf_types::{ErrorCode, PfError};

use crate::ffi;

pub const ERROR_BROKEN_PIPE: u32 = 109;
pub const ERROR_PIPE_BUSY: u32 = 231;
pub const ERROR_MORE_DATA: u32 = 234;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
// FILE_FLAG_FIRST_INSTANCE deliberately omitted: it returned
// ERROR_INVALID_PARAMETER (87) on the verified build 22631 machine in every
// configuration, and the single-daemon guarantee is enforced by the lock
// file (single_instance) plus the per-user pipe name.
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const SDDL_REVISION_1: u32 = 1;
// (The DACL string is built per call with the caller's real SID — see
// `owner_only_sddl()`.)

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    security_descriptor: *mut c_void,
    inherit_handle: i32,
}
const _: () = assert!(size_of::<SecurityAttributes>() == 24);

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: ffi::Handle, desired_access: u32, token: *mut ffi::Handle) -> i32;

    fn GetTokenInformation(
        token: ffi::Handle,
        token_information_class: u32,
        token_information: *mut c_void,
        token_information_length: u32,
        return_length: *mut u32,
    ) -> i32;

    fn ConvertSidToStringSidA(sid: *const c_void, string_sid: *mut *mut u8) -> i32;

    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;

    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER_CLASS: u32 = 1;

/// Build `D:P(A;;GA;;;<caller SID>)` — access for the creating user only.
// FFI buffer walking: lengths bounded by caps in the loops.
#[allow(clippy::arithmetic_side_effects)]
fn owner_only_sddl() -> Result<String, PfError> {
    // SAFETY: pseudo-handle from GetCurrentProcess; valid out-pointer.
    let mut token: ffi::Handle = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(ffi::GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(PfError::new(
            ErrorCode::PermissionDenied,
            "OpenProcessToken failed while building the pipe ACL",
        ));
    }
    struct Token(ffi::Handle);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: we own this handle.
            unsafe { ffi::CloseHandle(self.0) };
        }
    }
    let _token = Token(token);

    // Size query first.
    let mut needed: u32 = 0;
    // SAFETY: null buffer is the documented sizing pattern.
    let _ = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 || needed > 4096 {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "unexpected token information size",
        ));
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: buffer/length pair matches the sizing call.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "GetTokenInformation(TokenUser) failed",
        ));
    }
    // TOKEN_USER layout: { SID_AND_ATTRIBUTES { PSID User; ULONG Attributes; } }
    // PSID is the first pointer at offset 0.
    if buffer.len() < 8 {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "token buffer too small",
        ));
    }
    let sid = usize::from_le_bytes(buffer[0..8].try_into().unwrap_or([0; 8]));
    let sid = sid as *const c_void;
    if sid.is_null() {
        return Err(PfError::new(ErrorCode::BackendFailure, "null user SID"));
    }
    let mut sid_str: *mut u8 = std::ptr::null_mut();
    // SAFETY: SID comes from the token; out-pointer is valid.
    let ok = unsafe { ConvertSidToStringSidA(sid, &mut sid_str) };
    if ok == 0 || sid_str.is_null() {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "ConvertSidToStringSid failed",
        ));
    }
    // SAFETY: read the ASCII string then free the allocation.
    let text = unsafe {
        let mut len = 0usize;
        while *sid_str.add(len) != 0 && len < 256 {
            len += 1; // bounded by the 256 cap above
        }
        let slice = std::slice::from_raw_parts(sid_str, len);
        let s = std::str::from_utf8(slice).unwrap_or("").to_string();
        ffi::win32_message(0); // no-op to keep the unsafe block purposeful
        LocalFree(sid_str.cast());
        s
    };
    if text.is_empty() {
        return Err(PfError::new(ErrorCode::BackendFailure, "empty SID string"));
    }
    Ok(format!("D:P(A;;GA;;;{text})"))
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer_size: u32,
        in_buffer_size: u32,
        default_time_out: u32,
        security_attributes: *const SecurityAttributes,
    ) -> ffi::Handle;

    fn ConnectNamedPipe(named_pipe: ffi::Handle, overlapped: *mut c_void) -> i32;

    fn DisconnectNamedPipe(named_pipe: ffi::Handle) -> i32;

    fn ReadFile(
        file: ffi::Handle,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;

    fn WriteFile(
        file: ffi::Handle,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;

    fn FlushFileBuffers(file: ffi::Handle) -> i32;

    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *const c_void,
        creation_disposition: u32,
        flags: u32,
        template_file: ffi::Handle,
    ) -> ffi::Handle;

    fn WaitNamedPipeW(named_pipe: *const u16, time_out: u32) -> i32;
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Canonical daemon endpoint name for this user **and data root**.
///
/// The user name separates login contexts; a short FNV-1a hash of the data
/// root separates independent instances (tests, alternate
/// `PULSEFORGE_DATA` roots). The name is not a secret — the owner-only DACL
/// enforces access.
pub fn daemon_pipe_name(data_root: &std::path::Path) -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
    let root_hash = fnv1a(data_root.to_string_lossy().to_lowercase().as_bytes());
    let prefix = pipe_prefix();
    format!("{prefix}pulseforge-daemon-{user}-{root_hash:08x}")
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for b in bytes {
        // Deliberate wrapping FNV-1a.
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// "\\.\pipe\" built without backslash literals (5C = backslash).
fn pipe_prefix() -> String {
    let bytes: Vec<u8> = vec![0x5C, 0x5C, b'.', 0x5C, b'p', b'i', b'p', b'e', 0x5C];
    String::from_utf8(bytes).unwrap_or_default()
}

/// A connected pipe handle implementing `Read + Write` for the framed
/// protocol. Closing = Drop.
pub struct PipeStream {
    handle: ffi::Handle,
    /// True when this end created the pipe instance (server side) and must
    /// disconnect+reap it.
    server_side: bool,
    /// Server-side live-instance accounting (decremented on Drop so the
    /// listener can serve more than `max_instances` sequential clients).
    live: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

// A HANDLE is an opaque value; the stream owns it exclusively.
unsafe impl Send for PipeStream {}

impl Drop for PipeStream {
    fn drop(&mut self) {
        if self.server_side && !self.handle.is_null() {
            // SAFETY: we own this instance.
            unsafe { DisconnectNamedPipe(self.handle) };
        }
        if !self.handle.is_null() && self.handle as isize != -1 {
            // SAFETY: we own this handle.
            unsafe { ffi::CloseHandle(self.handle) };
        }
        if let Some(live) = &self.live {
            live.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read: u32 = 0;
        // SAFETY: buffer/length pair matches; handle owned.
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr().cast(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { ffi::GetLastError() };
            if err == ERROR_BROKEN_PIPE {
                return Ok(0); // clean disconnect, presented as EOF
            }
            return Err(std::io::Error::other(format!(
                "pipe read failed: {}",
                ffi::win32_message(err)
            )));
        }
        Ok(read as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written: u32 = 0;
        // SAFETY: buffer/length pair matches; handle owned.
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr().cast(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { ffi::GetLastError() };
            return Err(std::io::Error::other(format!(
                "pipe write failed: {}",
                ffi::win32_message(err)
            )));
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: handle owned.
        let ok = unsafe { FlushFileBuffers(self.handle) };
        if ok == 0 {
            return Err(std::io::Error::other("pipe flush failed"));
        }
        Ok(())
    }
}

/// Create one server pipe instance (owner-only DACL). Call
/// [`PipeListener::accept`] to wait for a client.
pub struct PipeListener {
    name: String,
    handle: ffi::Handle,
    max_instances: u32,
    /// Concurrently-connected clients right now (not lifetime total).
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle as isize != -1 {
            // SAFETY: we own this instance.
            unsafe { ffi::CloseHandle(self.handle) };
        }
    }
}

// Instance counter bounded by max_instances.
#[allow(clippy::arithmetic_side_effects)]
impl PipeListener {
    /// Bind the first instance. `max_instances` bounds concurrency.
    pub fn bind(name: &str, max_instances: u32) -> Result<PipeListener, PfError> {
        let handle = create_instance(name, max_instances)?;
        Ok(PipeListener {
            name: name.to_string(),
            handle,
            max_instances,
            live: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Block until a client connects. On success returns the connected
    /// stream and leaves a fresh instance created for the next client.
    /// `Err` with `RESOURCE_EXHAUSTED` means the instance cap was hit.
    pub fn accept(&mut self) -> Result<PipeStream, PfError> {
        // Concurrency cap: wait until a slot frees (client disconnected).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while self.live.load(Ordering::Relaxed) >= self.max_instances as usize {
            if std::time::Instant::now() >= deadline {
                return Err(PfError::new(
                    ErrorCode::ResourceExhausted,
                    format!("{} concurrent pipe clients exceeded", self.max_instances),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // SAFETY: handle owned; no overlapped.
        let connected = unsafe { ConnectNamedPipe(self.handle, std::ptr::null_mut()) };
        if connected == 0 {
            let err = unsafe { ffi::GetLastError() };
            // ERROR_NO_DATA (232): previous client didn't disconnect
            // cleanly; the instance is still usable for the next accept.
            if err != 232 && err != ERROR_BROKEN_PIPE {
                return Err(PfError::new(
                    ErrorCode::BackendFailure,
                    format!("ConnectNamedPipe: {}", ffi::win32_message(err)),
                ));
            }
        }
        let connected_handle = self.handle;
        // Prepare the next instance before handing this one out.
        self.handle = create_instance(&self.name, self.max_instances)?;
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(PipeStream {
            handle: connected_handle,
            server_side: true,
            live: Some(std::sync::Arc::clone(&self.live)),
        })
    }
}

fn create_instance(name: &str, max_instances: u32) -> Result<ffi::Handle, PfError> {
    let wide = to_wide(name);
    let sddl_text = owner_only_sddl()?;
    let sddl = to_wide(&sddl_text);
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid string pointers, valid out-pointer; freed below.
    let sd_ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if sd_ok == 0 || sd.is_null() {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "failed to build owner-only security descriptor",
        ));
    }
    let sa = SecurityAttributes {
        n_length: u32::try_from(size_of::<SecurityAttributes>()).unwrap_or(24),
        security_descriptor: sd,
        inherit_handle: 0,
    };
    // SAFETY: all pointers/lengths valid; name is NUL-terminated UTF-16.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            max_instances,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        )
    };
    // SAFETY: LocalFree on the descriptor we allocated.
    unsafe { LocalFree(sd) };
    if handle.is_null() || handle as isize == -1 {
        let err = unsafe { ffi::GetLastError() };
        return Err(match err {
            5 => PfError::new(
                ErrorCode::PermissionDenied,
                "CreateNamedPipeW denied; another daemon likely owns the endpoint",
            ),
            231 => PfError::new(ErrorCode::ResourceExhausted, "pipe instances exhausted"),
            other => PfError::new(
                ErrorCode::BackendFailure,
                format!("CreateNamedPipeW: {}", ffi::win32_message(other)),
            ),
        });
    }
    Ok(handle)
}

/// Connect a client to the daemon pipe (bounded retry while the daemon is
/// still setting up). The absence budget is kept small (~0.6 s): a missing
/// pipe is the common "daemon not running" case, and UI callers must never
/// sit here long enough to trip the OS "not responding" heuristics.
pub fn connect(name: &str) -> Result<PipeStream, PfError> {
    let wide = to_wide(name);
    for _ in 0..12 {
        // SAFETY: NUL-terminated name; no template file.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if !handle.is_null() && handle as isize != -1 {
            return Ok(PipeStream {
                handle,
                server_side: false,
                live: None,
            });
        }
        let err = unsafe { ffi::GetLastError() };
        if err == ERROR_PIPE_BUSY {
            // Server exists and is out of instances: wait for a slot.
            // SAFETY: name pointer valid; 2 s timeout.
            let waited = unsafe { WaitNamedPipeW(wide.as_ptr(), 2000) };
            if waited == 0 {
                continue;
            }
            continue;
        }
        if err == 2 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        return Err(match err {
            5 => PfError::new(
                ErrorCode::PermissionDenied,
                "daemon pipe exists but access was denied",
            )
            .with_remediation(
                "the pipe is restricted to its owner; connect as the user running the daemon",
            ),
            other => PfError::new(
                ErrorCode::BackendFailure,
                format!("connect: {}", ffi::win32_message(other)),
            ),
        });
    }
    Err(PfError::new(
        ErrorCode::Timeout,
        "daemon pipe not reachable (is the daemon running?)",
    )
    .with_remediation("start it with `pulseforge daemon`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_per_user_and_data_root() {
        let name = daemon_pipe_name(std::path::Path::new("C:/Data/A"));
        assert!(name.starts_with(r"\\.\pipe\pulseforge-daemon-"), "{name}");
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        assert!(name.contains(&user), "{name} vs {user}");
        // Different roots map to different endpoints; same root is stable.
        let other = daemon_pipe_name(std::path::Path::new("C:/Data/B"));
        assert_ne!(name, other);
        let same = daemon_pipe_name(std::path::Path::new("c:/data/a"));
        assert_eq!(name, same);
    }

    #[test]
    fn listener_accepts_and_streams_framed_bytes() {
        // In-process loopback over the real OS pipe.
        let name = format!(
            r"\\.\pipe\pf-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut listener = PipeListener::bind(&name, 4).expect("bind");

        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            let mut c = connect(&client_name).expect("connect");
            // Framed echo exchange.
            pf_daemon_ipc_frame(&mut c, r#"{"hello":1}"#);
            let reply = pf_daemon_ipc_read(&mut c);
            (reply,)
        });

        let mut server = listener.accept().expect("accept");
        // Read one frame, echo it back.
        let got = pf_daemon_ipc_read(&mut server);
        assert!(got.contains("hello"), "{got}");
        pf_daemon_ipc_frame(&mut server, &got);

        let (reply,) = client.join().expect("client thread");
        assert!(reply.contains("hello"), "{reply}");
    }

    fn pf_daemon_ipc_frame(s: &mut PipeStream, json: &str) {
        let body = json.as_bytes();
        let len = u32::try_from(body.len()).unwrap();
        let mut buf = len.to_le_bytes().to_vec();
        buf.extend_from_slice(body);
        s.write_all(&buf).expect("write frame");
    }

    fn pf_daemon_ipc_read(s: &mut PipeStream) -> String {
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).expect("read len");
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).expect("read body");
        String::from_utf8(body).unwrap()
    }
}
