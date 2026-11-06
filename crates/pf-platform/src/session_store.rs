//! Schema-0 session store: `<id>.pfsession/` directory with `manifest.json`
//! and `samples.jsonl`.
//!
//! **Provisional by design** (roadmap PF-207: "transient in-memory ring
//! aggregation; no permanent JSONL-only contract"). This store exists so v0.1
//! sessions are inspectable offline and the report command reads real files.
//! The durable chunked format (PF-501..506) replaces it in milestone v0.3;
//! the reader here will migrate, not reinterpret, older schema-0 sessions.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use pf_types::json::{self, JsonValue};
use pf_types::{CounterSnapshot, ErrorCode, PfError, SessionId, SessionState};

pub const SCHEMA_0: SchemaVersion = SchemaVersion(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaVersion(pub u32);

impl SchemaVersion {
    pub fn as_str(self) -> &'static str {
        match self.0 {
            0 => "pulseforge.session/0",
            _ => "pulseforge.session/unknown",
        }
    }
}

/// Cap on one JSONL line before parsing (defensive; our own writer stays far
/// below this).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Maximum raw samples read back from a schema-0 file (bound on trust).
pub const MAX_SAMPLES_READ: usize = 1_000_000;

/// A session directory writer. Files are written via a `.partial` temp name
/// and atomically renamed; a stale `.partial` is evidence, never treated as
/// sealed data.
#[derive(Debug)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Create a new session directory `<root>/<session-id>.pfsession`.
    pub fn create(root: &Path, id: SessionId) -> Result<SessionStore, PfError> {
        let dir = root.join(format!("{}.pfsession", id.as_hyphenated()));
        fs::create_dir_all(&dir).map_err(PfError::from)?;
        Ok(SessionStore { dir })
    }

    /// Open an existing session directory for reading.
    pub fn open(dir: &Path) -> Result<SessionStore, PfError> {
        if !dir.is_dir() {
            return Err(PfError::new(
                ErrorCode::SessionNotFound,
                format!("session directory not found: {}", dir.display()),
            ));
        }
        Ok(SessionStore {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write the manifest atomically.
    pub fn write_manifest(&self, manifest: &JsonValue) -> Result<(), PfError> {
        let tmp = self.dir.join("manifest.json.partial");
        let final_path = self.dir.join("manifest.json");
        {
            let mut f = fs::File::create(&tmp).map_err(PfError::from)?;
            f.write_all(manifest.to_json_string().as_bytes())
                .map_err(PfError::from)?;
            f.write_all(b"\n").map_err(PfError::from)?;
            f.sync_all().map_err(PfError::from)?;
        }
        fs::rename(&tmp, &final_path).map_err(PfError::from)
    }

    /// Append raw snapshots as JSONL lines.
    pub fn append_samples(&self, snapshots: &[CounterSnapshot]) -> Result<(), PfError> {
        let path = self.dir.join("samples.jsonl");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(PfError::from)?;
        for s in snapshots {
            let line = s.to_json().to_json_string();
            writeln!(f, "{line}").map_err(PfError::from)?;
        }
        Ok(())
    }

    /// Read the parsed manifest.
    pub fn read_manifest(&self) -> Result<JsonValue, PfError> {
        let path = self.dir.join("manifest.json");
        let text = fs::read_to_string(&path).map_err(|e| {
            PfError::new(
                ErrorCode::SessionNotFound,
                format!("manifest unreadable at {}: {e}", path.display()),
            )
        })?;
        if text.len() > 4 * 1024 * 1024 {
            return Err(PfError::new(
                ErrorCode::ResourceExhausted,
                "manifest exceeds 4 MiB safety cap",
            ));
        }
        json::parse(&text).map_err(PfError::from)
    }

    /// Read raw snapshots. Stops at the bounded sample cap; corrupt lines are
    /// surfaced as `PARTIAL_DATA`, not silently skipped.
    // Line numbers are bounded by the file's own line count.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn read_samples(&self) -> Result<(Vec<CounterSnapshot>, Option<String>), PfError> {
        let path = self.dir.join("samples.jsonl");
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), None));
            }
            Err(e) => return Err(PfError::from(e)),
        };
        let mut out = Vec::new();
        let mut partial: Option<String> = None;
        let reader = BufReader::new(file);
        for (idx, line) in reader.lines().enumerate() {
            if out.len() >= MAX_SAMPLES_READ {
                partial.get_or_insert_with(|| {
                    format!("sample cap {MAX_SAMPLES_READ} reached while reading")
                });
                break;
            }
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    partial.get_or_insert_with(|| format!("read error at line {}: {e}", idx + 1));
                    break;
                }
            };
            if line.len() > MAX_LINE_BYTES {
                partial.get_or_insert_with(|| {
                    format!("line {} exceeds {} byte cap", idx + 1, MAX_LINE_BYTES)
                });
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            match json::parse(&line).and_then(|v| {
                CounterSnapshot::from_json(&v).map_err(|m| json::ParseError {
                    offset: 0,
                    message: m,
                })
            }) {
                Ok(s) => out.push(s),
                Err(e) => {
                    partial
                        .get_or_insert_with(|| format!("corrupt sample at line {}: {e}", idx + 1));
                    break;
                }
            }
        }
        Ok((out, partial))
    }
}

/// Build a schema-0 manifest value.
#[allow(clippy::too_many_arguments)]
pub fn manifest_v0(
    session_id: SessionId,
    state: SessionState,
    os: &str,
    os_version: &str,
    identity: &pf_types::ProcessIdentity,
    image_name: &str,
    logical_cpus: Option<u32>,
    interval_ms: u64,
    stop_reason: &str,
    warnings: &[String],
    schema: SchemaVersion,
) -> JsonValue {
    use JsonValue as J;
    J::Object(vec![
        ("schema".into(), J::Str(schema.as_str().into())),
        ("session_id".into(), J::Str(session_id.as_hyphenated())),
        ("state".into(), J::Str(state.as_str().into())),
        ("provisional".into(), J::Bool(matches!(schema, SCHEMA_0))),
        (
            "created_by".into(),
            J::Object(vec![
                ("tool".into(), J::Str("pulseforge".into())),
                ("version".into(), J::Str(env!("CARGO_PKG_VERSION").into())),
            ]),
        ),
        (
            "target".into(),
            J::Object(vec![
                ("os".into(), J::Str(os.into())),
                ("os_version".into(), J::Str(os_version.into())),
                ("pid".into(), J::UInt(u64::from(identity.pid))),
                (
                    "creation_identity".into(),
                    J::UInt(identity.creation_identity),
                ),
                (
                    "boot_identity".into(),
                    J::Str(identity.boot_identity.clone()),
                ),
                ("image".into(), J::Str(image_name.into())),
                (
                    "logical_cpus".into(),
                    match logical_cpus {
                        Some(n) => J::UInt(u64::from(n)),
                        None => J::Null,
                    },
                ),
            ]),
        ),
        ("interval_ms".into(), J::UInt(interval_ms)),
        ("stop_reason".into(), J::Str(stop_reason.into())),
        (
            "warnings".into(),
            J::Array(warnings.iter().map(|w| J::Str(w.clone())).collect()),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_types::ProcessIdentity;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pf-session-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn manifest_and_samples_roundtrip() {
        let root = temp_root("roundtrip");
        let id = SessionId::now_v7();
        let store = SessionStore::create(&root, id).unwrap();
        let identity = ProcessIdentity::new(pf_types::Os::Windows, 7, 99, "boot-x");
        let manifest = manifest_v0(
            id,
            SessionState::Completed,
            "windows",
            "10.0.22631",
            &identity,
            "test.exe",
            Some(8),
            250,
            "duration_reached",
            &[],
            SCHEMA_0,
        );
        store.write_manifest(&manifest).unwrap();
        let samples = vec![
            CounterSnapshot {
                mono_ns: 0,
                cpu_user_ns: Some(10),
                resident_bytes: Some(4096),
                ..CounterSnapshot::default()
            },
            CounterSnapshot {
                mono_ns: 250_000_000,
                cpu_user_ns: Some(250_000_010),
                ..CounterSnapshot::default()
            },
        ];
        store.append_samples(&samples).unwrap();

        let read_back = SessionStore::open(store.dir()).unwrap();
        let m = read_back.read_manifest().unwrap();
        assert_eq!(
            m.get("schema").and_then(|v| v.as_str()),
            Some(SCHEMA_0.as_str())
        );
        assert_eq!(m.get("state").and_then(|v| v.as_str()), Some("COMPLETED"));
        let (s, partial) = read_back.read_samples().unwrap();
        assert!(partial.is_none());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], samples[0]);
        assert_eq!(s[1], samples[1]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_line_is_partial_not_silent() {
        let root = temp_root("corrupt");
        let id = SessionId::now_v7();
        let store = SessionStore::create(&root, id).unwrap();
        let good = CounterSnapshot {
            mono_ns: 1,
            ..CounterSnapshot::default()
        };
        store.append_samples(&[good]).unwrap();
        // Hand-write a corrupt line.
        let path = store.dir.join("samples.jsonl");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{not json").unwrap();
        drop(f);

        let (s, partial) = store.read_samples().unwrap();
        assert_eq!(s.len(), 1);
        let partial = partial.expect("corruption must be surfaced");
        assert!(partial.contains("corrupt sample"), "{partial}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_dir_is_session_not_found() {
        let err = SessionStore::open(Path::new("Z:/definitely/not/here")).unwrap_err();
        assert_eq!(err.code, ErrorCode::SessionNotFound);
    }

    #[test]
    fn missing_samples_file_is_empty_not_error() {
        let root = temp_root("empty");
        let id = SessionId::now_v7();
        let store = SessionStore::create(&root, id).unwrap();
        let (s, partial) = store.read_samples().unwrap();
        assert!(s.is_empty());
        assert!(partial.is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
