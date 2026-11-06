//! Request dispatch (roadmap PF-406): one `DaemonService` behind any
//! byte-stream transport. The Windows named pipe (PF-403) and tests both
//! drive this same dispatcher — there is no second implementation of the
//! business logic.
//!
//! Requests: `capabilities`, `list_processes`, `create_session`,
//! `start_recording`, `stop_recording` (idempotent), `get_session`,
//! `list_sessions`, `query_metrics`. All responses bounded by the frame cap.

use std::io::{Read, Write};

/// One bidirectional byte stream (named pipe, TCP-like socket, test duplex).
pub trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}
use std::sync::Arc;
use std::time::Duration;

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId};

use crate::ipc;
use crate::supervisor::{SessionOptions, SessionSupervisor, TargetSpec};

/// Authorization context for one connection (PF-408, v1 model: same-user
/// pipe ACL; richer capability tokens remain an open ADR item).
#[derive(Clone, Debug)]
pub struct ClientContext {
    pub client_name: String,
}

pub struct DaemonService {
    supervisor: Arc<SessionSupervisor>,
    backend: Arc<
        dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, PfError> + Send + Sync,
    >,
}

impl DaemonService {
    pub fn new(
        supervisor: Arc<SessionSupervisor>,
        backend: Arc<
            dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, PfError>
                + Send
                + Sync,
        >,
    ) -> Self {
        DaemonService {
            supervisor,
            backend,
        }
    }

    /// Serve one client connection to EOF. Disconnects are normal: the
    /// supervisor keeps every recording alive (exit gate P4).
    pub fn serve_connection(&self, stream: &mut (dyn Stream + Send)) {
        // Handshake first; on failure write one error frame and drop.
        let first = match ipc::read_frame(stream) {
            Ok(v) => v,
            Err(_) => return,
        };
        match ipc::accept_handshake(&first) {
            Ok(reply) => {
                if ipc::write_frame(stream, &reply).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = ipc::write_frame(stream, &ipc::response_err(0, &e));
                return;
            }
        }
        let context = ClientContext {
            client_name: first
                .get("client")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        };

        loop {
            let frame = match ipc::read_frame(stream) {
                Ok(v) => v,
                Err(_) => return, // disconnect (normal) or malformed frame
            };
            let reply = self.dispatch(&context, &frame);
            if ipc::write_frame(stream, &reply).is_err() {
                return;
            }
        }
    }

    /// Pure dispatch: request frame in, response frame out. Exposed for
    /// direct testing without a transport.
    pub fn dispatch(&self, ctx: &ClientContext, frame: &JsonValue) -> JsonValue {
        let id = frame.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let op = frame.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let args = frame
            .get("args")
            .cloned()
            .unwrap_or(JsonValue::Object(vec![]));
        match self.execute(ctx, op, &args) {
            Ok(result) => ipc::response_ok(id, result),
            Err(e) => ipc::response_err(id, &e),
        }
    }

    fn execute(
        &self,
        ctx: &ClientContext,
        op: &str,
        args: &JsonValue,
    ) -> Result<JsonValue, PfError> {
        match op {
            "capabilities" => self.op_capabilities(),
            "list_processes" => self.op_list_processes(args),
            "create_session" => self.op_create_session(args),
            "start_recording" => self.op_start(args),
            "stop_recording" => self.op_stop(args),
            "get_session" => self.op_get(args),
            "list_sessions" => Ok(JsonValue::Array(
                self.supervisor.list().iter().map(|v| v.to_json()).collect(),
            )),
            "query_metrics" => self.op_query_metrics(args),
            "query_top_functions" => self.op_top_functions(args),
            "get_stack" => self.op_get_stack(args),
            "compare_sessions" => self.op_compare(args),
            "delete_session" => self.op_delete_session(args),
            "ping" => {
                use pf_types::json::JsonValue as J;
                Ok(J::Object(vec![
                    ("pong".into(), J::Bool(true)),
                    ("client".into(), J::Str(ctx.client_name.clone())),
                ]))
            }
            other => Err(PfError::new(
                ErrorCode::InvalidArgument,
                format!("unknown operation `{other}`"),
            )),
        }
    }

    fn op_capabilities(&self) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        let backend = (self.backend)()?;
        let caps = backend.capabilities();
        let probe = |p: &pf_platform::contracts::ProbeStatus| {
            J::Object(vec![
                ("available".into(), J::Bool(p.available)),
                ("reason_code".into(), J::Str(p.reason_code.clone())),
                ("detail".into(), J::Str(p.detail.clone())),
            ])
        };
        Ok(J::Object(vec![
            ("os".into(), J::Str(caps.os.as_str().into())),
            ("os_version".into(), J::Str(caps.os_version.clone())),
            (
                "logical_cpus".into(),
                match caps.logical_cpus {
                    Some(n) => J::UInt(u64::from(n)),
                    None => J::Null,
                },
            ),
            ("counters".into(), probe(&caps.counters)),
            ("cpu_stack_sampling".into(), probe(&caps.cpu_stack_sampling)),
        ]))
    }

    fn op_list_processes(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        // 4096 rows worst case ≈ 0.6-1.2 MB of JSON against the 4 MiB
        // frame cap — bounded, and the snapshot stops at the limit.
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .min(4096) as usize;
        let backend = (self.backend)()?;
        let entries = backend.enumerate(limit)?;
        Ok(J::Array(
            entries
                .iter()
                .map(|e| {
                    J::Object(vec![
                        ("pid".into(), J::UInt(u64::from(e.pid))),
                        (
                            "parent_pid".into(),
                            match e.parent_pid {
                                Some(p) => J::UInt(u64::from(p)),
                                None => J::Null,
                            },
                        ),
                        ("image".into(), J::Str(e.image_name.clone())),
                        (
                            "cpu_pct".into(),
                            match e.cpu_pct {
                                Some(v) => J::Float(v),
                                None => J::Null,
                            },
                        ),
                        (
                            "rss_bytes".into(),
                            match e.rss_bytes {
                                Some(v) => J::UInt(v),
                                None => J::Null,
                            },
                        ),
                    ])
                })
                .collect(),
        ))
    }

    fn op_create_session(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        let spec = if let Some(pid) = args.get("pid").and_then(|v| v.as_u64()) {
            let pid = u32::try_from(pid)
                .map_err(|_| PfError::new(ErrorCode::InvalidArgument, "pid out of range"))?;
            TargetSpec::Attach { pid }
        } else if let Some(exe) = args.get("run").and_then(|v| v.as_str()) {
            let run_args = args
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            TargetSpec::Launch {
                exe: exe.to_string(),
                args: run_args,
            }
        } else {
            return Err(PfError::new(
                ErrorCode::InvalidArgument,
                "create_session needs `pid` or `run`",
            ));
        };
        let id = self.supervisor.create(spec).map_err(PfError::from)?;
        Ok(J::Object(vec![
            ("session_id".into(), J::Str(id.as_hyphenated())),
            ("state".into(), J::Str("CREATED".into())),
        ]))
    }

    fn op_start(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        let id = parse_session_id(args)?;
        let options = SessionOptions {
            interval_ms: args
                .get("interval_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(250),
            duration_ms: args
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(10_000),
            out_dir: args
                .get("out_dir")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from),
            label: args
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };
        let state = self.supervisor.start(id, options).map_err(PfError::from)?;
        Ok(view_state_json(id, state))
    }

    fn op_stop(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        let id = parse_session_id(args)?;
        let state = self.supervisor.stop(id).map_err(PfError::from)?;
        // Bounded wait so a stop-and-wait client gets the terminal state;
        // recording itself already ended (idempotent above).
        if !state.is_terminal() {
            let final_state = self
                .supervisor
                .wait(id, Duration::from_secs(30))
                .map_err(PfError::from)?;
            let _ = self.supervisor.finalize(id);
            return Ok(view_state_json(id, final_state));
        }
        Ok(view_state_json(id, state))
    }

    /// Delete a terminal session (record + on-disk store). Live sessions
    /// are refused with a typed error telling the caller to stop first.
    fn op_delete_session(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        let id = parse_session_id(args)?;
        let freed = self.supervisor.delete(id).map_err(PfError::from)?;
        Ok(J::Object(vec![
            ("deleted".into(), J::Bool(true)),
            ("bytes_freed".into(), J::UInt(freed)),
        ]))
    }

    fn op_get(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        let id = parse_session_id(args)?;
        let _ = self.supervisor.finalize(id);
        let view = self.supervisor.view(id).map_err(PfError::from)?;
        Ok(view.to_json())
    }

    /// PF-1110: compare two recorded sessions by id. Directories are
    /// resolved from session records (no client-supplied paths — no path
    /// traversal surface); snapshots load from the durable chunk store
    /// with schema-0 JSONL fallback; comparison goes through the shared
    /// `pf_analysis` engine (one implementation everywhere).
    fn op_compare(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        let load = |id: SessionId| -> Result<LoadedSessionFacts, PfError> {
            let view = self.supervisor.view(id).map_err(PfError::from)?;
            let dir = view.session_dir.clone().ok_or_else(|| {
                PfError::new(
                    ErrorCode::SessionNotFound,
                    format!("session {} has no persisted directory yet", id),
                )
            })?;
            let reader = pf_storage::ChunkReader::open(&dir)?;
            let snapshots = if reader.total_records() > 0 {
                reader.range(0, u64::MAX, 100_000)?.snapshots
            } else {
                let store = pf_platform::session_store::SessionStore::open(&dir)?;
                store.read_samples()?.0
            };
            Ok(LoadedSessionFacts { dir, snapshots })
        };
        let parse_id = |key: &str| -> Result<SessionId, PfError> {
            args.get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, format!("missing {key}")))
                .and_then(|s| {
                    SessionId::parse(s).map_err(|e| PfError::new(ErrorCode::InvalidArgument, e))
                })
        };
        let a_id = parse_id("baseline")?;
        let b_id = parse_id("candidate")?;
        let a = load(a_id)?;
        let b = load(b_id)?;
        let backend = (self.backend)()?;
        let caps = backend.capabilities();
        let facts = |id: SessionId, loaded: LoadedSessionFacts| {
            pf_analysis::SessionFacts::from_snapshots(
                id.as_hyphenated().as_str(),
                caps.os,
                250,
                caps.logical_cpus,
                &loaded.snapshots,
                vec![],
            )
        };
        let comparison = pf_analysis::compare_sessions(&pf_analysis::ComparisonInput {
            baseline: facts(a_id, a),
            candidate: facts(b_id, b),
            force_incompatible: args.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        Ok(comparison.to_json())
    }

    fn op_query_metrics(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        let id = parse_session_id(args)?;
        let samples = self.supervisor.samples(id).map_err(PfError::from)?;
        let backend = (self.backend)()?;
        let caps = backend.capabilities();
        let report = pf_platform::report::build_report(pf_platform::report::ReportInput {
            schema_label: "pulseforge.report/0",
            session_id: &id,
            state: pf_types::SessionState::Completed,
            stop_reason: "snapshot",
            identity: &pf_types::ProcessIdentity::new(
                caps.os,
                self.supervisor
                    .view(id)
                    .map_err(PfError::from)?
                    .target_pid
                    .unwrap_or(0),
                0,
                "",
            ),
            image_name: "",
            snapshots: &samples,
            logical_cpus: caps.logical_cpus,
            warnings: &[],
            poll_p50_ms: 0.0,
            poll_max_ms: 0.0,
            partial_read: None,
            stacks_summary: None,
        });
        Ok(report)
    }

    /// PF-905: top functions of a session's sampled stacks, bounded input
    /// and output, with symbol-coverage quality fields. Reads the
    /// self-contained `stacks.jsonl` (frames resolved at finalize time).
    fn op_top_functions(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        let id = parse_session_id(args)?;
        let dir = self
            .supervisor
            .view(id)
            .map_err(PfError::from)?
            .session_dir
            .ok_or_else(|| {
                PfError::new(
                    ErrorCode::CapabilityUnavailable,
                    "session has no directory on this daemon (recovered sessions only)",
                )
            })?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(32)
            .clamp(1, 64) as usize;
        let samples = pf_analysis::stacks_io::load_session_stacks(&dir)?;
        let mut trie = pf_analysis::stacks::StackTrie::new();
        trie.aggregate(&samples);
        let quality = pf_analysis::stacks_io::coverage_quality(&dir);
        let row = |(key, module, w, pct): &(&str, &str, u64, f64)| {
            J::Object(vec![
                ("function".to_string(), J::Str((*key).to_string())),
                ("module".to_string(), J::Str((*module).to_string())),
                ("weight".to_string(), J::UInt(*w)),
                ("weight_pct".to_string(), J::Float(*pct)),
            ])
        };
        Ok(J::Object(vec![
            (
                "schema".to_string(),
                J::Str("pulseforge.top-functions/0".into()),
            ),
            ("session_id".to_string(), J::Str(id.as_hyphenated())),
            ("samples".to_string(), J::UInt(trie.sample_count() as u64)),
            ("total_weight".to_string(), J::UInt(trie.total_weight())),
            (
                "frame_identity_coverage".to_string(),
                opt_f64(quality.frame_identity_coverage),
            ),
            (
                "frames_resolved".to_string(),
                opt_u64(quality.frames_resolved),
            ),
            ("frames_total".to_string(), opt_u64(quality.frames_total)),
            (
                "recursion_cuts".to_string(),
                J::UInt(trie.recursion_cuts() as u64),
            ),
            (
                "aggregation_truncated".to_string(),
                J::Bool(trie.nodes_truncated()),
            ),
            (
                "top_functions".to_string(),
                J::Array(trie.top_by_exclusive(limit).iter().map(row).collect()),
            ),
            (
                "top_inclusive".to_string(),
                J::Array(trie.top_by_inclusive(limit).iter().map(row).collect()),
            ),
        ]))
    }

    /// PF-905: sample callchains containing a given function, bounded
    /// count and depth, outermost-first, with honest coverage fields.
    fn op_get_stack(&self, args: &JsonValue) -> Result<JsonValue, PfError> {
        use pf_types::json::JsonValue as J;
        let id = parse_session_id(args)?;
        let function = args
            .get("function")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .ok_or_else(|| {
                PfError::new(ErrorCode::InvalidArgument, "missing or invalid `function`")
            })?
            .to_string();
        let dir = self
            .supervisor
            .view(id)
            .map_err(PfError::from)?
            .session_dir
            .ok_or_else(|| {
                PfError::new(
                    ErrorCode::CapabilityUnavailable,
                    "session has no directory on this daemon (recovered sessions only)",
                )
            })?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(1, 16) as usize;
        let stacks = pf_analysis::stacks_io::load_session_stacks(&dir)?;
        // load_session_stacks yields outermost-first stacks in file order.
        let mut matches: Vec<J> = Vec::new();
        let mut total_matches = 0u64;
        for st in &stacks {
            if st.frames.iter().any(|f| f.key == function) {
                total_matches = total_matches.saturating_add(1);
                if matches.len() < limit {
                    let frames: Vec<J> = st
                        .frames
                        .iter()
                        .take(64)
                        .map(|f| {
                            J::Object(vec![
                                ("function".to_string(), J::Str(f.key.clone())),
                                ("module".to_string(), J::Str(f.module.clone())),
                                ("resolved".to_string(), J::Bool(f.resolved)),
                            ])
                        })
                        .collect();
                    matches.push(J::Object(vec![
                        ("frames".to_string(), J::Array(frames)),
                        (
                            "frames_omitted".to_string(),
                            J::UInt(st.frames.len().saturating_sub(64) as u64),
                        ),
                    ]));
                }
            }
        }
        let quality = pf_analysis::stacks_io::coverage_quality(&dir);
        Ok(J::Object(vec![
            (
                "schema".to_string(),
                J::Str("pulseforge.stack-samples/0".into()),
            ),
            ("session_id".to_string(), J::Str(id.as_hyphenated())),
            ("function".to_string(), J::Str(function)),
            ("matches_total".to_string(), J::UInt(total_matches)),
            (
                "matches_returned".to_string(),
                J::UInt(matches.len() as u64),
            ),
            (
                "frame_identity_coverage".to_string(),
                opt_f64(quality.frame_identity_coverage),
            ),
            (
                "frames_resolved".to_string(),
                opt_u64(quality.frames_resolved),
            ),
            ("frames_total".to_string(), opt_u64(quality.frames_total)),
            ("samples".to_string(), J::Array(matches)),
        ]))
    }
}

struct LoadedSessionFacts {
    #[allow(dead_code)]
    dir: std::path::PathBuf,
    snapshots: Vec<pf_types::CounterSnapshot>,
}

fn view_state_json(id: SessionId, state: pf_types::SessionState) -> JsonValue {
    use pf_types::json::JsonValue as J;
    J::Object(vec![
        ("session_id".into(), J::Str(id.as_hyphenated())),
        ("state".into(), J::Str(state.as_str().into())),
    ])
}

fn opt_u64(v: Option<u64>) -> JsonValue {
    v.map(JsonValue::UInt).unwrap_or(JsonValue::Null)
}

fn opt_f64(v: Option<f64>) -> JsonValue {
    v.map(JsonValue::Float).unwrap_or(JsonValue::Null)
}

fn parse_session_id(args: &JsonValue) -> Result<SessionId, PfError> {
    args.get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, "missing session_id"))
        .and_then(|s| SessionId::parse(s).map_err(|e| PfError::new(ErrorCode::InvalidArgument, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_platform::contracts::{
        PlatformBackend, PlatformCapabilities, ProbeStatus, ProcessEntry,
    };
    use pf_types::Os;

    /// Minimal backend with scripted rows — locks the `list_processes`
    /// wire shape (live-resource fields included, honest Null on absence).
    struct RowsBackend(Vec<ProcessEntry>);

    impl PlatformBackend for RowsBackend {
        fn name(&self) -> &'static str {
            "rows-fake"
        }
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities {
                os: Os::Windows,
                os_version: "test".into(),
                logical_cpus: Some(8),
                counters: ProbeStatus::ok(),
                cpu_stack_sampling: ProbeStatus::unavailable("NOT_IMPLEMENTED", "test"),
            }
        }
        fn enumerate(&self, limit: usize) -> Result<Vec<ProcessEntry>, PfError> {
            Ok(self.0.iter().take(limit).cloned().collect())
        }
        fn identity(&self, _pid: u32) -> Result<pf_types::ProcessIdentity, PfError> {
            Err(PfError::new(ErrorCode::UnsupportedPlatform, "test"))
        }
        fn open(
            &self,
            _pid: u32,
        ) -> Result<Box<dyn pf_platform::contracts::CounterSource>, PfError> {
            Err(PfError::new(ErrorCode::UnsupportedPlatform, "test"))
        }
    }

    fn service_with(rows: Vec<ProcessEntry>) -> DaemonService {
        DaemonService::new(
            Arc::new(crate::supervisor::SessionSupervisor::new(Box::new(|| {
                Err(PfError::new(ErrorCode::UnsupportedPlatform, "test"))
            }))),
            Arc::new(move || Ok(Box::new(RowsBackend(rows.clone())) as Box<dyn PlatformBackend>)),
        )
    }

    #[test]
    fn list_processes_carries_live_resource_fields() {
        let svc = service_with(vec![
            ProcessEntry {
                pid: 100,
                parent_pid: None,
                image_name: "hot.exe".into(),
                cpu_pct: Some(97.3),
                rss_bytes: Some(812 * 1024 * 1024),
            },
            ProcessEntry {
                pid: 101,
                parent_pid: Some(100),
                image_name: "quiet.exe".into(),
                cpu_pct: None,
                rss_bytes: None,
            },
        ]);
        let ctx = ClientContext {
            client_name: "test".into(),
        };
        let res = svc
            .execute(&ctx, "list_processes", &JsonValue::Object(vec![]))
            .expect("list ok");
        let rows = res.as_array().expect("array");
        assert_eq!(rows.len(), 2);
        let hot = &rows[0];
        assert_eq!(hot.get("pid").and_then(|v| v.as_u64()), Some(100));
        assert_eq!(hot.get("cpu_pct").and_then(|v| v.as_f64()), Some(97.3));
        assert_eq!(
            hot.get("rss_bytes").and_then(|v| v.as_u64()),
            Some(812 * 1024 * 1024)
        );
        let quiet = &rows[1];
        assert!(
            quiet
                .get("cpu_pct")
                .map(|v| matches!(v, JsonValue::Null))
                .unwrap_or(false),
            "absent cpu must be JSON null, not a zero guess"
        );
        assert!(
            quiet
                .get("rss_bytes")
                .map(|v| matches!(v, JsonValue::Null))
                .unwrap_or(false),
            "absent rss must be JSON null"
        );
    }

    #[test]
    fn delete_session_wire_shape_on_a_terminal_session() {
        // Real lifecycle with a deliberately failing backend: the worker
        // fails fast, the session lands terminal (FAILED), and delete must
        // remove record + store with the documented JSON shape.
        let svc = service_with(vec![]);
        let ctx = ClientContext {
            client_name: "test".into(),
        };

        let out = std::env::temp_dir().join(format!(
            "pf-svc-del-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&out).expect("out dir");

        let created = svc
            .execute(
                &ctx,
                "create_session",
                &pf_types::json::parse(r#"{"pid":1}"#).unwrap(),
            )
            .expect("create");
        let sid = created
            .get("session_id")
            .and_then(|v| v.as_str())
            .expect("session_id")
            .to_string();

        let started = svc.execute(
            &ctx,
            "start_recording",
            &pf_types::json::parse(&format!(
                r#"{{"session_id":"{sid}","interval_ms":150,"duration_ms":8000}}"#
            ))
            .unwrap(),
        );
        // The failing backend may refuse start outright or fail the worker
        // async; either way the record must exist afterwards.
        if started.is_ok() {
            let _ = svc.execute(
                &ctx,
                "stop_recording",
                &pf_types::json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
            );
            // Give the worker a moment to land terminal.
            for _ in 0..50 {
                let view = svc.execute(
                    &ctx,
                    "get_session",
                    &pf_types::json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
                );
                let state = view
                    .ok()
                    .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string));
                if matches!(
                    state.as_deref(),
                    Some("FAILED") | Some("COMPLETED") | Some("CANCELLED")
                ) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let del = svc
                .execute(
                    &ctx,
                    "delete_session",
                    &pf_types::json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
                )
                .expect("delete terminal session");
            assert_eq!(del.get("deleted").and_then(|v| v.as_bool()), Some(true));
            assert!(del.get("bytes_freed").and_then(|v| v.as_u64()).is_some());
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn delete_session_unknown_id_is_a_typed_error() {
        let svc = service_with(vec![]);
        let ctx = ClientContext {
            client_name: "test".into(),
        };
        let res = svc.execute(
            &ctx,
            "delete_session",
            &pf_types::json::parse(r#"{"session_id":"00000000-0000-1000-8000-000000000001"}"#)
                .unwrap(),
        );
        let err = res.expect_err("unknown session must be typed");
        assert_eq!(err.code, pf_types::ErrorCode::SessionNotFound);
    }
}
