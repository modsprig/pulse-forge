//! Single shared report implementation lives in `pf-platform` (used by the
//! CLI **and** the daemon; there is exactly one metric rendering path).
pub use pf_platform::report::*;
