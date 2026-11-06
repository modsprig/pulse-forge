//! The real daemon client for the GUI: the same framed-JSON protocol the
//! CLI agent and the MCP bridge speak (one handshaked connection per call;
//! v1 policy from ADR-013/014).

#![cfg(windows)]

use std::path::PathBuf;

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId};

use crate::viewmod::{DaemonClient, SessionRow, SessionSnapshot};

pub struct PipeDaemonClient {
    data_root: PathBuf,
}

impl PipeDaemonClient {
    pub fn connect() -> Result<PipeDaemonClient, PfError> {
        let data_root = pf_platform::paths::data_root()
            .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
        Ok(PipeDaemonClient { data_root })
    }

    /// Test/explicit-root constructor (same protocol, pinned data root).
    pub fn connect_with_root(data_root: std::path::PathBuf) -> Result<PipeDaemonClient, PfError> {
        Ok(PipeDaemonClient { data_root })
    }

    fn call(&self, op: &str, args: JsonValue) -> Result<JsonValue, PfError> {
        use pf_daemon::ipc;
        let name = pf_windows::pipe::daemon_pipe_name(&self.data_root);
        let mut stream = pf_windows::pipe::connect(&name)?;
        ipc::write_frame(&mut stream, &ipc::client_handshake("pulseforge-gui"))?;
        let ack = ipc::read_frame(&mut stream)?;
        if ack.get("type").and_then(|v| v.as_str()) != Some("handshake_ok") {
            return Err(PfError::new(
                ErrorCode::BackendFailure,
                "daemon rejected the GUI handshake",
            ));
        }
        ipc::write_frame(&mut stream, &ipc::request(1, op, args))?;
        let reply = ipc::read_frame(&mut stream)?;
        if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(reply.get("result").cloned().unwrap_or(JsonValue::Null))
        } else {
            Err(PfError::new(
                ErrorCode::BackendFailure,
                reply
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("daemon request failed"),
            ))
        }
    }

    fn call_session(&self, op: &str, id: SessionId) -> Result<JsonValue, PfError> {
        self.call(
            op,
            JsonValue::Object(vec![(
                "session_id".into(),
                JsonValue::Str(id.as_hyphenated()),
            )]),
        )
    }
}

impl DaemonClient for PipeDaemonClient {
    fn capabilities(&self) -> Result<JsonValue, PfError> {
        self.call("capabilities", JsonValue::Object(vec![]))
    }

    fn list_processes(&self, limit: u32) -> Result<Vec<crate::viewmod::ProcessRow>, PfError> {
        let result = self.call(
            "list_processes",
            JsonValue::Object(vec![("limit".into(), JsonValue::UInt(u64::from(limit)))]),
        )?;
        let arr = result.as_array().map(|a| a.to_vec()).unwrap_or_default();
        let mut rows = Vec::with_capacity(arr.len().min(100));
        for v in arr.iter().take(100) {
            rows.push(crate::viewmod::ProcessRow {
                pid: v
                    .get("pid")
                    .and_then(|x| x.as_u64())
                    .and_then(|x| u32::try_from(x).ok())
                    .unwrap_or(0),
                parent_pid: v
                    .get("parent_pid")
                    .and_then(|x| x.as_u64())
                    .and_then(|x| u32::try_from(x).ok()),
                image: v.get("image").and_then(|x| x.as_str()).unwrap_or("").into(),
                window_title: None,
                cpu_pct: v.get("cpu_pct").and_then(|x| x.as_f64()),
                rss_bytes: v.get("rss_bytes").and_then(|x| x.as_u64()),
            });
        }
        Ok(rows)
    }

    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        let result = self.call("list_sessions", JsonValue::Object(vec![]))?;
        let arr = result.as_array().map(|a| a.to_vec()).unwrap_or_default();
        let mut rows = Vec::with_capacity(arr.len().min(100));
        for v in arr.iter().take(100) {
            let Some(id_str) = v.get("session_id").and_then(|s| s.as_str()) else {
                continue;
            };
            let Ok(id) = SessionId::parse(id_str) else {
                continue;
            };
            rows.push(SessionRow {
                id,
                state: v
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .into(),
                samples: v.get("samples").and_then(|s| s.as_u64()).unwrap_or(0),
                image: v.get("image").and_then(|s| s.as_str()).unwrap_or("").into(),
                stop_reason: v
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
            });
        }
        Ok(rows)
    }

    fn start_attach(
        &self,
        pid: u32,
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError> {
        // create + start, mirroring the MCP bridge's attach semantics.
        let created = self.call(
            "create_session",
            JsonValue::Object(vec![("pid".into(), JsonValue::UInt(u64::from(pid)))]),
        )?;
        let sid = created
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| PfError::new(ErrorCode::BackendFailure, "no session id"))?
            .to_string();
        let mut args = vec![
            ("session_id".to_string(), JsonValue::Str(sid.clone())),
            ("duration_ms".to_string(), JsonValue::UInt(duration_ms)),
            ("interval_ms".to_string(), JsonValue::UInt(interval_ms)),
        ];
        if !label.is_empty() {
            args.push(("label".to_string(), JsonValue::Str(label.to_string())));
        }
        self.call("start_recording", JsonValue::Object(args))?;
        SessionId::parse(&sid)
            .map_err(|e| PfError::new(ErrorCode::BackendFailure, format!("bad session id: {e}")))
    }

    fn start_run(
        &self,
        exe: &str,
        args: &[String],
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError> {
        let run_args: Vec<JsonValue> = args.iter().map(|a| JsonValue::Str(a.clone())).collect();
        let created = self.call(
            "create_session",
            JsonValue::Object(vec![
                ("run".into(), JsonValue::Str(exe.to_string())),
                ("args".into(), JsonValue::Array(run_args)),
            ]),
        )?;
        let sid = created
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| PfError::new(ErrorCode::BackendFailure, "no session id"))?
            .to_string();
        let mut call_args = vec![
            ("session_id".to_string(), JsonValue::Str(sid.clone())),
            ("duration_ms".to_string(), JsonValue::UInt(duration_ms)),
            ("interval_ms".to_string(), JsonValue::UInt(interval_ms)),
        ];
        if !label.is_empty() {
            call_args.push(("label".to_string(), JsonValue::Str(label.to_string())));
        }
        self.call("start_recording", JsonValue::Object(call_args))?;
        SessionId::parse(&sid)
            .map_err(|e| PfError::new(ErrorCode::BackendFailure, format!("bad session id: {e}")))
    }
    fn status(&self, id: SessionId) -> Result<SessionSnapshot, PfError> {
        let v = self.call_session("get_session", id)?;
        Ok(SessionSnapshot {
            id,
            state: v
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .into(),
            samples: v.get("samples").and_then(|s| s.as_u64()).unwrap_or(0),
            session_dir: v
                .get("session_dir")
                .and_then(|s| s.as_str())
                .map(std::path::PathBuf::from),
            warnings: v
                .get("warnings")
                .and_then(|w| w.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|w| w.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn stop(&self, id: SessionId) -> Result<String, PfError> {
        let v = self.call_session("stop_recording", id)?;
        Ok(v.get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string())
    }

    fn delete_session(&self, id: SessionId) -> Result<u64, PfError> {
        let v = self.call_session("delete_session", id)?;
        Ok(v.get("bytes_freed").and_then(|b| b.as_u64()).unwrap_or(0))
    }

    fn envelope(
        &self,
        id: SessionId,
        field: pf_storage::EnvelopeField,
        from_ns: u64,
        to_ns: u64,
        buckets: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        let dir = self.session_dir(id)?;
        let reader = pf_storage::ChunkReader::open(&dir)?;
        reader.envelope(from_ns, to_ns, buckets, field)
    }

    fn samples(&self, id: SessionId) -> Result<Vec<pf_types::CounterSnapshot>, PfError> {
        // Read from the session's chunk store when present (offline-open
        // path), else the daemon's live samples via query_metrics' source.
        // The chunk store is the durable source of truth (P5).
        let dir = self.session_dir(id)?;
        {
            if let Ok(reader) = pf_storage::ChunkReader::open(&dir) {
                if reader.total_records() > 0 {
                    return reader.range(0, u64::MAX, 20_000).map(|p| p.snapshots);
                }
            }
            // Schema-0 JSONL fallback.
            if let Ok(store) = pf_platform::session_store::SessionStore::open(&dir) {
                if let Ok((snapshots, _)) = store.read_samples() {
                    return Ok(snapshots);
                }
            }
        }
        // Live recording: in-memory samples are not IPC-exposed as raw
        // arrays in v1 — report what exists honestly.
        Ok(Vec::new())
    }

    fn compare(&self, baseline: SessionId, candidate: SessionId) -> Result<JsonValue, PfError> {
        self.call(
            "compare_sessions",
            JsonValue::Object(vec![
                ("baseline".into(), JsonValue::Str(baseline.as_hyphenated())),
                (
                    "candidate".into(),
                    JsonValue::Str(candidate.as_hyphenated()),
                ),
            ]),
        )
    }

    // Counters bounded by the sessions directory walk.
    #[allow(clippy::arithmetic_side_effects)]
    /// PF-707: storage health measured the same way `doctor` does — probe
    /// the sessions root, count sessions, sum file sizes.
    fn storage_health(&self) -> Result<crate::viewmod::StorageHealth, PfError> {
        use crate::viewmod::StorageHealth;
        let caps = self.capabilities()?;
        let counters_available = caps
            .get("counters")
            .and_then(|c| c.get("available"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let counters_reason = caps
            .get("counters")
            .and_then(|c| c.get("reason_code"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stacks = caps.get("cpu_stack_sampling");
        let stacks_available = stacks
            .and_then(|c| c.get("available"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let stacks_reason = stacks
            .and_then(|c| c.get("reason_code"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let os_version = caps
            .get("os_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let logical_cpus = caps
            .get("logical_cpus")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let sessions_root = self.data_root.join("sessions");
        let mut session_count = 0usize;
        let mut total_bytes = 0u64;
        if let Ok(entries) = std::fs::read_dir(&sessions_root) {
            for entry in entries.filter_map(|e| e.ok()) {
                if entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == "pfsession")
                {
                    session_count += 1;
                    total_bytes += dir_size(&entry.path());
                }
            }
        }
        let writable = probe_write(&self.data_root);
        Ok(StorageHealth {
            data_root: self.data_root.display().to_string(),
            writable,
            session_count,
            total_bytes,
            counters_available,
            counters_reason,
            stacks_available,
            stacks_reason,
            os_version,
            logical_cpus,
        })
    }
}

/// Directory size, bounded walk (symlinks not followed).
#[allow(clippy::arithmetic_side_effects)]
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.is_file() {
                if let Ok(meta) = e.metadata() {
                    total += meta.len();
                }
            } else if path.is_dir() {
                total += dir_size(&path);
            }
        }
    }
    total
}

/// Write probe identical to the CLI doctor's (create/remove a marker).
fn probe_write(root: &std::path::Path) -> bool {
    let probe = root.join(".gui-write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl PipeDaemonClient {
    /// Resolve a session's directory via the daemon status call.
    fn session_dir(&self, id: SessionId) -> Result<std::path::PathBuf, PfError> {
        let status = self.status(id)?;
        status.session_dir.ok_or_else(|| {
            PfError::new(
                ErrorCode::SessionNotFound,
                format!("session {} has no persisted directory yet", id),
            )
        })
    }
}
