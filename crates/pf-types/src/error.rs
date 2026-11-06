//! Typed error taxonomy for all PulseForge layers.
//!
//! Rules (architecture §15): stable machine-readable codes, human-readable
//! context, remediation hints where useful, and **no panics across trust
//! boundaries**. Error codes are part of the public contract and must only
//! gain members.

use core::fmt;

/// Stable machine-readable error codes (architecture §15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// Malformed argument (bad PID, out-of-range interval, bad path shape).
    InvalidArgument,
    /// Caller is not permitted by the OS to access the target resource.
    PermissionDenied,
    /// The requested profile mode is unavailable on this machine; a lesser
    /// mode may still work (always paired with an explanation).
    CapabilityUnavailable,
    /// The target process exited before or during the operation.
    TargetExited,
    /// The process identity changed (PID reuse or different creation time).
    IdentityMismatch,
    /// Referenced session does not exist or is unreadable.
    SessionNotFound,
    /// Data is present but incomplete; results carry caveats.
    PartialData,
    /// A configured resource limit (quota, size, duration) was hit.
    ResourceExhausted,
    /// An operation exceeded its time budget.
    Timeout,
    /// A platform backend failed in a way not covered above.
    BackendFailure,
    /// Local I/O failure (disk, permissions on our own storage).
    StorageIo,
    /// Operation is not implemented for this platform/build.
    UnsupportedPlatform,
    /// Internal invariant violation — always a bug; please report it.
    Internal,
}

impl ErrorCode {
    /// Stable string form used in JSON output and (later) IPC/MCP errors.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::CapabilityUnavailable => "CAPABILITY_UNAVAILABLE",
            ErrorCode::TargetExited => "TARGET_EXITED",
            ErrorCode::IdentityMismatch => "IDENTITY_MISMATCH",
            ErrorCode::SessionNotFound => "SESSION_NOT_FOUND",
            ErrorCode::PartialData => "PARTIAL_DATA",
            ErrorCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::BackendFailure => "BACKEND_FAILURE",
            ErrorCode::StorageIo => "STORAGE_IO",
            ErrorCode::UnsupportedPlatform => "UNSUPPORTED_PLATFORM",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A PulseForge error: code + message + optional detail and remediation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PfError {
    pub code: ErrorCode,
    pub message: String,
    pub remediation: Option<String>,
}

impl PfError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        PfError {
            code,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}

impl fmt::Display for PfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(r) = &self.remediation {
            write!(f, " ({r})")?;
        }
        Ok(())
    }
}

impl std::error::Error for PfError {}

impl From<std::io::Error> for PfError {
    fn from(err: std::io::Error) -> Self {
        let code = match err.kind() {
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            std::io::ErrorKind::NotFound => ErrorCode::SessionNotFound,
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                ErrorCode::InvalidArgument
            }
            _ => ErrorCode::StorageIo,
        };
        PfError::new(code, err.to_string())
    }
}

impl From<crate::json::ParseError> for PfError {
    fn from(err: crate::json::ParseError) -> Self {
        PfError::new(ErrorCode::InvalidArgument, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_strings() {
        // These strings are a public contract; changing any is a breaking
        // change. Snapshot them so edits are deliberate.
        let expected = [
            ("INVALID_ARGUMENT", ErrorCode::InvalidArgument),
            ("PERMISSION_DENIED", ErrorCode::PermissionDenied),
            ("CAPABILITY_UNAVAILABLE", ErrorCode::CapabilityUnavailable),
            ("TARGET_EXITED", ErrorCode::TargetExited),
            ("IDENTITY_MISMATCH", ErrorCode::IdentityMismatch),
            ("SESSION_NOT_FOUND", ErrorCode::SessionNotFound),
            ("PARTIAL_DATA", ErrorCode::PartialData),
            ("RESOURCE_EXHAUSTED", ErrorCode::ResourceExhausted),
            ("TIMEOUT", ErrorCode::Timeout),
            ("BACKEND_FAILURE", ErrorCode::BackendFailure),
            ("STORAGE_IO", ErrorCode::StorageIo),
            ("UNSUPPORTED_PLATFORM", ErrorCode::UnsupportedPlatform),
            ("INTERNAL", ErrorCode::Internal),
        ];
        for (s, code) in expected {
            assert_eq!(code.as_str(), s);
            assert_eq!(code.to_string(), s);
        }
    }

    #[test]
    fn display_includes_remediation() {
        let e = PfError::new(ErrorCode::PermissionDenied, "OpenProcess denied for pid 4")
            .with_remediation("protected processes cannot be profiled without elevation");
        assert_eq!(
            e.to_string(),
            "PERMISSION_DENIED: OpenProcess denied for pid 4 \
             (protected processes cannot be profiled without elevation)"
        );
    }

    #[test]
    fn io_error_maps_to_typed_codes() {
        let e = PfError::from(std::io::Error::other("disk on fire"));
        assert_eq!(e.code, ErrorCode::StorageIo);
        let e = PfError::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    }
}
