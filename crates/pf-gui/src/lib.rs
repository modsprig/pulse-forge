//! # pf-gui
//!
//! The native PulseForge application (roadmap P7, ADR-002): `egui`/`eframe`
//! over WGPU, speaking to the same daemon as the CLI and the MCP bridge.
//! There is exactly one implementation of every number the UI shows — the
//! view models call the same types/derivation engines as the reports.
//!
//! Architecture (testability contract):
//! - [`viewmod`]: pure view models behind a [`DaemonClient`] trait — every
//!   behavior is unit-testable with a scripted fake, no window required.
//! - [`app`]: the egui layer — thin widgets over the view models; exercised
//!   headlessly (a full `update()` pass test, no GPU surface needed).
//! - [`pipe_client`]: the real named-pipe client (Windows).
//! - `pulseforge-gui` binary: opens the window.

pub mod app;
pub mod app_icon;
pub mod daemon_start;
pub mod flame;
#[cfg(windows)]
pub mod pipe_client;
#[doc(hidden)]
pub mod testing;
pub mod viewmod;

pub use app::{PulseForgeApp, Theme, open_session_offline};
pub use app_icon::{
    AppCategory, ProcessIconCache, get_desktop_window_titles, paint_category_icon,
    paint_process_icon,
};
#[cfg(windows)]
pub use pipe_client::PipeDaemonClient;
pub use viewmod::{
    ActionOutcome, CompareMetricRow, CompareModel, DaemonClient, DashboardModel, DiagnosticsModel,
    FakeDaemon, HomeModel, ProcessFilterTab, ProcessRow, ProcessSort, ProcessTreeNode,
    ProcessViewMode, SessionFilter, SessionRow, SessionSnapshot, SessionSort, StorageHealth, View,
};
