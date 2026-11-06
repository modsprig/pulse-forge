//! # pf-types
//!
//! Core data model shared by every PulseForge layer (collectors, storage, CLI,
//! future daemon/MCP/GUI). This crate is deliberately dependency-free and
//! contains no OS API access: platform backends live behind `pf-platform`
//! traits and are implemented in `pf-windows` / future `pf-linux`.
//!
//! ## Semantic contracts enforced here
//!
//! - Every metric has a canonical name, a [`Unit`] and a per-sample
//!   [`Availability`][metrics::Availability] — an unavailable metric is
//!   *recorded as unavailable*, never as zero.
//! - [`ProcessIdentity`] binds PID + creation timestamp + boot context so PID
//!   reuse can never silently contaminate a session.
//! - Derived rates (like CPU core percent) distinguish "not yet computable"
//!   (first sample) from "reset/gap" explicitly.
//! - The [`json`] module is a bounded writer + parser: fixed depth limit,
//!   no panics on hostile input, integers kept exact.

pub mod error;
pub mod identity;
pub mod json;
pub mod metrics;
pub mod session;

pub use error::{ErrorCode, PfError};
pub use identity::{IdentityCheck, Os, ProcessIdentity};
pub use metrics::{
    Availability, CounterSnapshot, DerivedMetric, DerivedSample, MetricId, Unit, derive_samples,
};
pub use session::{SessionId, SessionState, StateTransitionError};
