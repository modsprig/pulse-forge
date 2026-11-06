//! # pf-daemon
//!
//! Per-user recording daemon (roadmap P4, ADR-001): recordings are owned by
//! the daemon, not by any client. Disconnecting a client (CLI, future
//! MCP bridge, GUI) never terminates an active session.
//!
//! Layers:
//! - [`supervisor`]: session lifecycle FSM, worker threads, idempotent stop
//!   (PF-401);
//! - [`single_instance`]: one daemon per user/login context with stale-lock
//!   recovery (PF-402);
//! - [`recovery`]: orphaned active-session recovery on daemon start
//!   (PF-407);
//! - [`ipc`]: versioned, length-delimited JSON protocol shared by all
//!   clients (PF-405);
//! - [`service`]: request dispatch over any byte-stream transport
//!   (PF-406). Platform transports (Windows named pipe / future Linux UDS)
//!   live in `pf-windows` behind a tiny trait.

pub mod ipc;
pub mod recovery;
pub mod service;
pub mod single_instance;
#[cfg(target_os = "linux")]
pub mod stacks;
pub mod supervisor;

pub use service::DaemonService;
pub use single_instance::{SingleInstance, SingleInstanceError};
#[cfg(target_os = "linux")]
pub use stacks::{StackRecorder, StackSample};
pub use supervisor::{SessionSupervisor, SupervisorError, TargetSpec};
