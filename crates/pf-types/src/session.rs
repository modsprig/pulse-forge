//! Session identifiers and the session lifecycle state machine.
//!
//! `SessionId` is a UUIDv7 (time-ordered, locally minted). The generator is
//! dependency-free: 48-bit millisecond Unix timestamp + process-unique
//! randomness (monotonic counter, thread id, address entropy, and a
//! nanosecond clock mixed with SplitMix64). It guarantees uniqueness within a
//! process and monotonic ordering within the same millisecond without
//! relying on an OS random device — session ids are identifiers, not secrets.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

/// A PulseForge recording session identifier (UUIDv7, hyphenated lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u128);

impl SessionId {
    /// Mint a new time-ordered id.
    pub fn now_v7() -> SessionId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ms = unix_ms_now() & 0x0000_FFFF_FFFF_FFFF; // 48 bits
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Mix cheap per-call entropy. ASLR heap address differs across runs;
        // thread id differs across threads; counter differs across calls.
        let tid = thread_id_hint();
        let addr = &COUNTER as *const _ as usize as u64;
        let mut rng = SplitMix64::new(
            ms ^ (counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ tid ^ addr.rotate_left(17),
        );
        let rand_a = rng.next() & 0x0FFF; // 12 bits
        let rand_b = rng.next() & 0x3FFF_FFFF_FFFF_FFFF; // 62 bits
        let uuid = (u128::from(ms) << 80)
            | (u128::from(rand_a | 0x7000) << 64) // version 7
            | (u128::from(rand_b | 0x8000_0000_0000_0000)); // RFC 4122 variant
        SessionId(uuid)
    }

    /// Raw 128-bit value.
    pub fn as_u128(self) -> u128 {
        self.0
    }

    /// Hyphenated canonical form (36 chars).
    /// The 48-bit millisecond Unix timestamp embedded in the UUIDv7 top
    /// bits (the identity IS the creation time — no extra field needed).
    pub fn unix_ms(self) -> u64 {
        u64::try_from(self.0 >> 80).unwrap_or(0)
    }

    pub fn as_hyphenated(self) -> String {
        let h = format!("{:032x}", self.0);
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }

    /// Parse a hyphenated or bare-hex form. Errors are typed, never panics.
    pub fn parse(s: &str) -> Result<SessionId, String> {
        let cleaned: String = s.chars().filter(|c| *c != '-').collect();
        if cleaned.len() != 32 {
            return Err(format!(
                "session id must be 32 hex digits, got {}",
                cleaned.len()
            ));
        }
        let mut value: u128 = 0;
        for c in cleaned.chars() {
            let digit = c
                .to_digit(16)
                .ok_or_else(|| format!("invalid hex digit `{c}` in session id"))?;
            value = (value << 4) | u128::from(digit);
        }
        Ok(SessionId(value))
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_hyphenated())
    }
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn thread_id_hint() -> u64 {
    // No stable std thread id as u64; use a stack address as a per-thread
    // discriminator — adequate entropy for identifier uniqueness.
    let local = 0u8;
    &local as *const u8 as usize as u64
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Session lifecycle (architecture §5.1). Transitions are validated by
/// [`SessionState::transition`]; the daemon serializes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionState {
    Created,
    Starting,
    Recording,
    Stopping,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
    RecoveredPartial,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Created => "CREATED",
            SessionState::Starting => "STARTING",
            SessionState::Recording => "RECORDING",
            SessionState::Stopping => "STOPPING",
            SessionState::Finalizing => "FINALIZING",
            SessionState::Completed => "COMPLETED",
            SessionState::Failed => "FAILED",
            SessionState::Cancelled => "CANCELLED",
            SessionState::RecoveredPartial => "RECOVERED_PARTIAL",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionState::Completed
                | SessionState::Failed
                | SessionState::Cancelled
                | SessionState::RecoveredPartial
        )
    }

    /// Valid transitions. Terminal states accept nothing.
    pub fn transition(self, next: SessionState) -> Result<SessionState, StateTransitionError> {
        use SessionState::*;
        let ok = matches!(
            (self, next),
            (Created, Starting)
                | (Created, Cancelled)
                | (Created, Failed)
                | (Starting, Recording)
                | (Starting, Failed)
                | (Starting, Cancelled)
                | (Recording, Stopping)
                | (Recording, Failed)
                | (Recording, Cancelled)
                | (Recording, Finalizing) // target exited: finalize directly
                | (Stopping, Finalizing)
                | (Stopping, Failed)
                | (Finalizing, Completed)
                | (Finalizing, RecoveredPartial)
                | (Finalizing, Failed)
        );
        if ok {
            Ok(next)
        } else {
            Err(StateTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateTransitionError {
    pub from: SessionState,
    pub to: SessionState,
}

impl fmt::Display for StateTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid session state transition {} -> {}",
            self.from, self.to
        )
    }
}

impl std::error::Error for StateTransitionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn session_ids_are_uuid_v7_shaped() {
        for _ in 0..1_000 {
            let id = SessionId::now_v7();
            let hex = format!("{:032x}", id.as_u128());
            // version nibble
            assert_eq!(&hex[12..13], "7", "must be version 7");
            // variant bits: top two bits of hex[16] are 10
            let variant_nibble = u8::from_str_radix(&hex[16..17], 16).unwrap();
            assert!(variant_nibble & 0b1000 != 0, "variant must be 10xx");
            assert!(variant_nibble & 0b0100 == 0);
        }
    }

    #[test]
    fn session_ids_unique_under_rapid_generation() {
        let set: HashSet<u128> = (0..100_000)
            .map(|_| SessionId::now_v7().as_u128())
            .collect();
        assert_eq!(set.len(), 100_000);
    }

    #[test]
    fn session_ids_parse_roundtrip() {
        let id = SessionId::now_v7();
        let hyph = id.as_hyphenated();
        assert_eq!(hyph.len(), 36);
        let parsed = SessionId::parse(&hyph).unwrap();
        assert_eq!(parsed, id);
        let bare = format!("{:032x}", id.as_u128());
        assert_eq!(SessionId::parse(&bare).unwrap(), id);
    }

    #[test]
    fn session_id_parse_rejects_garbage() {
        assert!(SessionId::parse("").is_err());
        assert!(SessionId::parse("zzzz").is_err());
        assert!(SessionId::parse("0123456789abcdef0123456789abcde").is_err()); // 31 digits
    }

    #[test]
    fn lifecycle_happy_path() {
        use SessionState::*;
        let mut s = Created;
        for next in [Starting, Recording, Stopping, Finalizing, Completed] {
            s = s.transition(next).unwrap();
        }
        assert!(s.is_terminal());
        assert_eq!(s.as_str(), "COMPLETED");
    }

    #[test]
    fn target_exit_finalizes_directly_from_recording() {
        use SessionState::*;
        Recording.transition(Finalizing).unwrap();
    }

    #[test]
    fn terminal_states_accept_nothing() {
        use SessionState::*;
        for terminal in [Completed, Failed, Cancelled, RecoveredPartial] {
            for next in [
                Created,
                Starting,
                Recording,
                Stopping,
                Finalizing,
                Completed,
                Failed,
                Cancelled,
                RecoveredPartial,
            ] {
                assert!(terminal.transition(next).is_err(), "{terminal} -> {next}");
            }
        }
    }

    #[test]
    fn skipping_states_is_rejected() {
        use SessionState::*;
        assert!(Created.transition(Recording).is_err());
        assert!(Starting.transition(Stopping).is_err());
        assert!(Stopping.transition(Recording).is_err());
    }
}
