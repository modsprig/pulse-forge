//! Stable process identity: PID alone is not identity.
//!
//! Architecture §5.1: identity is `{os, pid, creation_identity, boot_identity}`.
//! A PID is only meaningful together with the process creation timestamp and a
//! boot discriminator; collectors recheck identity on every snapshot so a
//! reused PID can never silently feed a new process's counters into an ongoing
//! session.

use core::fmt;

/// Operating system the identity was minted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Os {
    Windows,
    Linux,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Linux => "linux",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of comparing two identities for the same nominal PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityCheck {
    /// Same PID, same creation timestamp, same boot context.
    Match,
    /// Same PID but different creation time or boot context: PID was reused.
    Mismatch,
    /// One side lacked enough information to decide.
    Unknown,
}

/// Identity of one process instance across a single boot.
///
/// `creation_identity` is the OS-native creation token:
/// - Windows: process creation FILETIME (100 ns units since 1601-01-01),
/// - Linux (planned): `/proc/<pid>/stat` field 22 (`starttime` in clock ticks).
///
/// `boot_identity` discriminates across reboots. It is intentionally a coarse
/// string (each backend documents its precision) — it never needs to be exact,
/// only different across boots.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub os: Os,
    pub pid: u32,
    pub creation_identity: u64,
    pub boot_identity: String,
}

impl ProcessIdentity {
    pub fn new(os: Os, pid: u32, creation_identity: u64, boot_identity: impl Into<String>) -> Self {
        ProcessIdentity {
            os,
            pid,
            creation_identity,
            boot_identity: boot_identity.into(),
        }
    }

    /// Compare against another identity nominally referring to the same PID.
    pub fn check(&self, other: &ProcessIdentity) -> IdentityCheck {
        if self.pid != other.pid || self.os != other.os {
            return IdentityCheck::Mismatch;
        }
        if self.creation_identity == 0 || other.creation_identity == 0 {
            return IdentityCheck::Unknown;
        }
        if self.boot_identity.is_empty() || other.boot_identity.is_empty() {
            return IdentityCheck::Unknown;
        }
        if self.creation_identity == other.creation_identity
            && self.boot_identity == other.boot_identity
        {
            IdentityCheck::Match
        } else {
            IdentityCheck::Mismatch
        }
    }
}

impl fmt::Display for ProcessIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}#{}@{}+{}",
            self.os, self.pid, self.creation_identity, self.boot_identity
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(pid: u32, creation: u64) -> ProcessIdentity {
        ProcessIdentity::new(Os::Windows, pid, creation, "boot-2026-10-24T12:00")
    }

    #[test]
    fn same_pid_and_creation_matches() {
        assert_eq!(ident(10, 100).check(&ident(10, 100)), IdentityCheck::Match);
    }

    #[test]
    fn pid_reuse_detected() {
        // PID 10 exits; a new process gets PID 10 with a later creation time.
        assert_eq!(
            ident(10, 100).check(&ident(10, 900)),
            IdentityCheck::Mismatch
        );
    }

    #[test]
    fn different_pid_is_mismatch() {
        assert_eq!(
            ident(10, 100).check(&ident(11, 100)),
            IdentityCheck::Mismatch
        );
    }

    #[test]
    fn cross_boot_same_creation_is_mismatch() {
        let a = ProcessIdentity::new(Os::Windows, 10, 100, "boot-a");
        let b = ProcessIdentity::new(Os::Windows, 10, 100, "boot-b");
        assert_eq!(a.check(&b), IdentityCheck::Mismatch);
    }

    #[test]
    fn missing_fields_are_unknown_not_match() {
        let missing_creation = ProcessIdentity::new(Os::Windows, 10, 0, "boot-a");
        let missing_boot = ProcessIdentity::new(Os::Windows, 10, 100, "");
        assert_eq!(
            ident(10, 100).check(&missing_creation),
            IdentityCheck::Unknown
        );
        assert_eq!(ident(10, 100).check(&missing_boot), IdentityCheck::Unknown);
    }

    #[test]
    fn cross_os_never_matches() {
        let w = ProcessIdentity::new(Os::Windows, 10, 100, "boot");
        let l = ProcessIdentity::new(Os::Linux, 10, 100, "boot");
        assert_eq!(w.check(&l), IdentityCheck::Mismatch);
    }
}
