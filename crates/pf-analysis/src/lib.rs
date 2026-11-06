//! # pf-analysis
//!
//! Honest measurement comparison and stack aggregation (roadmap P11 subset
//! and P9 analysis core).
//!
//! Contract (architecture §10.2, "Diagnostics vs. recommendations"):
//! - a single A/B pair yields **observed deltas only** — never a claim of
//!   statistical significance;
//! - repeated paired runs yield distribution statistics (median, IQR, MAD)
//!   with explicit uncertainty, not invented confidence;
//! - CPU share (percent of samples) is never confused with CPU time;
//! - incompatible scopes are flagged; comparing them requires explicit
//!   opt-in and the output says so.

pub mod compare;
pub mod distribution;
pub mod export;
pub mod stacks;
pub mod stacks_io;
pub mod workload;

pub use compare::{ComparisonInput, MetricDelta, SessionFacts, compare_sessions};
pub use distribution::{Distribution, distribution_of};
pub use stacks::{StackTrie, aggregate_stacks};
pub use workload::{RepeatedMetric, RunResult, WorkloadSpec, compare_repeated};
