//! # pf-platform
//!
//! OS-neutral contracts and the recording engine. Platform backends
//! (`pf-windows`, future `pf-linux`) implement the traits; CLI, daemon, MCP
//! and GUI consume only these contracts, never OS APIs directly
//! (architecture §4: no collector operations in UI/MCP layers).

pub mod contracts;
pub mod export;
pub mod logging;
pub mod paths;
pub mod record;
pub mod report;
pub mod session_store;

pub use contracts::{
    CounterSource, PlatformBackend, PlatformCapabilities, ProbeStatus, ProcessEntry,
};
pub use export::{RedactionPolicy, export_session};
pub use record::{Recorder, RecorderConfig, RecorderOutcome, RecorderReport};
pub use report::{REPORT_METRICS, ReportInput, build_report, from_recorder, one_line_summary};
pub use session_store::{SCHEMA_0, SchemaVersion, SessionStore};
