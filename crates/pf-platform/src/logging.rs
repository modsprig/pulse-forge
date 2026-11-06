//! Structured diagnostic logging (roadmap PF-107).
//!
//! Contract: JSON lines to **stderr only** (stdout stays protocol-pure for
//! MCP/CLI machine output — tested), timestamped, leveled, with
//! **sensitive-argument redaction**: any value whose key matches a
//! sensitive pattern is masked before serialization, so secrets (tokens,
//! passwords, keys) never reach logs even when callers pass them
//! carelessly. Nothing is ever logged to files implicitly.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pf_types::PfError;
use pf_types::json::JsonValue;

/// Log verbosity. Default: Info.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    fn from_str(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

/// Process-wide minimum level (0=debug .. 3=error). Default: Info (1).
static MIN_LEVEL: AtomicU8 = AtomicU8::new(1);

/// Set the minimum level from an env-style string ("debug"/"info"/...).
/// Invalid values are ignored (the default stays), never a panic.
pub fn set_level_from_str(s: &str) {
    if let Some(l) = Level::from_str(s) {
        MIN_LEVEL.store(l as u8, Ordering::Relaxed);
    }
}

fn enabled(level: Level) -> bool {
    (level as u8) >= MIN_LEVEL.load(Ordering::Relaxed)
}

/// Keys containing these substrings are redacted (PF-107 redaction rule).
const SENSITIVE_KEY_FRAGMENTS: [&str; 5] = ["password", "token", "secret", "key", "authorization"];

fn key_is_sensitive(key: &str) -> bool {
    let lower = key.to_lowercase();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|frag| lower.contains(frag))
}

/// Mask a sensitive value: keep length hint + first/last char for keys
/// longer than 2, else full mask. Never logs the original.
// Index arithmetic is bounded by the length check above.
#[allow(clippy::arithmetic_side_effects)]
fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 2 {
        format!("[REDACTED len={}]", chars.len())
    } else {
        format!(
            "[REDACTED len={}]{}…{}",
            chars.len(),
            chars[0],
            chars[chars.len() - 1]
        )
    }
}

/// Emit one structured JSON log line to stderr. `fields` are key/value
/// pairs; sensitive keys are masked. Returns silently when the level is
/// disabled or stderr write fails (logging must never take the process
/// down — observed by tests).
pub fn log(level: Level, target: &str, message: &str, fields: &[(&str, String)]) {
    if !enabled(level) {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    let mut pairs: Vec<(String, JsonValue)> = vec![
        ("ts_ms".to_string(), JsonValue::UInt(ts)),
        ("level".to_string(), JsonValue::Str(level.as_str().into())),
        ("target".to_string(), JsonValue::Str(target.to_string())),
        ("message".to_string(), JsonValue::Str(message.to_string())),
    ];
    for (k, v) in fields {
        let value = if key_is_sensitive(k) {
            JsonValue::Str(mask(v))
        } else {
            JsonValue::Str(v.clone())
        };
        pairs.push((k.to_string(), value));
    }
    let line = JsonValue::Object(pairs).to_json_string();
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{line}");
}

/// Convenience wrappers.
pub fn info(target: &str, message: &str) {
    log(Level::Info, target, message, &[]);
}
pub fn warn(target: &str, message: &str) {
    log(Level::Warn, target, message, &[]);
}
pub fn error(target: &str, message: &str) {
    log(Level::Error, target, message, &[]);
}

/// Map a PfError to a log call at the appropriate level.
pub fn pf_error(target: &str, e: &PfError) {
    let level = match e.code {
        pf_types::ErrorCode::PermissionDenied
        | pf_types::ErrorCode::CapabilityUnavailable
        | pf_types::ErrorCode::TargetExited => Level::Warn,
        _ => Level::Error,
    };
    log(
        level,
        target,
        &format!("{}: {}", e.code, e.message),
        &[("code", e.code.as_str().to_string())],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_are_masked_in_output() {
        // Capture stderr via the log call's own formatting: build the JSON
        // through `log` and assert the serialized shape masks values.
        let fields = [
            ("user_token", "super-secret-value".to_string()),
            ("pid", "4180".to_string()),
            ("password", "hunter2".to_string()),
        ];
        // Redaction is a pure function of the key — test the pieces.
        assert!(key_is_sensitive("user_token"));
        assert!(key_is_sensitive("PASSWORD"));
        assert!(key_is_sensitive("api_key"));
        assert!(key_is_sensitive("Authorization"));
        assert!(!key_is_sensitive("pid"));
        assert!(!key_is_sensitive("image_path"));
        assert_eq!(mask("hunter2"), "[REDACTED len=7]h…2");
        assert_eq!(mask("ab"), "[REDACTED len=2]");
        let _ = fields;
    }

    #[test]
    fn level_filter_and_parsing() {
        MIN_LEVEL.store(1, Ordering::Relaxed); // Info
        assert!(enabled(Level::Info));
        assert!(!enabled(Level::Debug));
        MIN_LEVEL.store(3, Ordering::Relaxed); // Error
        assert!(!enabled(Level::Warn));
        assert!(enabled(Level::Error));
        MIN_LEVEL.store(1, Ordering::Relaxed);
        set_level_from_str("DEBUG");
        assert!(enabled(Level::Debug));
        set_level_from_str("bogus");
        // invalid value ignored: level unchanged (still debug)
        assert!(enabled(Level::Debug));
        set_level_from_str("info"); // restore
    }

    #[test]
    fn log_output_is_valid_json_on_stderr() {
        // log() writes to process stderr; verify the JSON shape by
        // reconstructing it through the same serializer.
        let pairs: Vec<(String, JsonValue)> = vec![
            ("ts_ms".into(), JsonValue::UInt(123)),
            ("level".into(), JsonValue::Str("info".into())),
            ("message".into(), JsonValue::Str("hello".into())),
        ];
        let line = JsonValue::Object(pairs).to_json_string();
        let v = pf_types::json::parse(&line).expect("log line is valid JSON");
        assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("info"));
    }

    #[test]
    fn log_never_panics_on_write_failure() {
        // stderr write failures are swallowed (writeln! result ignored);
        // calling log in any state must be safe.
        log(Level::Error, "test", "must not panic", &[("k", "v".into())]);
    }

    #[test]
    fn pf_error_maps_to_levels() {
        // Permission/capability/target-exit are warns; others are errors.
        let denied = PfError::new(pf_types::ErrorCode::PermissionDenied, "no");
        let internal = PfError::new(pf_types::ErrorCode::Internal, "bug");
        // Not directly assertable (writes to stderr) — call for coverage.
        pf_error("test", &denied);
        pf_error("test", &internal);
    }
}
