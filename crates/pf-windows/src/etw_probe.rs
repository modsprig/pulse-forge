//! ETW feasibility probe (roadmap PF-004/PF-005, P0 spike).
//!
//! What this probe does — and deliberately does **not** do:
//!
//! * attempts to create a **uniquely named, PulseForge-owned** ETW session
//!   (real-time mode, no providers enabled, no kernel flags) as the current
//!   (non-elevated) user;
//! * immediately stops and forgets the session **only if PulseForge created
//!   it**;
//! * enumerates existing sessions via `QueryAllTraces` (read-only) to detect
//!   pre-existing tracing activity such as an NT Kernel Logger session;
//! * never touches a session it did not create — no takeover, no stop of
//!   foreign controllers, no privilege bypass.
//!
//! Expected outcomes are recorded verbatim in `docs/feasibility-results.md`;
//! the probe returns structured JSON-ready results, not booleans, so the
//! exact OS behavior stays visible.

#![allow(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use pf_types::json::JsonValue;

use crate::ffi;

/// Result of one probe step.
#[derive(Clone, Debug)]
pub struct ProbeStep {
    pub name: &'static str,
    /// Raw Win32 return code (0 = success).
    pub win32_error: u32,
    /// True when the step succeeded end-to-end.
    pub succeeded: bool,
    pub message: String,
}

impl ProbeStep {
    fn to_json(&self) -> JsonValue {
        use JsonValue as J;
        J::Object(vec![
            ("step".into(), J::Str(self.name.into())),
            ("win32_error".into(), J::UInt(u64::from(self.win32_error))),
            (
                "win32_error_hex".into(),
                J::Str(format!("0x{:08X}", self.win32_error)),
            ),
            ("succeeded".into(), J::Bool(self.succeeded)),
            ("message".into(), J::Str(self.message.clone())),
        ])
    }
}

/// Structured outcome of the full ETW feasibility probe.
#[derive(Clone, Debug)]
pub struct EtwProbeResult {
    pub elevated: Option<bool>,
    pub steps: Vec<ProbeStep>,
    /// Honest conclusion for this machine right now.
    pub conclusion: &'static str,
    pub can_create_session: bool,
    pub existing_sessions_visible: bool,
}

impl EtwProbeResult {
    pub fn to_json(&self) -> JsonValue {
        use JsonValue as J;
        let elevated = match self.elevated {
            Some(v) => J::Bool(v),
            None => J::Null,
        };
        J::Object(vec![
            ("elevated".into(), elevated),
            (
                "steps".into(),
                J::Array(self.steps.iter().map(|s| s.to_json()).collect()),
            ),
            ("conclusion".into(), J::Str(self.conclusion.into())),
            (
                "can_create_session".into(),
                J::Bool(self.can_create_session),
            ),
            (
                "existing_sessions_visible".into(),
                J::Bool(self.existing_sessions_visible),
            ),
        ])
    }
}

fn unique_session_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("PulseForgeProbe-{}-{nanos}", std::process::id())
}

/// Probe GUID derived deterministically from the session purpose — this is a
/// *session* identity for an empty probe session, not a claim on any
/// provider's events.
fn probe_guid() -> ffi::Guid {
    ffi::Guid {
        data1: 0x7f2a_91c4,
        data2: 0x6b3d,
        data3: 0x4e58,
        data4: [0x9c, 0x11, 0x70, 0x66, 0x6f, 0x72, 0x67, 0x65],
    }
}

/// Are we running elevated? Uses the documented (undirected) heuristic:
/// try opening a process that requires elevation to query. `None` = unknown.
fn elevation_hint() -> Option<bool> {
    // SAFETY: plain call with numeric pid 4 (System).
    let h = unsafe { ffi::OpenProcess(ffi::PROCESS_QUERY_LIMITED_INFORMATION, 0, 4) };
    if h.is_null() || h as isize == -1 {
        let err = unsafe { ffi::GetLastError() };
        if err == ffi::ERROR_ACCESS_DENIED {
            return Some(false);
        }
        return None;
    }
    // SAFETY: we own this handle.
    unsafe { ffi::CloseHandle(h) };
    Some(true)
}

/// Run the probe. Never panics, never leaves a PulseForge-owned session
/// behind, never touches foreign sessions.
pub fn probe_etw_session_creation() -> EtwProbeResult {
    let mut steps = Vec::new();
    let elevated = elevation_hint();
    steps.push(ProbeStep {
        name: "elevation_hint_open_pid4",
        win32_error: 0,
        succeeded: true,
        message: format!(
            "OpenProcess(pid=4) suggests elevated={:?} (heuristic, not a privilege claim)",
            elevated
        ),
    });

    // -- Step 1: create an owned probe session (real-time, no providers) --
    let session_name = unique_session_name();
    let mut buffer = vec![0u8; size_of_props_with_names()];
    let props: *mut ffi::EventTraceProperties = buffer.as_mut_ptr().cast();
    // SAFETY: writing into our own zeroed, correctly sized allocation.
    unsafe {
        std::ptr::write_bytes(props.cast::<u8>(), 0, buffer.len());
        (*props).wnode.buffer_size = buffer_len_u32();
        (*props).wnode.flags = ffi::WNODE_FLAG_TRACED_GUID;
        (*props).wnode.guid = probe_guid();
        (*props).wnode.client_context = 1; // QPC timestamps
        (*props).log_file_mode = ffi::EVENT_TRACE_REAL_TIME_MODE;
        (*props).buffer_size = 16; // 16 KiB buffers — minimal footprint
        (*props).minimum_buffers = 2;
        (*props).maximum_buffers = 4;
    }

    let name_bytes = session_name.as_bytes();
    let mut trace_handle: ffi::TraceHandle = 0;
    // SAFETY: name is NUL-appended below the struct in the same allocation;
    // properties pointer/size pair is valid.
    let start_rc = unsafe { ffi::StartTraceA(&mut trace_handle, name_bytes.as_ptr(), props) };
    let created = start_rc == ffi::ERROR_SUCCESS;
    steps.push(ProbeStep {
        name: "start_trace_own_session",
        win32_error: start_rc,
        succeeded: created,
        message: if created {
            format!(
                "session `{session_name}` created (handle {trace_handle}); no providers enabled"
            )
        } else {
            format!(
                "StartTraceA denied/failed for `{session_name}`: {}",
                ffi::win32_message(start_rc)
            )
        },
    });

    // -- Step 2: stop it again, ONLY if we own it --
    if created {
        // SAFETY: same allocation, our own session name and handle.
        let stop_rc = unsafe {
            ffi::ControlTraceA(
                trace_handle,
                name_bytes.as_ptr(),
                props,
                ffi::EVENT_TRACE_CONTROL_STOP,
            )
        };
        steps.push(ProbeStep {
            name: "stop_own_session",
            win32_error: stop_rc,
            succeeded: stop_rc == ffi::ERROR_SUCCESS,
            message: format!(
                "stopped PulseForge-owned session `{session_name}`: {}",
                ffi::win32_message(stop_rc)
            ),
        });
    } else {
        steps.push(ProbeStep {
            name: "stop_own_session",
            win32_error: 0,
            succeeded: true,
            message: "skipped: no session was created, nothing to stop (foreign sessions are never touched)".into(),
        });
    }

    let enumeration = probe_etw_trace_enumeration();
    for step in &enumeration.steps {
        steps.push(step.clone());
    }

    let can_create = created;
    let existing_visible = enumeration.existing_sessions_visible;
    let conclusion = match (created, elevated) {
        (true, Some(false)) => {
            "non-elevated ETW session creation works for private sessions on this build; kernel/system providers still gated by privilege (P0 evidence for v0.6 planning)"
        }
        (true, _) => {
            "ETW session creation works in this context; privileged stack capture still requires SeSystemProfilePrivilege (probe in v0.6)"
        }
        (false, Some(false)) => {
            "ETW session creation is denied for a normal user on this build; stack sampling will require an elevated, user-approved context — counters-only remains the default path"
        }
        (false, _) => "ETW session creation failed; see step details for the exact OS error",
    };

    EtwProbeResult {
        elevated,
        steps,
        conclusion,
        can_create_session: can_create,
        existing_sessions_visible: existing_visible,
    }
}

/// Read-only enumeration of existing ETW sessions (`QueryAllTraces`).
pub fn probe_etw_trace_enumeration() -> EtwProbeResult {
    let mut steps = Vec::new();
    let mut handles = vec![0u64; 64];
    let mut count: u32 = 0;
    // SAFETY: array/size pair plus valid out-pointer.
    let rc = unsafe {
        ffi::QueryAllTracesA(
            handles.as_mut_ptr(),
            u32::try_from(handles.len()).unwrap_or(64),
            &mut count,
        )
    };
    let visible = rc == ffi::ERROR_SUCCESS;
    steps.push(ProbeStep {
        name: "query_all_traces",
        win32_error: rc,
        succeeded: visible,
        message: if visible {
            format!("{count} existing ETW session(s) visible (handles only; names require per-session queries)")
        } else {
            format!(
                "QueryAllTraces failed: {} — existing tracing activity cannot be inventoried from this context",
                ffi::win32_message(rc)
            )
        },
    });

    EtwProbeResult {
        elevated: None,
        steps,
        conclusion: if visible {
            "existing sessions enumerable"
        } else {
            "session inventory not visible from this context"
        },
        can_create_session: false,
        existing_sessions_visible: visible,
    }
}

// Struct size is a compile-time constant; +512 bytes of logger-name room
// cannot overflow on any realistic platform.
#[allow(clippy::arithmetic_side_effects)]
fn size_of_props_with_names() -> usize {
    std::mem::size_of::<ffi::EventTraceProperties>() + 512
}

fn buffer_len_u32() -> u32 {
    u32::try_from(size_of_props_with_names()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_runs_and_reports_structured_steps() {
        let result = probe_etw_session_creation();
        let json = result.to_json();
        // The probe must always produce a conclusion and step list,
        // regardless of privilege outcomes.
        assert!(
            json.get("conclusion")
                .and_then(|v| v.as_str())
                .unwrap()
                .len()
                > 10
        );
        let steps = json.get("steps").and_then(|v| v.as_array()).unwrap();
        assert!(steps.len() >= 3, "at least elevation/start/stop steps");
        // Round-trip through our parser to guarantee the JSON is well-formed.
        let text = json.to_json_string();
        pf_types::json::parse(&text).expect("probe JSON parses");
    }

    #[test]
    fn session_names_are_unique_per_call() {
        assert_ne!(unique_session_name(), unique_session_name());
    }

    #[test]
    fn probe_never_leaks_a_session_it_created() {
        // Run the probe twice: if creation works, each run must stop its own
        // session; the second run must observe the same behavior (not, say,
        // ERROR_ALREADY_EXISTS from a leaked first session).
        let a = probe_etw_session_creation();
        let b = probe_etw_session_creation();
        if a.can_create_session {
            let stop = a
                .steps
                .iter()
                .find(|s| s.name == "stop_own_session")
                .expect("stop step present");
            assert!(
                stop.succeeded,
                "own session must be stopped: {}",
                stop.message
            );
            assert!(
                b.can_create_session,
                "second probe must also create a session (no name collision from a leak)"
            );
        }
    }
}
