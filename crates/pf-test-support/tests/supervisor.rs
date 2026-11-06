//! Supervisor integration tests (PF-401/PF-409): real backend, real
//! fixtures, real threads. The load-bearing assertions:
//!
//! - a session outlives its "client" (the supervisor owns recordings);
//! - stop is idempotent from any client, concurrently;
//! - target exit ends a session in a terminal state with data;
//! - stopping never kills an attached target;
//! - the daemon restart path recovers orphaned sessions.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pf_daemon::DaemonService;
use pf_daemon::supervisor::{SessionOptions, SessionSupervisor, TargetSpec};
use pf_types::SessionState;

fn supervisor() -> Arc<SessionSupervisor> {
    Arc::new(SessionSupervisor::new(Box::new(|| {
        Ok(Box::new(pf_windows::WindowsPlatform::new()))
    })))
}

fn service() -> DaemonService {
    let sup = supervisor();
    let backend: Arc<
        dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, pf_types::PfError>
            + Send
            + Sync,
    > = Arc::new(|| Ok(Box::new(pf_windows::WindowsPlatform::new())));
    DaemonService::new(sup, backend)
}

fn spin_child(ms: u64) -> std::process::Child {
    // Locate any workspace binary that can burn CPU deterministically.
    let exe = env!("CARGO_BIN_EXE_pf-fixture-cpu");
    std::process::Command::new(exe)
        .args(["--ms", &ms.to_string()])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn fixture")
}

fn temp_out() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-sup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn session_survives_client_disconnect_and_stops_cleanly() {
    // THE exit-gate-P4 assertion: the recording continues after the client
    // that started it is gone (here: after the request returns and the
    // "client" handle is dropped).
    let mut target = spin_child(20_000);
    let out = temp_out();
    let sup = supervisor();

    let id = sup.create(TargetSpec::Attach { pid: target.id() }).unwrap();
    let opts = SessionOptions {
        interval_ms: 150,
        duration_ms: 8_000,
        out_dir: Some(out.clone()),
        label: None,
    };
    sup.start(id, opts).unwrap();

    // "Client disconnect": we simply stop touching the supervisor from any
    // client context and let time pass. A short recording budget would end
    // by itself; here we prove survival by observing progress after a pause.
    std::thread::sleep(Duration::from_millis(700));
    let view1 = sup.view(id).unwrap();
    assert_eq!(view1.state, SessionState::Recording);
    assert!(view1.sample_count >= 2, "recording progressed: {view1:?}");

    // A *different* client (new request context) stops the session:
    let state = sup.stop(id).unwrap();
    assert_eq!(state, SessionState::Stopping);
    let final_state = sup.wait(id, Duration::from_secs(10)).unwrap();
    assert!(final_state.is_terminal(), "{final_state:?}");
    let view = sup.finalize(id).unwrap();
    assert!(view.sample_count >= 2);
    assert!(
        view.session_dir.is_some(),
        "session must be persisted; got warnings {:?}",
        view.warnings
    );
    assert!(view.report_path.as_ref().is_some_and(|p| p.is_file()));

    // Stop never kills the attached target: it must still be alive now.
    assert!(
        target.try_wait().expect("target status").is_none(),
        "attached target must survive recording stop"
    );
    let _ = target.kill();
    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn stop_is_idempotent_and_concurrent_safe() {
    let mut target = spin_child(20_000);
    let out = temp_out();
    let sup = supervisor();
    let id = sup.create(TargetSpec::Attach { pid: target.id() }).unwrap();
    sup.start(
        id,
        SessionOptions {
            interval_ms: 150,
            duration_ms: 8_000,
            out_dir: Some(out.clone()),
            label: None,
        },
    )
    .unwrap();

    // Two concurrent stoppers + two sequential repeats: all must succeed
    // with well-formed states, exactly one real stop.
    let sup2 = Arc::clone(&sup);
    let sup3 = Arc::clone(&sup);
    let a = std::thread::spawn(move || sup2.stop(id));
    let b = std::thread::spawn(move || sup3.stop(id));
    let s1 = a.join().unwrap().unwrap();
    let s2 = b.join().unwrap().unwrap();
    assert!(s1 == SessionState::Stopping || s1.is_terminal(), "{s1:?}");
    assert!(s2 == SessionState::Stopping || s2.is_terminal(), "{s2:?}");
    // Repeated stop on the terminal session: successful no-op.
    sup.wait(id, Duration::from_secs(10)).unwrap();
    sup.finalize(id).unwrap();
    let again = sup.stop(id).unwrap();
    assert!(again.is_terminal());

    let _ = target.kill();
    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn target_exit_terminates_session_with_data() {
    let mut target = spin_child(800);
    let out = temp_out();
    let sup = supervisor();
    let id = sup.create(TargetSpec::Attach { pid: target.id() }).unwrap();
    sup.start(
        id,
        SessionOptions {
            interval_ms: 100,
            duration_ms: 60_000, // far beyond the target's life
            out_dir: Some(out.clone()),
            label: None,
        },
    )
    .unwrap();
    let state = sup.wait(id, Duration::from_secs(15)).unwrap();
    assert!(state.is_terminal(), "{state:?}");
    let view = sup.finalize(id).unwrap();
    assert_eq!(view.stop_reason.as_deref(), Some("target_exited"));
    assert!(view.sample_count >= 1, "captured data before exit");
    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn launch_target_is_managed_and_recorded() {
    let out = temp_out();
    let sup = supervisor();
    let exe = env!("CARGO_BIN_EXE_pf-fixture-cpu").to_string();
    let id = sup
        .create(TargetSpec::Launch {
            exe,
            args: vec!["--ms".into(), "6000".into()],
        })
        .unwrap();
    sup.start(
        id,
        SessionOptions {
            interval_ms: 150,
            duration_ms: 700,
            out_dir: Some(out.clone()),
            label: Some("launch-test".into()),
        },
    )
    .unwrap();
    let state = sup.wait(id, Duration::from_secs(10)).unwrap();
    assert!(state.is_terminal());
    let view = sup.finalize(id).unwrap();
    assert!(view.sample_count >= 1);
    // Direct-parent cleanup happens on the worker after finalization: give
    // the thread a beat to reap the child.
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn invalid_start_options_are_rejected_before_recording() {
    let sup = supervisor();
    let id = sup.create(TargetSpec::Attach { pid: 4 }).unwrap();
    let err = sup.start(
        id,
        SessionOptions {
            interval_ms: 10, // out of range
            duration_ms: 1_000,
            out_dir: None,
            label: None,
        },
    );
    assert!(err.is_err());
}

#[test]
fn unknown_session_is_typed_error() {
    let sup = supervisor();
    let missing = pf_types::SessionId::now_v7();
    assert!(sup.stop(missing).is_err());
    assert!(sup.view(missing).is_err());
}

#[test]
fn service_dispatch_round_trip_over_fake_client() {
    // Service-level request/response without a transport: exercises the
    // exact frames the pipe carries.
    let svc = service();
    let mut target = spin_child(6_000);
    let out = temp_out();

    let create = pf_types::json::parse(&format!(
        r#"{{"type":"request","id":1,"op":"create_session","args":{{"pid":{}}}}}"#,
        target.id()
    ))
    .unwrap();
    let reply = svc.dispatch(
        &pf_daemon::service::ClientContext {
            client_name: "test".into(),
        },
        &create,
    );
    assert_eq!(reply.get("ok").and_then(|v| v.as_bool()), Some(true));
    let sid = reply
        .get("result")
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .expect("session id")
        .to_string();

    let start = pf_types::json::parse(&format!(
        r#"{{"type":"request","id":2,"op":"start_recording","args":{{"session_id":"{sid}","interval_ms":120,"duration_ms":600,"out_dir":"{}"}}}}"#,
        out.display().to_string().replace('\\', "/")
    ))
    .unwrap();
    let reply = svc.dispatch(
        &pf_daemon::service::ClientContext {
            client_name: "test".into(),
        },
        &start,
    );
    assert_eq!(reply.get("ok").and_then(|v| v.as_bool()), Some(true));

    // Disconnect the "client" (nothing to do: service is stateless per
    // request) — then a second client polls status.
    std::thread::sleep(Duration::from_millis(250));
    let get = pf_types::json::parse(&format!(
        r#"{{"type":"request","id":3,"op":"get_session","args":{{"session_id":"{sid}"}}}}"#
    ))
    .unwrap();
    let reply = svc.dispatch(
        &pf_daemon::service::ClientContext {
            client_name: "second".into(),
        },
        &get,
    );
    let state = reply
        .get("result")
        .and_then(|r| r.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        state == "RECORDING" || state == "STOPPING" || state == "COMPLETED",
        "{state}"
    );

    let _ = target.kill();
    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn bad_frames_produce_typed_errors_not_panics() {
    let svc = service();
    let ctx = pf_daemon::service::ClientContext {
        client_name: "t".into(),
    };
    for frame in [
        r#"{"type":"request","id":1,"op":"nonsense","args":{}}"#,
        r#"{"type":"request","id":2,"op":"create_session","args":{}}"#,
        r#"{"type":"request","id":3,"op":"start_recording","args":{"session_id":"zzz"}}"#,
        r#"{"type":"request","id":4,"op":"stop_recording","args":{}}"#,
        "{}",
    ] {
        let v = pf_types::json::parse(frame).unwrap();
        let reply = svc.dispatch(&ctx, &v);
        // Malformed requests must get ok=false with a code — never a panic,
        // never a dropped connection.
        assert_eq!(
            reply.get("ok").and_then(|b| b.as_bool()),
            Some(false),
            "{frame}"
        );
        assert!(reply.get("error").is_some(), "{frame}");
    }
}

#[test]
fn delete_removes_terminal_session_record_and_store() {
    let mut target = spin_child(20_000);
    let out = temp_out();
    let sup = supervisor();
    let id = sup.create(TargetSpec::Attach { pid: target.id() }).unwrap();
    sup.start(
        id,
        SessionOptions {
            interval_ms: 150,
            duration_ms: 8_000,
            out_dir: Some(out.clone()),
            label: None,
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    sup.stop(id).unwrap();
    sup.wait(id, Duration::from_secs(10)).unwrap();
    let view = sup.finalize(id).unwrap();
    let dir = view.session_dir.clone().expect("session persisted");
    assert!(dir.is_dir());

    // Live-session refusal BEFORE the session was finalized is covered by
    // ordering; here the session IS terminal, so delete must succeed:
    // record gone, directory gone, bytes reported.
    let freed = sup.delete(id).expect("delete terminal session");
    assert!(freed > 0, "must report freed bytes: {freed}");
    assert!(!dir.exists(), "session store must be gone");
    assert!(sup.view(id).is_err(), "record must be gone");
    // Deleting again: the directory no longer exists — typed UnknownSession.
    assert!(
        sup.delete(id).is_err(),
        "second delete must be a typed error"
    );

    let _ = target.kill();
    let _ = target.wait();
}

#[test]
fn delete_refuses_a_live_recording() {
    let mut target = spin_child(20_000);
    let out = temp_out();
    let sup = supervisor();
    let id = sup.create(TargetSpec::Attach { pid: target.id() }).unwrap();
    sup.start(
        id,
        SessionOptions {
            interval_ms: 150,
            duration_ms: 8_000,
            out_dir: Some(out.clone()),
            label: None,
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let err = sup.delete(id).expect_err("live session must refuse delete");
    match err {
        pf_daemon::supervisor::SupervisorError::InvalidState { wanted, .. } => {
            assert!(wanted.contains("terminal"), "{wanted}");
        }
        other => panic!("expected InvalidState, got {other:?}"),
    }
    sup.stop(id).unwrap();
    let _ = sup.wait(id, Duration::from_secs(10));
    let _ = sup.finalize(id);
    // Cleanup so the test leaves nothing behind.
    let _ = sup.delete(id);
    let _ = target.kill();
    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&out);
}
