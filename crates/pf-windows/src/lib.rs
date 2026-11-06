//! # pf-windows
//!
//! Windows backend: process counters, stable identity, process enumeration
//! and the ETW feasibility probe. Everything is cfg(windows); on other
//! targets this crate compiles to documentation.

#[cfg(windows)]
pub mod counters;
#[cfg(windows)]
pub mod enumerate;
#[cfg(windows)]
pub mod etw_probe;
#[cfg(windows)]
pub mod ffi;
#[cfg(windows)]
pub mod modules;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod symbols;
#[cfg(windows)]
pub mod theme;

#[cfg(windows)]
pub use counters::{WindowsCounterSource, WindowsPlatform};
#[cfg(windows)]
pub use etw_probe::{probe_etw_session_creation, probe_etw_trace_enumeration};
#[cfg(windows)]
pub use pipe::{PipeListener, PipeStream, connect, daemon_pipe_name};
#[cfg(windows)]
pub use theme::apps_use_light_theme;
