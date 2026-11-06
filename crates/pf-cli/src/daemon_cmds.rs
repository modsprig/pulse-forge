//! Daemon and agent-client subcommands (roadmap PF-402/PF-406).
//!
//! `pulseforge daemon` — the per-user recording daemon: single-instance
//! lock, orphan recovery, named-pipe accept loop, one thread per client.
//! Client disconnects never touch recordings.
//!
//! `pulseforge agent <op>` — one-shot pipe client used by tests, scripts
//! and (later) the MCP bridge: versioned handshake, request, one JSON
//! response on stdout.

use pf_types::json::{self, JsonValue};
use pf_types::{ErrorCode, PfError};

use crate::args::AgentCommand;

pub fn dispatch_daemon() -> Result<(), PfError> {
    #[cfg(windows)]
    {
        run_daemon()
    }
    #[cfg(target_os = "linux")]
    {
        run_daemon_linux()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "no daemon transport for this OS (Windows pipe / Linux UDS)",
        ))
    }
}

pub fn dispatch_agent(cmd: AgentCommand) -> Result<(), PfError> {
    #[cfg(windows)]
    {
        run_agent(cmd)
    }
    #[cfg(target_os = "linux")]
    {
        run_agent_linux(cmd)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = cmd;
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "no agent client transport for this OS (Windows pipe / Linux UDS)",
        ))
    }
}

#[cfg(windows)]
fn run_daemon() -> Result<(), PfError> {
    use pf_daemon::SessionSupervisor;
    use pf_daemon::service::DaemonService;
    use pf_daemon::single_instance;
    use std::sync::Arc;

    let data_root = pf_platform::paths::data_root()
        .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
    let sessions_root = data_root.join("sessions");
    std::fs::create_dir_all(&sessions_root).map_err(PfError::from)?;

    // PF-407: recover orphaned sessions before serving.
    let recovered = pf_daemon::recovery::mark_abandoned_sessions(&sessions_root)?;
    if recovered > 0 {
        pf_platform::logging::warn(
            "daemon",
            &format!("recovered {recovered} orphaned session(s) as RECOVERED_PARTIAL"),
        );
    }

    let supervisor = Arc::new(SessionSupervisor::new(Box::new(|| {
        Ok(Box::new(pf_windows::WindowsPlatform::new()))
    })));
    // PF-402: single instance, stale-lock safe. Held for the process
    // lifetime.
    let backend = pf_windows::WindowsPlatform::new();
    let _lock_guard = single_instance::acquire(&data_root, std::process::id(), &|pid| {
        use pf_platform::contracts::PlatformBackend;
        backend.identity(pid)
    })
    .map_err(|e| PfError::new(ErrorCode::PermissionDenied, e.to_string()))?;

    let service_backend: Arc<
        dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, PfError> + Send + Sync,
    > = Arc::new(|| Ok(Box::new(pf_windows::WindowsPlatform::new())));
    let service = Arc::new(DaemonService::new(Arc::clone(&supervisor), service_backend));

    let pipe_name = pf_windows::pipe::daemon_pipe_name(&data_root);
    let mut listener = pf_windows::pipe::PipeListener::bind(&pipe_name, 8)?;
    pf_platform::logging::log(
        pf_platform::logging::Level::Info,
        "daemon",
        "serving; single instance held",
        &[("pipe", pipe_name), ("pid", std::process::id().to_string())],
    );

    loop {
        let stream = listener.accept()?;
        let service = Arc::clone(&service);
        std::thread::Builder::new()
            .name("pf-ipc-client".into())
            .spawn(move || {
                let mut stream = stream;
                service.serve_connection(&mut stream);
            })
            .map_err(|e| {
                PfError::new(
                    ErrorCode::BackendFailure,
                    format!("client thread spawn failed: {e}"),
                )
            })?;
    }
}

#[cfg(windows)]
fn run_agent(cmd: AgentCommand) -> Result<(), PfError> {
    use pf_daemon::ipc;
    use pf_types::json::JsonValue as J;

    let data_root = pf_platform::paths::data_root()
        .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
    let mut stream = pf_windows::pipe::connect(&pf_windows::pipe::daemon_pipe_name(&data_root))?;
    ipc::write_frame(&mut stream, &ipc::client_handshake("pulseforge-cli"))?;
    let ack = ipc::read_frame(&mut stream)?;
    if ack.get("type").and_then(|v| v.as_str()) != Some("handshake_ok") {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "daemon rejected the handshake",
        ));
    }

    match cmd {
        // `start` is a compound op over one connection: create + start,
        // then print the session id. The recording keeps running under the
        // daemon after this client exits — that is the daemon's purpose.
        AgentCommand::Start {
            pid,
            run,
            run_args,
            duration_ms,
            interval_ms,
            out_dir,
        } => {
            let create_args = match (pid, run) {
                (Some(p), _) => J::Object(vec![("pid".into(), J::UInt(u64::from(p)))]),
                (None, Some(exe)) => J::Object(vec![
                    ("run".into(), J::Str(exe)),
                    (
                        "args".into(),
                        J::Array(run_args.iter().map(|a| J::Str(a.clone())).collect()),
                    ),
                ]),
                (None, None) => {
                    return Err(PfError::new(
                        ErrorCode::InvalidArgument,
                        "agent start needs --pid or --run",
                    ));
                }
            };
            ipc::write_frame(&mut stream, &ipc::request(1, "create_session", create_args))?;
            let created = ipc::read_frame(&mut stream)?;
            expect_ok(&created)?;
            let sid = created
                .get("result")
                .and_then(|r| r.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    PfError::new(ErrorCode::BackendFailure, "no session id from daemon")
                })?
                .to_string();
            let mut start_args = vec![
                ("session_id".into(), J::Str(sid.clone())),
                ("duration_ms".into(), J::UInt(duration_ms)),
                ("interval_ms".into(), J::UInt(interval_ms)),
            ];
            if let Some(dir) = out_dir {
                start_args.push(("out_dir".into(), J::Str(dir)));
            }
            ipc::write_frame(
                &mut stream,
                &ipc::request(2, "start_recording", J::Object(start_args)),
            )?;
            let started = ipc::read_frame(&mut stream)?;
            expect_ok(&started)?;
            print_json(&J::Object(vec![
                ("session_id".into(), J::Str(sid)),
                (
                    "state".into(),
                    started
                        .get("result")
                        .and_then(|r| r.get("state"))
                        .cloned()
                        .unwrap_or(J::Null),
                ),
                (
                    "note".into(),
                    J::Str(
                        "recording continues under the daemon after this client exits; \
                         stop it with `pulseforge agent stop`"
                            .into(),
                    ),
                ),
            ]));
            Ok(())
        }
        simple => {
            let (op, args) = simple_request(&simple);
            ipc::write_frame(&mut stream, &ipc::request(1, op, args))?;
            let reply = ipc::read_frame(&mut stream)?;
            print_json(&reply);
            expect_ok(&reply)
        }
    }
}

#[cfg(windows)]
fn simple_request(cmd: &AgentCommand) -> (&'static str, JsonValue) {
    use pf_types::json::JsonValue as J;
    match cmd {
        AgentCommand::Capabilities => ("capabilities", J::Object(vec![])),
        AgentCommand::List { limit } => (
            "list_processes",
            J::Object(vec![("limit".into(), J::UInt(u64::from(*limit)))]),
        ),
        AgentCommand::Sessions => ("list_sessions", J::Object(vec![])),
        AgentCommand::Status { session } => (
            "get_session",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        AgentCommand::Stop { session } => (
            "stop_recording",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        AgentCommand::Metrics { session } => (
            "query_metrics",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        // `start` is handled by the compound path in run_agent.
        AgentCommand::Start { .. } => unreachable!("start handled in run_agent"),
    }
}

fn expect_ok(reply: &JsonValue) -> Result<(), PfError> {
    if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    let err = reply.get("error");
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .and_then(parse_code)
        .unwrap_or(ErrorCode::BackendFailure);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("request failed")
        .to_string();
    Err(PfError::new(code, message))
}

fn parse_code(s: &str) -> Option<ErrorCode> {
    use ErrorCode::*;
    Some(match s {
        "INVALID_ARGUMENT" => InvalidArgument,
        "PERMISSION_DENIED" => PermissionDenied,
        "CAPABILITY_UNAVAILABLE" => CapabilityUnavailable,
        "TARGET_EXITED" => TargetExited,
        "IDENTITY_MISMATCH" => IdentityMismatch,
        "SESSION_NOT_FOUND" => SessionNotFound,
        "PARTIAL_DATA" => PartialData,
        "RESOURCE_EXHAUSTED" => ResourceExhausted,
        "TIMEOUT" => Timeout,
        "BACKEND_FAILURE" => BackendFailure,
        "STORAGE_IO" => StorageIo,
        "UNSUPPORTED_PLATFORM" => UnsupportedPlatform,
        "INTERNAL" => Internal,
        _ => return None,
    })
}

fn print_json(v: &JsonValue) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", v.to_json_string());
}

// Keep the json import exercised for future compound clients.
#[allow(dead_code)]
fn _parse_guard(text: &str) -> Result<JsonValue, PfError> {
    json::parse(text).map_err(PfError::from)
}

/// Linux daemon: UDS transport (PF-404), same service/supervisor as
/// Windows — the single-instance lock and orphan recovery are shared.
#[cfg(target_os = "linux")]
fn run_daemon_linux() -> Result<(), PfError> {
    use pf_daemon::SessionSupervisor;
    use pf_daemon::service::DaemonService;
    use pf_daemon::single_instance;
    use std::sync::Arc;

    let data_root = pf_platform::paths::data_root()
        .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
    let sessions_root = data_root.join("sessions");
    std::fs::create_dir_all(&sessions_root).map_err(PfError::from)?;

    let supervisor = Arc::new(SessionSupervisor::new(Box::new(|| {
        Ok(Box::new(pf_linux::LinuxPlatform::new()))
    })));
    let recovered = pf_daemon::recovery::mark_abandoned_sessions(&sessions_root)?;
    if recovered > 0 {
        pf_platform::logging::warn(
            "daemon",
            &format!("recovered {recovered} orphaned session(s) as RECOVERED_PARTIAL"),
        );
    }

    let backend = pf_linux::LinuxPlatform::new();
    let _lock_guard = single_instance::acquire(&data_root, std::process::id(), &|pid| {
        use pf_platform::contracts::PlatformBackend;
        backend.identity(pid)
    })
    .map_err(|e| PfError::new(ErrorCode::PermissionDenied, e.to_string()))?;

    let service_backend: Arc<
        dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, PfError> + Send + Sync,
    > = Arc::new(|| Ok(Box::new(pf_linux::LinuxPlatform::new())));
    let service = Arc::new(DaemonService::new(Arc::clone(&supervisor), service_backend));

    let socket_path = pf_linux::uds::daemon_uds_path(&data_root);
    let mut listener = pf_linux::uds::UdsListener::bind(&socket_path, 8)?;
    pf_platform::logging::log(
        pf_platform::logging::Level::Info,
        "daemon",
        "serving; single instance held",
        &[
            ("socket", socket_path.display().to_string()),
            ("pid", std::process::id().to_string()),
        ],
    );

    loop {
        let stream = listener.accept()?;
        let service = Arc::clone(&service);
        std::thread::Builder::new()
            .name("pf-ipc-client".into())
            .spawn(move || {
                let mut stream = stream;
                service.serve_connection(&mut stream);
            })
            .map_err(|e| {
                PfError::new(
                    ErrorCode::BackendFailure,
                    format!("client thread spawn failed: {e}"),
                )
            })?;
    }
}

/// Linux agent: same framed-JSON protocol over UDS.
#[cfg(target_os = "linux")]
fn run_agent_linux(cmd: AgentCommand) -> Result<(), PfError> {
    use pf_daemon::ipc;
    use pf_types::json::JsonValue as J;

    let data_root = pf_platform::paths::data_root()
        .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
    let socket = pf_linux::uds::daemon_uds_path(&data_root);
    let mut stream = pf_linux::uds::connect(&socket)?;
    ipc::write_frame(&mut stream, &ipc::client_handshake("pulseforge-cli"))?;
    let ack = ipc::read_frame(&mut stream)?;
    if ack.get("type").and_then(|v| v.as_str()) != Some("handshake_ok") {
        return Err(PfError::new(
            ErrorCode::BackendFailure,
            "daemon rejected the handshake",
        ));
    }

    // Compound create+start flow for Start; simple ops otherwise.
    if let AgentCommand::Start {
        pid,
        run,
        run_args,
        duration_ms,
        interval_ms,
        out_dir,
    } = cmd
    {
        let create_args = match (pid, run) {
            (Some(p), _) => J::Object(vec![("pid".into(), J::UInt(u64::from(p)))]),
            (None, Some(exe)) => J::Object(vec![
                ("run".into(), J::Str(exe)),
                (
                    "args".into(),
                    J::Array(run_args.iter().map(|a| J::Str(a.clone())).collect()),
                ),
            ]),
            (None, None) => {
                return Err(PfError::new(
                    ErrorCode::InvalidArgument,
                    "agent start needs --pid or --run",
                ));
            }
        };
        ipc::write_frame(&mut stream, &ipc::request(1, "create_session", create_args))?;
        let created = ipc::read_frame(&mut stream)?;
        expect_ok(&created)?;
        let sid = created
            .get("result")
            .and_then(|r| r.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| PfError::new(ErrorCode::BackendFailure, "no session id from daemon"))?
            .to_string();
        let mut start_args = vec![
            ("session_id".to_string(), J::Str(sid.clone())),
            ("duration_ms".to_string(), J::UInt(duration_ms)),
            ("interval_ms".to_string(), J::UInt(interval_ms)),
        ];
        if let Some(dir) = out_dir {
            start_args.push(("out_dir".to_string(), J::Str(dir)));
        }
        ipc::write_frame(
            &mut stream,
            &ipc::request(2, "start_recording", J::Object(start_args)),
        )?;
        let started = ipc::read_frame(&mut stream)?;
        expect_ok(&started)?;
        print_json(&J::Object(vec![
            ("session_id".into(), J::Str(sid)),
            (
                "state".into(),
                started
                    .get("result")
                    .and_then(|r| r.get("state"))
                    .cloned()
                    .unwrap_or(J::Null),
            ),
            (
                "note".into(),
                J::Str("recording continues under the daemon".into()),
            ),
        ]));
        return Ok(());
    }

    let (op, args) = match &cmd {
        AgentCommand::Capabilities => ("capabilities", J::Object(vec![])),
        AgentCommand::List { limit } => (
            "list_processes",
            J::Object(vec![("limit".into(), J::UInt(u64::from(*limit)))]),
        ),
        AgentCommand::Sessions => ("list_sessions", J::Object(vec![])),
        AgentCommand::Status { session } => (
            "get_session",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        AgentCommand::Stop { session } => (
            "stop_recording",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        AgentCommand::Metrics { session } => (
            "query_metrics",
            J::Object(vec![("session_id".into(), J::Str(session.clone()))]),
        ),
        AgentCommand::Start { .. } => unreachable!("start handled above"),
    };
    ipc::write_frame(&mut stream, &ipc::request(1, op, args))?;
    let reply = ipc::read_frame(&mut stream)?;
    print_json(&reply);
    expect_ok(&reply)
}
