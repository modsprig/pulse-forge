//! GUI view models (roadmap PF-701/702/703): pure, headless-testable state
//! machines behind the egui widgets. The rendering layer stays thin; every
//! behavior (list refresh, attach flow, stop, live series assembly, warning
//! surfacing) is a view-model operation exercised by tests with a scripted
//! client — no window required (the same discipline the PF-008 spike
//! proved feasible).
//!
//! The daemon connection is behind the [`DaemonClient`] trait: the real
//! named-pipe client (Windows) in production, a [`FakeDaemon`] in tests.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId};

/// Operations the GUI needs from the daemon (a subset of the IPC service).
pub trait DaemonClient {
    fn capabilities(&self) -> Result<JsonValue, PfError>;

    /// PF-702: live process picker rows (bounded list from the daemon;
    /// image names may be sensitive — the UI treats them accordingly).
    fn list_processes(&self, limit: u32) -> Result<Vec<ProcessRow>, PfError>;

    /// PF-704: spike-preserving envelope rollup for a time window, served
    /// from the durable chunk store (real client) or in-memory samples
    /// (tests). One bucketing implementation lives in pf-storage.
    fn envelope(
        &self,
        id: SessionId,
        field: pf_storage::EnvelopeField,
        from_ns: u64,
        to_ns: u64,
        buckets: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError>;

    /// PF-707: storage health — data root, writability, session count and
    /// total bytes (from the same data `pulseforge doctor` reports).
    fn storage_health(&self) -> Result<StorageHealth, PfError>;

    /// PF-1106: observed-delta comparison of two sessions (shared
    /// pf_analysis engine via the daemon).
    fn compare(&self, baseline: SessionId, candidate: SessionId) -> Result<JsonValue, PfError>;
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError>;
    fn start_attach(
        &self,
        pid: u32,
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError>;
    /// Launch-and-record: start a NEW workload and record it (CLI
    /// `record --run` parity). The default implementation delegates to
    /// start_attach is not possible — implementors must handle it.
    fn start_run(
        &self,
        exe: &str,
        args: &[String],
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError>;
    fn status(&self, id: SessionId) -> Result<SessionSnapshot, PfError>;
    fn stop(&self, id: SessionId) -> Result<String, PfError>;
    /// Delete a terminal session (record + on-disk store); bytes freed on
    /// success. Live sessions are refused by the daemon — the UI confirms
    /// before calling.
    fn delete_session(&self, id: SessionId) -> Result<u64, PfError>;
    fn samples(&self, id: SessionId) -> Result<Vec<pf_types::CounterSnapshot>, PfError>;
}

/// One-shot background-slot: spawn a computation on a worker thread and
/// hand the result to the UI thread without ever blocking a frame. The
/// in-flight flag prevents stacking duplicate workers while one runs.
pub struct AsyncSlot<T: Send + 'static> {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<T>>>,
}

impl<T: Send + 'static> Default for AsyncSlot<T> {
    fn default() -> Self {
        AsyncSlot {
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rx: std::sync::Mutex::new(None),
        }
    }
}

impl<T: Send + 'static> AsyncSlot<T> {
    /// Spawn `f` on a worker thread unless one is already in flight.
    /// Returns true when the work was spawned.
    pub fn spawn<F>(&self, f: F) -> bool
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let was = self
            .in_flight
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        if was {
            return false;
        }
        let flag = std::sync::Arc::clone(&self.in_flight);
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut guard) = self.rx.lock() {
            *guard = Some(rx);
        }
        let spawned = std::thread::Builder::new()
            .name("pf-refresh".into())
            .spawn(move || {
                let value = f();
                let _ = tx.send(value);
                // In-flight clears when the worker finishes, success or
                // failure — the next cadence tick may spawn again.
                flag.store(false, std::sync::atomic::Ordering::Release);
            });
        if spawned.is_err() {
            // Thread spawn failed: release the slot so a later tick can
            // retry (no worker exists to clear it).
            self.in_flight
                .store(false, std::sync::atomic::Ordering::Release);
        }
        spawned.is_ok()
    }

    /// Whether a worker is currently running (no result yet).
    pub fn in_flight(&self) -> bool {
        self.in_flight.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Non-blocking take of a completed result.
    pub fn try_take(&self) -> Option<T> {
        let mut guard = self.rx.lock().ok()?;
        guard.as_mut().and_then(|rx| rx.try_recv().ok())
    }
}

/// How many process rows the picker requests per refresh. The daemon
/// protocol caps this at 4096 (`op_list_processes` min(4096)); the JSON
/// frame for the worst case is ~0.6-1.2 MB against the 4 MiB frame cap,
/// and the picker renders rows virtualized — so the old conservative 100
/// was a visible gap, not a budget: a machine with more than 100
/// processes could not reach the rest through search either (the filter
/// runs client-side over the fetched rows). Known cost, noted in the
/// ledger: the open picker filters/sorts every frame (~1-3 ms at 4096
/// rows in release); a borrow-based filter is the follow-up if that
/// ever shows.
pub const PROCESS_LIST_LIMIT: u32 = 4096;

/// Attach-bar recording mode: attach to a running pid, or launch a new
/// workload and record it (CLI `record --run` parity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordMode {
    Attach,
    Run,
}

/// One row of the live process picker (from `list_processes`).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRow {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub image: String,
    pub window_title: Option<String>,
    /// Percent of one logical core since the previous daemon enumeration
    /// (backend-computed delta). `None` = no baseline yet or access denied.
    pub cpu_pct: Option<f64>,
    /// Resident set size in bytes when the OS reports it.
    pub rss_bytes: Option<u64>,
}

impl ProcessRow {
    /// Returns the human-readable display title (window title if present, otherwise executable image name).
    pub fn display_title(&self) -> &str {
        self.window_title.as_deref().unwrap_or(&self.image)
    }

    /// Whether this process has an active top-level desktop window with a title.
    pub fn has_window(&self) -> bool {
        self.window_title.is_some()
    }
}

/// One row of the session browser (from `list_sessions`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    pub state: String,
    pub samples: u64,
    pub image: String,
    pub stop_reason: Option<String>,
}

/// Live snapshot of one session (from `get_session`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub state: String,
    pub samples: u64,
    pub session_dir: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// PF-707 diagnostics: storage + backend status surfaced to the drawer.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageHealth {
    pub data_root: String,
    pub writable: bool,
    pub session_count: usize,
    pub total_bytes: u64,
    /// Capability summaries (availability + reason code) for counters and
    /// stack sampling.
    pub counters_available: bool,
    pub counters_reason: String,
    pub stacks_available: bool,
    pub stacks_reason: String,
    pub os_version: String,
    pub logical_cpus: Option<u32>,
}

/// Which view is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    Home,
    Dashboard(SessionId),
}

/// Outcome of a user action, for assertion in tests and status text in UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionOutcome {
    None,
    Started(SessionId),
    Stopped(String),
    Failed(String),
}

/// Status filter for the session browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionFilter {
    #[default]
    All,
    Recording,
    Completed,
    Failed,
}

/// Sort order for the live processes list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessSort {
    #[default]
    CpuDesc,
    PidAsc,
    PidDesc,
    NameAsc,
    NameDesc,
}

impl ProcessSort {
    pub fn label(self) -> &'static str {
        match self {
            ProcessSort::CpuDesc => "CPU (hot first)",
            ProcessSort::PidAsc => "PID (asc)",
            ProcessSort::PidDesc => "PID (desc)",
            ProcessSort::NameAsc => "Name (A-Z)",
            ProcessSort::NameDesc => "Name (Z-A)",
        }
    }

    pub fn cycle(&mut self) -> Self {
        *self = match self {
            ProcessSort::CpuDesc => ProcessSort::PidAsc,
            ProcessSort::PidAsc => ProcessSort::PidDesc,
            ProcessSort::PidDesc => ProcessSort::NameAsc,
            ProcessSort::NameAsc => ProcessSort::NameDesc,
            ProcessSort::NameDesc => ProcessSort::CpuDesc,
        };
        *self
    }
}

/// View presentation mode for live processes (Flat list vs Parent/Child tree).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessViewMode {
    #[default]
    Flat,
    Tree,
}

/// Tab filter for live processes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessFilterTab {
    #[default]
    All,
    Recording,
}

/// One rendered node in the hierarchical process tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessTreeNode {
    pub process: ProcessRow,
    pub depth: usize,
    pub has_children: bool,
    pub is_expanded: bool,
    pub children_count: usize,
}

/// Sort order for the session browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionSort {
    #[default]
    Newest,
    Oldest,
    MostSamples,
    Name,
}

/// Background-refresh payload for the home view model.
pub struct HomeRefresh {
    pub sessions: Vec<SessionRow>,
    pub processes: Vec<ProcessRow>,
    pub status_line: String,
    /// True when the sessions call failed with the daemon-unreachable
    /// timeout (auto-start trigger).
    pub daemon_unreachable: bool,
}

/// The home view model: session list + attach form + refresh bookkeeping.
pub struct HomeModel {
    pub sessions: Vec<SessionRow>,
    /// PF-702: live process picker rows (refreshed with the sessions).
    pub processes: Vec<ProcessRow>,
    pub process_sort: ProcessSort,
    /// Destructive delete in flight (confirmed in the modal); the typed
    /// outcome lands through `delete_slot`.
    pub deleting: Option<SessionId>,
    pub delete_slot: AsyncSlot<Result<u64, String>>,
    pub process_view_mode: ProcessViewMode,
    pub process_tab: ProcessFilterTab,
    pub collapsed_pids: HashSet<u32>,
    pub session_filter: SessionFilter,
    pub session_sort: SessionSort,
    pub last_refresh: Option<Instant>,
    pub refresh_interval: Duration,
    pub attach_pid: String,
    pub attach_duration_ms: String,
    pub attach_interval_ms: String,
    pub attach_label: String,
    /// Launch-and-record mode (CLI `record --run` parity).
    pub record_mode: RecordMode,
    pub run_exe: String,
    pub run_args: String,
    /// Background refresh slot: daemon calls run OFF the UI thread so a
    /// slow/unreachable daemon can never freeze the window.
    pub refresh_slot: AsyncSlot<HomeRefresh>,
    /// Start-action slot: the daemon round-trip for attach/run runs OFF
    /// the UI thread; the outcome lands via [`HomeModel::apply_outcome`].
    pub outcome_slot: AsyncSlot<ActionOutcome>,
    /// On-demand daemon auto-start: spawn + wait runs OFF the UI thread.
    pub autostart_slot: AsyncSlot<Result<(), String>>,
    /// Whether the latest applied refresh found the daemon unreachable
    /// (drives the attempt; see [`HomeModel::maybe_autostart_daemon`]).
    pub last_refresh_unreachable: bool,
    last_autostart: Option<Instant>,
    /// Which start action is in flight (for honest status lines).
    pending_action: &'static str,
    pub last_outcome: ActionOutcome,
    pub status_line: String,
}

impl Default for HomeModel {
    fn default() -> Self {
        HomeModel {
            sessions: Vec::new(),
            processes: Vec::new(),
            process_sort: ProcessSort::default(),
            process_view_mode: ProcessViewMode::Flat,
            process_tab: ProcessFilterTab::All,
            collapsed_pids: HashSet::new(),
            deleting: None,
            delete_slot: AsyncSlot::default(),
            session_filter: SessionFilter::All,
            session_sort: SessionSort::Newest,
            last_refresh: None,
            refresh_interval: Duration::from_millis(1500),
            attach_pid: String::new(),
            attach_duration_ms: "10000".into(),
            attach_interval_ms: "250".into(),
            attach_label: String::new(),
            record_mode: RecordMode::Attach,
            run_exe: String::new(),
            run_args: String::new(),
            refresh_slot: AsyncSlot::default(),
            outcome_slot: AsyncSlot::default(),
            autostart_slot: AsyncSlot::default(),
            last_refresh_unreachable: false,
            last_autostart: None,
            pending_action: "start",
            last_outcome: ActionOutcome::None,
            status_line: "ready".into(),
        }
    }
}

impl HomeModel {
    /// Parse human-friendly duration string (e.g. "5s", "10s", "1m", "500ms", "10000").
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn parse_duration_friendly(text: &str) -> Option<u64> {
        let t = text.trim().to_lowercase();
        if t.is_empty() {
            return None;
        }
        if let Some(s) = t.strip_suffix("ms") {
            return s.trim().parse::<u64>().ok();
        }
        if let Some(s) = t.strip_suffix('s') {
            return s
                .trim()
                .parse::<f64>()
                .ok()
                .map(|sec| (sec * 1000.0) as u64);
        }
        if let Some(s) = t.strip_suffix('m') {
            return s
                .trim()
                .parse::<f64>()
                .ok()
                .map(|min| (min * 60.0 * 1000.0) as u64);
        }
        t.parse::<u64>().ok()
    }

    /// Parse + validate the attach form. Errors are user-facing strings
    /// (typed codes preserved in the outcome).
    pub fn validate_attach_form(&self) -> Result<(u32, u64, u64), String> {
        let pid: u32 = self
            .attach_pid
            .trim()
            .parse()
            .map_err(|_| format!("pid must be a number, got {:?}", self.attach_pid))?;
        let duration: u64 = Self::parse_duration_friendly(&self.attach_duration_ms)
            .ok_or_else(|| "--duration must be a number".to_string())?;
        if !(1..=3_600_000).contains(&duration) {
            return Err("duration must be within 1..=3600000 ms".into());
        }
        let interval: u64 = self
            .attach_interval_ms
            .trim()
            .parse()
            .map_err(|_| "interval must be a number".to_string())?;
        if !(100..=5_000).contains(&interval) {
            return Err("interval must be within 100..=5000 ms".into());
        }
        Ok((pid, duration, interval))
    }

    /// Refresh the session list (called by the UI on its cadence).
    pub fn refresh(&mut self, client: &dyn DaemonClient) {
        match client.list_sessions() {
            Ok(rows) => {
                self.sessions = rows;
                self.last_refresh = Some(Instant::now());
                self.status_line = format!("{} session(s)", self.sessions.len());
            }
            Err(e) => {
                self.status_line = format!("refresh failed: {e}");
            }
        }
        // PF-702: process picker refreshes alongside; failure is surfaced
        // but never blocks the session browser.
        match client.list_processes(PROCESS_LIST_LIMIT) {
            Ok(mut rows) => {
                let titles = crate::app_icon::get_desktop_window_titles();
                for r in &mut rows {
                    if r.window_title.is_none() {
                        if let Some(t) = titles.get(&r.pid) {
                            r.window_title = Some(t.clone());
                        }
                    }
                }
                self.processes = rows;
            }
            Err(e) => self.status_line = format!("{} (process list: {e})", self.status_line),
        }
    }

    /// Filtered picker rows (case-insensitive substring on pid/image/window_title).
    pub fn filter_processes(&self, query: &str) -> Vec<ProcessRow> {
        let q = query.trim().to_lowercase();
        self.processes
            .iter()
            .filter(|p| {
                q.is_empty()
                    || p.image.to_lowercase().contains(&q)
                    || p.pid.to_string().contains(&q)
                    || p.window_title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    /// Fill the attach form from a picked row (pid in, image echoed as the
    /// label suggestion).
    pub fn pick_process(&mut self, row: &ProcessRow) {
        self.attach_pid = row.pid.to_string();
        self.attach_label = row
            .window_title
            .clone()
            .unwrap_or_else(|| row.image.clone());
    }

    /// Sort a mutable slice of ProcessRow in place.
    pub fn sort_processes(list: &mut [ProcessRow], sort: ProcessSort) {
        match sort {
            ProcessSort::PidAsc => list.sort_by_key(|p| p.pid),
            ProcessSort::PidDesc => list.sort_by_key(|p| std::cmp::Reverse(p.pid)),
            ProcessSort::NameAsc => list.sort_by_key(|p| p.display_title().to_lowercase()),
            ProcessSort::NameDesc => {
                list.sort_by_key(|p| std::cmp::Reverse(p.display_title().to_lowercase()));
            }
            // Hottest first; rows without a delta (no baseline / denied)
            // sort last so absence never masquerades as "idle".
            ProcessSort::CpuDesc => {
                list.sort_by(|a, b| {
                    let av = a.cpu_pct.unwrap_or(f64::NEG_INFINITY);
                    let bv = b.cpu_pct.unwrap_or(f64::NEG_INFINITY);
                    bv.partial_cmp(&av)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.pid.cmp(&b.pid))
                });
            }
        }
    }

    /// Check if a process is actively being recorded in an existing session.
    pub fn is_process_recording(&self, p: &ProcessRow) -> Option<&SessionRow> {
        self.sessions.iter().find(|s| {
            (s.state == "RECORDING" || s.state == "STARTING")
                && (s.image.eq_ignore_ascii_case(&p.image)
                    || s.image
                        .eq_ignore_ascii_case(p.image.trim_end_matches(".exe")))
        })
    }

    /// Dispatch a destructive session delete off the UI thread (the
    /// confirm modal already gated it; one delete at a time).
    pub fn request_delete_async(
        &mut self,
        client: std::sync::Arc<dyn DaemonClient + Send + Sync>,
        id: SessionId,
    ) {
        self.deleting = Some(id);
        self.delete_slot
            .spawn(move || client.delete_session(id).map_err(|e| e.to_string()));
    }

    /// Non-blocking take of a finished delete outcome; clears `deleting`.
    pub fn apply_delete_result(&mut self) -> Option<Result<u64, String>> {
        let out = self.delete_slot.try_take()?;
        self.deleting = None;
        Some(out)
    }

    /// Count of active processes currently being recorded by PulseForge.
    pub fn recording_process_count(&self) -> usize {
        self.processes
            .iter()
            .filter(|p| self.is_process_recording(p).is_some())
            .count()
    }

    /// Filtered and sorted processes for Flat mode.
    pub fn filtered_and_sorted_processes(&self, query: &str) -> Vec<ProcessRow> {
        let mut list = match self.process_tab {
            ProcessFilterTab::All => self.filter_processes(query),
            ProcessFilterTab::Recording => {
                let all = self.filter_processes(query);
                all.into_iter()
                    .filter(|p| self.is_process_recording(p).is_some())
                    .collect()
            }
        };
        Self::sort_processes(&mut list, self.process_sort);
        list
    }

    /// Flattened visible tree nodes for Tree mode with ancestor expansion on search.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn visible_tree_nodes(&self, query: &str) -> Vec<ProcessTreeNode> {
        let q = query.trim().to_lowercase();
        let all = &self.processes;
        if all.is_empty() {
            return Vec::new();
        }

        let all_pids: HashSet<u32> = all.iter().map(|p| p.pid).collect();
        let mut parent_to_children: HashMap<u32, Vec<ProcessRow>> = HashMap::new();
        for p in all {
            if let Some(ppid) = p.parent_pid {
                if all_pids.contains(&ppid) && ppid != p.pid {
                    parent_to_children.entry(ppid).or_default().push(p.clone());
                }
            }
        }

        fn matches_node_or_desc(
            p: &ProcessRow,
            q: &str,
            tab: ProcessFilterTab,
            home: &HomeModel,
            p2c: &HashMap<u32, Vec<ProcessRow>>,
        ) -> bool {
            let tab_ok = match tab {
                ProcessFilterTab::All => true,
                ProcessFilterTab::Recording => home.is_process_recording(p).is_some(),
            };
            let text_ok = q.is_empty()
                || p.image.to_lowercase().contains(q)
                || p.pid.to_string().contains(q)
                || p.window_title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase().contains(q));
            if tab_ok && text_ok {
                return true;
            }
            if let Some(children) = p2c.get(&p.pid) {
                for c in children {
                    if matches_node_or_desc(c, q, tab, home, p2c) {
                        return true;
                    }
                }
            }
            false
        }

        let mut roots: Vec<ProcessRow> = all
            .iter()
            .filter(|p| match p.parent_pid {
                None => true,
                Some(ppid) => !all_pids.contains(&ppid) || ppid == p.pid,
            })
            .cloned()
            .collect();

        Self::sort_processes(&mut roots, self.process_sort);

        let mut out = Vec::new();

        #[allow(clippy::too_many_arguments)]
        fn traverse(
            node: ProcessRow,
            depth: usize,
            q: &str,
            tab: ProcessFilterTab,
            home: &HomeModel,
            sort: ProcessSort,
            collapsed: &HashSet<u32>,
            p2c: &HashMap<u32, Vec<ProcessRow>>,
            out: &mut Vec<ProcessTreeNode>,
        ) {
            if !matches_node_or_desc(&node, q, tab, home, p2c) {
                return;
            }
            let children = p2c.get(&node.pid);
            let has_children = children.map(|c| !c.is_empty()).unwrap_or(false);
            let children_count = children.map(|c| c.len()).unwrap_or(0);
            let is_expanded = if !q.is_empty() {
                true
            } else {
                !collapsed.contains(&node.pid)
            };

            out.push(ProcessTreeNode {
                process: node.clone(),
                depth,
                has_children,
                is_expanded,
                children_count,
            });

            if has_children && is_expanded {
                if let Some(mut ch_list) = children.cloned() {
                    HomeModel::sort_processes(&mut ch_list, sort);
                    for child in ch_list {
                        traverse(
                            child,
                            depth.saturating_add(1),
                            q,
                            tab,
                            home,
                            sort,
                            collapsed,
                            p2c,
                            out,
                        );
                    }
                }
            }
        }

        for root in roots {
            traverse(
                root,
                0,
                &q,
                self.process_tab,
                self,
                self.process_sort,
                &self.collapsed_pids,
                &parent_to_children,
                &mut out,
            );
        }

        out
    }

    /// Toggle expansion of a process node in Tree mode.
    pub fn toggle_process_expanded(&mut self, pid: u32) {
        if self.collapsed_pids.contains(&pid) {
            self.collapsed_pids.remove(&pid);
        } else {
            self.collapsed_pids.insert(pid);
        }
    }

    /// Expand all processes in Tree mode.
    pub fn expand_all_processes(&mut self) {
        self.collapsed_pids.clear();
    }

    /// Collapse all processes that have children in Tree mode.
    pub fn collapse_all_processes(&mut self) {
        let all_pids: HashSet<u32> = self.processes.iter().map(|p| p.pid).collect();
        for p in &self.processes {
            if let Some(ppid) = p.parent_pid {
                if all_pids.contains(&ppid) {
                    self.collapsed_pids.insert(ppid);
                }
            }
        }
    }

    /// Compute the refresh payload (runs on a worker thread).
    pub fn compute_home_refresh(client: &dyn DaemonClient) -> HomeRefresh {
        let mut out = HomeRefresh {
            sessions: Vec::new(),
            processes: Vec::new(),
            status_line: String::new(),
            daemon_unreachable: false,
        };
        match client.list_sessions() {
            Ok(rows) => {
                out.sessions = rows;
                out.status_line = format!("{} session(s)", out.sessions.len());
            }
            Err(e) => {
                out.daemon_unreachable = matches!(e.code, pf_types::ErrorCode::Timeout);
                out.status_line = format!("refresh failed: {e}");
            }
        }
        match client.list_processes(PROCESS_LIST_LIMIT) {
            Ok(mut rows) => {
                let titles = crate::app_icon::get_desktop_window_titles();
                for r in &mut rows {
                    if r.window_title.is_none() {
                        if let Some(t) = titles.get(&r.pid) {
                            r.window_title = Some(t.clone());
                        }
                    }
                }
                out.processes = rows;
            }
            Err(e) => {
                // The same pipe failure reaches both calls; do not print
                // the identical wall of text twice.
                let msg = e.to_string();
                if !out.status_line.contains(&msg) {
                    out.status_line = format!("{} (process list: {msg})", out.status_line);
                }
            }
        }
        out
    }

    /// Request a background refresh when the cadence says so. Never
    /// blocks the UI thread; results arrive via [`Self::apply_refresh`].
    pub fn request_refresh_if_due(
        &mut self,
        client: std::sync::Arc<dyn DaemonClient + Send + Sync>,
        now: Instant,
    ) {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= self.refresh_interval);
        if due {
            // The cadence stamp only moves when a worker actually starts:
            // a request blocked by the in-flight guard retries next frame
            // instead of skipping a full cycle after the worker finishes.
            if self
                .refresh_slot
                .spawn(move || Self::compute_home_refresh(client.as_ref()))
            {
                self.last_refresh = Some(now);
            }
        }
    }

    /// Request a refresh immediately (user action), ignoring the cadence.
    /// Still bounded by the in-flight guard; never blocks the UI thread.
    pub fn request_refresh_now(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        if self
            .refresh_slot
            .spawn(move || Self::compute_home_refresh(client.as_ref()))
        {
            self.last_refresh = Some(Instant::now());
        }
    }

    /// Apply a completed background refresh (non-blocking).
    pub fn apply_refresh(&mut self) {
        if let Some(r) = self.refresh_slot.try_take() {
            self.sessions = r.sessions;
            self.processes = r.processes;
            self.last_refresh_unreachable = r.daemon_unreachable;
            // While an auto-start attempt is in flight its status is the
            // honest one; a mid-flight refresh must not clobber it.
            if !self.autostart_slot.in_flight() {
                self.status_line = r.status_line;
            }
        }
    }

    /// If the daemon is unreachable and the cooldown has elapsed, launch
    /// it off-thread and wait for the pipe. Runs every frame; the
    /// guards make it a no-op unless an attempt is genuinely due.
    pub fn maybe_autostart_daemon(
        &mut self,
        client: std::sync::Arc<dyn DaemonClient + Send + Sync>,
    ) {
        if !self.last_refresh_unreachable || self.autostart_slot.in_flight() {
            return;
        }
        if !self
            .last_autostart
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(30))
        {
            return;
        }
        self.last_autostart = Some(Instant::now());
        self.status_line = "daemon not running — auto-starting…".into();
        self.autostart_slot.spawn(move || {
            let Some(exe) = crate::daemon_start::daemon_exe() else {
                return Err(
                    "daemon binary not found next to the GUI — start it with `pulseforge daemon`"
                        .into(),
                );
            };
            crate::daemon_start::spawn_detached(&exe).map_err(|e| e.to_string())?;
            // Wait for the pipe through the real client (bounded).
            let deadline = std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(6))
                .unwrap_or_else(std::time::Instant::now);
            loop {
                match client.list_sessions() {
                    Ok(_) => return Ok(()),
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    Err(e) => return Err(format!("daemon did not come up: {e}")),
                }
            }
        });
    }

    /// Apply a finished auto-start attempt (non-blocking): success forces
    /// a refresh, failure keeps the manual instruction visible.
    pub fn apply_autostart(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        if let Some(r) = self.autostart_slot.try_take() {
            match r {
                Ok(()) => {
                    self.status_line = "daemon auto-started — connected".into();
                    self.last_refresh_unreachable = false;
                    // Immediate data fetch now that the pipe is up.
                    if !self.refresh_slot.in_flight() {
                        self.refresh_slot
                            .spawn(move || Self::compute_home_refresh(client.as_ref()));
                    }
                }
                Err(msg) => {
                    self.status_line = format!("auto-start failed: {msg}");
                }
            }
        }
    }

    /// Refresh only when the cadence says so (UI calls this every frame).
    pub fn refresh_if_due(&mut self, client: &dyn DaemonClient, now: Instant) {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= self.refresh_interval);
        if due {
            self.refresh(client);
        }
    }

    /// Submit the attach form: starts a recording via the daemon and
    /// returns the dashboard to open (the UI decides navigation).
    pub fn submit_attach(&mut self, client: &dyn DaemonClient) -> Option<SessionId> {
        match self.validate_attach_form() {
            Ok((pid, duration, interval)) => {
                match client.start_attach(pid, duration, interval, &self.attach_label) {
                    Ok(id) => {
                        self.last_outcome = ActionOutcome::Started(id);
                        self.status_line = format!("recording {id}");
                        Some(id)
                    }
                    Err(e) => {
                        self.last_outcome = ActionOutcome::Failed(e.to_string());
                        self.status_line = format!("attach failed: {e}");
                        None
                    }
                }
            }
            Err(msg) => {
                self.last_outcome = ActionOutcome::Failed(
                    PfError::new(ErrorCode::InvalidArgument, msg.clone()).to_string(),
                );
                self.status_line = msg;
                None
            }
        }
    }

    /// Launch-and-record (CLI `record --run` parity): create a session
    /// targeting a NEW workload, then start recording it.
    pub fn submit_run(&mut self, client: &dyn DaemonClient) -> Option<SessionId> {
        if self.run_exe.trim().is_empty() {
            self.last_outcome = ActionOutcome::Failed("run mode needs an executable path".into());
            return None;
        }
        let duration: u64 = self.attach_duration_ms.parse().unwrap_or(10_000);
        let interval: u64 = self.attach_interval_ms.parse().unwrap_or(250);
        let args: Vec<String> = self
            .run_args
            .split_whitespace()
            .map(str::to_string)
            .collect();
        match client.start_run(
            self.run_exe.trim(),
            &args,
            duration,
            interval,
            &self.attach_label,
        ) {
            Ok(id) => {
                self.last_outcome = ActionOutcome::Started(id);
                self.status_line = format!("recording {id}");
                Some(id)
            }
            Err(e) => {
                self.last_outcome = ActionOutcome::Failed(e.to_string());
                self.status_line = format!("run failed: {e}");
                None
            }
        }
    }

    /// Attach-and-record: validate the form synchronously (instant, no
    /// IPC), then run the daemon round-trip OFF the UI thread. The result
    /// lands via [`Self::apply_outcome`].
    pub fn submit_attach_async(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        let (pid, duration, interval) = match self.validate_attach_form() {
            Ok(v) => v,
            Err(msg) => {
                self.last_outcome = ActionOutcome::Failed(
                    PfError::new(ErrorCode::InvalidArgument, msg.clone()).to_string(),
                );
                self.status_line = msg;
                return;
            }
        };
        let label = self.attach_label.clone();
        self.pending_action = "attach";
        self.status_line = "starting…".into();
        self.outcome_slot.spawn(move || {
            match client.start_attach(pid, duration, interval, &label) {
                Ok(id) => ActionOutcome::Started(id),
                Err(e) => ActionOutcome::Failed(e.to_string()),
            }
        });
    }

    /// Launch-and-record, off the UI thread (see [`Self::submit_attach_async`]).
    pub fn submit_run_async(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        if self.run_exe.trim().is_empty() {
            self.last_outcome = ActionOutcome::Failed("run mode needs an executable path".into());
            return;
        }
        let duration: u64 = self.attach_duration_ms.parse().unwrap_or(10_000);
        let interval: u64 = self.attach_interval_ms.parse().unwrap_or(250);
        let args: Vec<String> = self
            .run_args
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let exe = self.run_exe.trim().to_string();
        let label = self.attach_label.clone();
        self.pending_action = "run";
        self.status_line = "starting…".into();
        self.outcome_slot.spawn(move || {
            match client.start_run(&exe, &args, duration, interval, &label) {
                Ok(id) => ActionOutcome::Started(id),
                Err(e) => ActionOutcome::Failed(e.to_string()),
            }
        });
    }

    /// Apply a finished start action (non-blocking). Returns the new
    /// session id exactly once, when a Started outcome lands, so the
    /// caller can navigate to its dashboard.
    pub fn apply_outcome(&mut self) -> Option<SessionId> {
        match self.outcome_slot.try_take()? {
            ActionOutcome::Started(id) => {
                self.status_line = format!("recording {id}");
                self.last_outcome = ActionOutcome::Started(id);
                Some(id)
            }
            ActionOutcome::Failed(msg) => {
                self.status_line = format!("{} failed: {msg}", self.pending_action);
                self.last_outcome = ActionOutcome::Failed(msg);
                None
            }
            ActionOutcome::Stopped(_) | ActionOutcome::None => None,
        }
    }

    /// Filter rows for the browser search box (case-insensitive substring
    /// over id/image/state), taking status filter and sort order into account.
    pub fn filter_rows(&self, query: &str) -> Vec<SessionRow> {
        let q = query.trim().to_lowercase();
        let mut rows: Vec<SessionRow> = self
            .sessions
            .iter()
            .filter(|r| {
                let matches_query = q.is_empty()
                    || r.id.as_hyphenated().to_lowercase().contains(&q)
                    || r.image.to_lowercase().contains(&q)
                    || r.state.to_lowercase().contains(&q);
                let matches_status = match self.session_filter {
                    SessionFilter::All => true,
                    SessionFilter::Recording => r.state == "RECORDING" || r.state == "STARTING",
                    SessionFilter::Completed => r.state == "COMPLETED",
                    SessionFilter::Failed => r.state == "FAILED" || r.state == "CANCELLED",
                };
                matches_query && matches_status
            })
            .cloned()
            .collect();

        match self.session_sort {
            SessionSort::Newest => {
                // Default: preserve natural session order
            }
            SessionSort::Oldest => {
                rows.sort_by_key(|a| a.id.unix_ms());
            }
            SessionSort::MostSamples => {
                rows.sort_by_key(|b| std::cmp::Reverse(b.samples));
            }
            SessionSort::Name => {
                rows.sort_by_key(|a| a.image.to_lowercase());
            }
        }
        rows
    }
}

/// One chartable series for the dashboard.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub name: &'static str,
    pub unit: &'static str,
    /// (mono_ns, value) points, time-ordered.
    pub points: Vec<(u64, f64)>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Series {
    #[allow(clippy::arithmetic_side_effects, clippy::cast_precision_loss)]
    pub fn mean(&self) -> Option<f64> {
        if self.points.is_empty() {
            None
        } else {
            let sum: f64 = self.points.iter().map(|p| p.1).sum();
            Some(sum / self.points.len() as f64)
        }
    }

    pub fn peak(&self) -> Option<f64> {
        self.max
    }

    fn from_samples(
        name: &'static str,
        unit: &'static str,
        pick: impl Fn(&pf_types::CounterSnapshot) -> Option<f64>,
        samples: &[pf_types::CounterSnapshot],
    ) -> Series {
        let mut points = Vec::with_capacity(samples.len());
        for s in samples {
            if let Some(v) = pick(s) {
                points.push((s.mono_ns, v));
            }
        }
        let min = points
            .iter()
            .map(|p| p.1)
            .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.min(v))));
        let max = points
            .iter()
            .map(|p| p.1)
            .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.max(v))));
        Series {
            name,
            unit,
            points,
            min,
            max,
        }
    }
}

/// The dashboard view model for one session.
/// Background-refresh payload for the dashboard (everything the refresh
/// mutates; applied to the model on the UI thread when it arrives).
#[derive(Clone)]
pub struct DashboardRefresh {
    pub state: String,
    pub samples: u64,
    pub session_dir: Option<std::path::PathBuf>,
    pub warnings: Vec<String>,
    pub data_start_ns: Option<u64>,
    pub data_end_ns: Option<u64>,
    pub resident_bytes: Series,
    pub io_bytes: Series,
    pub cpu_core_pct: Series,
}

pub struct DashboardModel {
    pub id: SessionId,
    pub state: String,
    pub samples: u64,
    pub warnings: Vec<String>,
    pub session_dir: Option<PathBuf>,
    pub cpu_core_pct: Series,
    pub resident_bytes: Series,
    pub io_bytes: Series,
    pub last_refresh: Option<Instant>,
    pub refresh_slot: AsyncSlot<DashboardRefresh>,
    /// Stop outcome slot: the daemon round-trip runs OFF the UI thread.
    pub stop_slot: AsyncSlot<String>,
    pub refresh_interval: Duration,
    pub stop_status: Option<String>,
    /// PF-704 viewport: None = full data range. When set, refresh() pulls
    /// envelope buckets instead of raw series (spike-preserving, bounded).
    pub view_start_ns: Option<u64>,
    pub view_end_ns: Option<u64>,
    /// Full data time range as last observed (for clamping zoom/pan).
    pub data_start_ns: Option<u64>,
    pub data_end_ns: Option<u64>,
    pub zoomed: bool,
    /// Synchronized hover timestamp across all metric charts.
    pub hovered_timestamp: Option<u64>,
}

// Series assembly arithmetic on bounded sample counts.
#[allow(clippy::arithmetic_side_effects)]
impl DashboardModel {
    pub fn new(id: SessionId) -> DashboardModel {
        DashboardModel {
            id,
            state: "UNKNOWN".into(),
            samples: 0,
            warnings: Vec::new(),
            session_dir: None,
            view_start_ns: None,
            view_end_ns: None,
            data_start_ns: None,
            data_end_ns: None,
            zoomed: false,
            hovered_timestamp: None,
            cpu_core_pct: Series {
                name: "cpu_core_pct",
                unit: "percent",
                points: Vec::new(),
                min: None,
                max: None,
            },
            resident_bytes: Series {
                name: "resident_bytes",
                unit: "bytes",
                points: Vec::new(),
                min: None,
                max: None,
            },
            io_bytes: Series {
                name: "io_bytes",
                unit: "bytes",
                points: Vec::new(),
                min: None,
                max: None,
            },
            last_refresh: None,
            refresh_slot: AsyncSlot::default(),
            stop_slot: AsyncSlot::default(),
            refresh_interval: Duration::from_millis(700),
            stop_status: None,
        }
    }

    /// Pull status + samples and rebuild the series. CPU rate is derived
    /// with the same semantics engine as the reports (no third
    /// implementation).
    /// Compute the dashboard refresh payload (runs on a worker thread).
    /// `window` is the effective viewport captured at request time.
    pub fn compute_dashboard_refresh(
        client: &dyn DaemonClient,
        id: SessionId,
        window: (u64, u64),
        zoomed: bool,
    ) -> DashboardRefresh {
        let mut state = String::new();
        let mut samples_count = 0u64;
        let mut session_dir: Option<std::path::PathBuf> = None;
        let mut warnings: Vec<String> = Vec::new();
        let mut data_start_ns: Option<u64> = None;
        let mut data_end_ns: Option<u64> = None;
        let mut cpu = Series {
            name: "",
            unit: "",
            points: Vec::new(),
            min: None,
            max: None,
        };
        let mut resident = Series {
            name: "",
            unit: "",
            points: Vec::new(),
            min: None,
            max: None,
        };
        let mut io = Series {
            name: "",
            unit: "",
            points: Vec::new(),
            min: None,
            max: None,
        };
        match client.status(id) {
            Ok(snap) => {
                state = snap.state;
                samples_count = snap.samples;
                session_dir = snap.session_dir;
                warnings = snap.warnings;
            }
            Err(e) => {
                warnings.push(format!("status failed: {e}"));
            }
        }
        // One full fetch per refresh; the zoomed CPU window below slices
        // this in memory instead of re-fetching the whole payload again
        // (100k-sample refetch per zoomed frame was a measurable stall).
        let mut samples_all: Vec<pf_types::CounterSnapshot> = Vec::new();
        if let Ok(samples) = client.samples(id) {
            // Track the full data range for zoom/pan clamping (PF-704).
            if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
                data_start_ns = Some(first.mono_ns);
                data_end_ns = Some(last.mono_ns);
            }
            let derived = pf_types::metrics::derive_samples(&samples, None);
            // CPU rate from derived samples (availability-aware).
            let mut cpu_points = Vec::with_capacity(derived.len());
            for d in &derived {
                let value = d.metrics.iter().find_map(|m| {
                    if m.id == pf_types::MetricId::CpuCorePct {
                        match &m.availability {
                            pf_types::Availability::Available(v) => Some(*v),
                            _ => None,
                        }
                    } else {
                        None
                    }
                });
                if let Some(v) = value {
                    cpu_points.push((d.mono_ns, v));
                }
            }
            let cpu_min = cpu_points
                .iter()
                .map(|p| p.1)
                .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.min(v))));
            let cpu_max = cpu_points
                .iter()
                .map(|p| p.1)
                .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.max(v))));
            cpu = Series {
                name: "cpu_core_pct",
                unit: "percent",
                points: cpu_points,
                min: cpu_min,
                max: cpu_max,
            };
            resident = Series::from_samples(
                "resident_bytes",
                "bytes",
                |s| s.resident_bytes.map(|v| v as f64),
                &samples,
            );
            // I/O chart: cumulative read+write transfer.
            io = Series::from_samples(
                "io_bytes",
                "bytes",
                |s| match (s.io_read_bytes, s.io_write_bytes) {
                    (Some(r), Some(w)) => Some(r.saturating_add(w) as f64),
                    _ => None,
                },
                &samples,
            );
            samples_count = samples.len() as u64;
            samples_all = samples;
        }
        // PF-704: when a viewport is set, replace raw series with envelope
        // buckets (bounded per-bucket data, spikes preserved by min/max).
        if zoomed {
            let (from, to) = window;
            for (field, slot) in [
                (pf_storage::EnvelopeField::ResidentBytes, &mut resident),
                (pf_storage::EnvelopeField::IoReadBytes, &mut io),
            ] {
                match client.envelope(id, field, from, to, 128) {
                    Ok(buckets) => {
                        slot.points = buckets
                            .iter()
                            .filter(|b| b.count > 0)
                            .map(|b| (b.bucket_start_ns, b.min.unwrap_or(0.0)))
                            .collect();
                        slot.min = buckets
                            .iter()
                            .filter_map(|b| b.min)
                            .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.min(v))));
                        slot.max = buckets
                            .iter()
                            .filter_map(|b| b.max)
                            .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.max(v))));
                    }
                    Err(e) => warnings.push(format!("envelope failed: {e}")),
                }
            }
            // CPU: derive rates over the window slice (shared derivation),
            // sliced from the single fetch above.
            {
                let window: Vec<pf_types::CounterSnapshot> = samples_all
                    .iter()
                    .filter(|s| s.mono_ns >= from && s.mono_ns <= to)
                    .cloned()
                    .collect();
                let derived = pf_types::metrics::derive_samples(&window, None);
                let pts: Vec<(u64, f64)> = derived
                    .iter()
                    .filter_map(|d| {
                        d.metrics.iter().find_map(|m| {
                            if m.id == pf_types::MetricId::CpuCorePct {
                                match &m.availability {
                                    pf_types::Availability::Available(v) => Some((d.mono_ns, *v)),
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        })
                    })
                    .collect();
                let min = pts
                    .iter()
                    .map(|p| p.1)
                    .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.min(v))));
                let max = pts
                    .iter()
                    .map(|p| p.1)
                    .fold(None::<f64>, |a, v| Some(a.map_or(v, |m: f64| m.max(v))));
                cpu = Series {
                    name: "cpu_core_pct",
                    unit: "percent",
                    points: pts,
                    min,
                    max,
                };
            }
        }

        DashboardRefresh {
            state,
            samples: samples_count,
            session_dir,
            warnings,
            data_start_ns,
            data_end_ns,
            resident_bytes: resident,
            io_bytes: io,
            cpu_core_pct: cpu,
        }
    }

    /// Synchronous refresh (tests and immediate callers).
    pub fn refresh(&mut self, client: &dyn DaemonClient) {
        let data =
            Self::compute_dashboard_refresh(client, self.id, self.effective_view(), self.zoomed);
        self.apply_data(data);
    }

    /// Apply a computed refresh payload to the model fields.
    pub fn apply_data(&mut self, data: DashboardRefresh) {
        self.state = data.state;
        self.samples = data.samples;
        self.session_dir = data.session_dir;
        self.warnings = data.warnings;
        self.data_start_ns = data.data_start_ns;
        self.data_end_ns = data.data_end_ns;
        self.resident_bytes = data.resident_bytes;
        self.io_bytes = data.io_bytes;
        self.cpu_core_pct = data.cpu_core_pct;
        self.last_refresh = Some(Instant::now());
    }

    /// Apply a completed background refresh (non-blocking).
    pub fn apply_refresh(&mut self) {
        if let Some(r) = self.refresh_slot.try_take() {
            self.apply_data(r);
        }
    }

    /// Request a background refresh when the cadence says so. Never
    /// blocks the UI thread; results arrive via [`Self::apply_refresh`].
    pub fn request_refresh_if_due(
        &mut self,
        client: std::sync::Arc<dyn DaemonClient + Send + Sync>,
        now: Instant,
    ) {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= self.refresh_interval);
        if due {
            // Bump the cadence stamp only when a worker actually starts
            // (in-flight requests retry next frame, see HomeModel).
            let window = self.effective_view();
            let zoomed = self.zoomed;
            let id = self.id;
            if self
                .refresh_slot
                .spawn(move || Self::compute_dashboard_refresh(client.as_ref(), id, window, zoomed))
            {
                self.last_refresh = Some(now);
            }
        }
    }

    /// Request a refresh immediately (user action — zoom/pan, open),
    /// ignoring the cadence. Still bounded by the in-flight guard; never
    /// blocks the UI thread.
    pub fn request_refresh_now(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        let window = self.effective_view();
        let zoomed = self.zoomed;
        let id = self.id;
        if self
            .refresh_slot
            .spawn(move || Self::compute_dashboard_refresh(client.as_ref(), id, window, zoomed))
        {
            self.last_refresh = Some(Instant::now());
        }
    }

    pub fn effective_view(&self) -> (u64, u64) {
        let start = self
            .view_start_ns
            .unwrap_or(self.data_start_ns.unwrap_or(0));
        let end = self
            .view_end_ns
            .unwrap_or(self.data_end_ns.unwrap_or(start + 1));
        (start, end.max(start + 1))
    }

    /// Zoom around the viewport center by `factor` (>1 zooms in), clamped
    /// to the data range and a minimum span of 1 ms.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn zoom_by(&mut self, factor: f64) {
        let (d_start, d_end) = match (self.data_start_ns, self.data_end_ns) {
            (Some(a), Some(b)) => (a, b),
            _ => return, // no data yet; nothing to zoom into
        };
        let (vs, ve) = self.effective_view();
        let center = (vs as f64 + ve as f64) / 2.0;
        let span = ((ve - vs) as f64 / factor).clamp(1_000_000.0, (d_end - d_start) as f64);
        // Round the span to whole nanoseconds so start/end stay consistent
        // with the span the tests (and the view label) see.
        let span = span.round();
        let mut start = (center - span / 2.0).max(d_start as f64);
        let mut end = start + span;
        if end > d_end as f64 {
            end = d_end as f64;
            start = (end - span).max(d_start as f64);
        }
        self.view_start_ns = Some(start as u64);
        self.view_end_ns = Some(end as u64);
        self.zoomed = self.view_start_ns.is_some();
    }

    /// Pan the viewport by `fraction` of its span (positive = right),
    /// clamped to the data range.
    #[allow(clippy::arithmetic_side_effects)]
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn pan_by(&mut self, fraction: f64) {
        let (d_start, d_end) = match (self.data_start_ns, self.data_end_ns) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };
        let (vs, ve) = self.effective_view();
        let span = ve - vs;
        let shift = (span as f64 * fraction) as i128;
        let new_start = (i128::from(vs) + shift).clamp(
            i128::from(d_start),
            (i128::from(d_end) - i128::from(span)).max(i128::from(d_start)),
        );
        self.view_start_ns = Some(u64::try_from(new_start).unwrap_or(d_start));
        self.view_end_ns = Some(u64::try_from(new_start + i128::from(span)).unwrap_or(d_end));
        self.zoomed = true;
    }

    /// Restore the full-range view.
    pub fn reset_zoom(&mut self) {
        self.view_start_ns = None;
        self.view_end_ns = None;
        self.zoomed = false;
    }

    pub fn refresh_if_due(&mut self, client: &dyn DaemonClient, now: Instant) {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= self.refresh_interval);
        if due {
            self.refresh(client);
        }
    }

    /// Stop the recording (never the target). Idempotent in the daemon; the
    /// model records the outcome for display.
    pub fn stop(&mut self, client: &dyn DaemonClient) {
        match client.stop(self.id) {
            Ok(state) => {
                self.stop_status = Some(state);
            }
            Err(e) => {
                self.stop_status = Some(format!("stop failed: {e}"));
            }
        }
    }

    /// Stop OFF the UI thread; the status lands via [`Self::apply_stop`].
    pub fn stop_async(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        let id = self.id;
        self.stop_status = Some("stopping…".into());
        self.stop_slot.spawn(move || match client.stop(id) {
            Ok(state) => state,
            Err(e) => format!("stop failed: {e}"),
        });
    }

    /// Apply a finished stop (non-blocking).
    pub fn apply_stop(&mut self) {
        if let Some(status) = self.stop_slot.try_take() {
            self.stop_status = Some(status);
        }
    }
}

/// PF-707: diagnostics drawer view model — capabilities + storage health,
/// refreshed on demand (slow cadence; diagnostics are not hot data).
#[derive(Default)]
pub struct DiagnosticsModel {
    pub health: Option<StorageHealth>,
    pub last_refresh: Option<Instant>,
    pub error: Option<String>,
    pub refresh_slot: AsyncSlot<DiagnosticsRefresh>,
}

/// Payload computed off-thread for the diagnostics drawer.
pub struct DiagnosticsRefresh {
    pub health: Option<StorageHealth>,
    pub error: Option<String>,
}

impl DiagnosticsModel {
    /// Compute the diagnostics payload (runs on a worker thread).
    pub fn compute_diagnostics_refresh(client: &dyn DaemonClient) -> DiagnosticsRefresh {
        // Capabilities probe keeps the liveness touch; its error is advisory
        // because the storage-health outcome always dominates the line.
        let _ = client.capabilities();
        // Storage health is the load-bearing half for the drawer.
        let (health, error) = match client.storage_health() {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(format!("storage: {e}"))),
        };
        DiagnosticsRefresh { health, error }
    }

    /// Synchronous refresh (tests and immediate callers).
    pub fn refresh(&mut self, client: &dyn DaemonClient) {
        let data = Self::compute_diagnostics_refresh(client);
        self.apply_data(data);
    }

    /// Apply a computed refresh payload to the model fields.
    pub fn apply_data(&mut self, data: DiagnosticsRefresh) {
        self.health = data.health;
        self.error = data.error;
        self.last_refresh = Some(Instant::now());
    }

    /// Apply a completed background refresh (non-blocking).
    pub fn apply_refresh(&mut self) {
        if let Some(r) = self.refresh_slot.try_take() {
            self.apply_data(r);
        }
    }

    /// Request a background refresh when the 2 s drawer cadence says so.
    /// Never blocks the UI thread; results arrive via [`Self::apply_refresh`].
    pub fn request_refresh_if_due(
        &mut self,
        client: std::sync::Arc<dyn DaemonClient + Send + Sync>,
        now: Instant,
    ) {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(2));
        if due {
            // Bump the cadence stamp only when a worker actually starts
            // (in-flight requests retry next frame, see HomeModel).
            if self
                .refresh_slot
                .spawn(move || Self::compute_diagnostics_refresh(client.as_ref()))
            {
                self.last_refresh = Some(now);
            }
        }
    }

    /// Request a refresh immediately (drawer opened), ignoring the 2 s
    /// cadence. Still bounded by the in-flight guard; never blocks.
    pub fn request_refresh_now(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        if self
            .refresh_slot
            .spawn(move || Self::compute_diagnostics_refresh(client.as_ref()))
        {
            self.last_refresh = Some(Instant::now());
        }
    }

    /// Refresh when at least 2 s elapsed (drawer cadence).
    pub fn refresh_if_due_manual(&mut self, client: &dyn DaemonClient) {
        let due = self
            .last_refresh
            .is_none_or(|t| now_duration(t) >= std::time::Duration::from_secs(2));
        if due {
            self.refresh(client);
        }
    }

    /// Human-facing one-line status (color-independent: text says it all).
    pub fn summary_line(&self) -> String {
        match &self.health {
            Some(h) => format!(
                "storage {} | counters {} | stacks {}",
                if h.writable { "OK" } else { "NOT WRITABLE" },
                if h.counters_available {
                    "available".to_string()
                } else {
                    format!("unavailable ({})", h.counters_reason)
                },
                if h.stacks_available {
                    "available".to_string()
                } else {
                    format!("unavailable ({})", h.stacks_reason)
                },
            ),
            None => self
                .error
                .clone()
                .unwrap_or_else(|| "diagnostics not loaded".into()),
        }
    }
}

/// PF-1106: compare view model — pick two sessions, run the comparison
/// through the daemon (shared engine), render a text table.
#[derive(Default)]
pub struct CompareModel {
    pub baseline: Option<SessionId>,
    pub candidate: Option<SessionId>,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub ran: bool,
    /// Comparison slot: the daemon round-trip runs OFF the UI thread.
    pub result_slot: AsyncSlot<Result<JsonValue, String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompareMetricRow {
    pub name: String,
    pub baseline: String,
    pub candidate: String,
    pub delta: String,
    pub pct_change: Option<f64>,
}

impl CompareModel {
    /// Detailed metric table with percentage shifts.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn metrics_detailed(&self) -> Vec<CompareMetricRow> {
        let Some(v) = &self.result else {
            return Vec::new();
        };
        let Some(metrics) = v.get("metrics").and_then(|m| m.as_object_pairs()) else {
            return Vec::new();
        };
        metrics
            .iter()
            .map(|(name, m)| {
                let fmt = |v: &JsonValue| {
                    v.as_f64()
                        .map(|x| format!("{x:.0}"))
                        .unwrap_or_else(|| "unavailable".into())
                };
                let base_val = m.get("baseline").and_then(|v| v.as_f64());
                let delta_val = m.get("delta").and_then(|v| v.as_f64());
                let pct = match (base_val, delta_val) {
                    (Some(b), Some(d)) if b.abs() > 0.0001 => Some((d / b) * 100.0),
                    _ => None,
                };
                CompareMetricRow {
                    name: name.clone(),
                    baseline: m.get("baseline").map(fmt).unwrap_or_default(),
                    candidate: m.get("candidate").map(fmt).unwrap_or_default(),
                    delta: m.get("delta").map(fmt).unwrap_or_else(|| "—".into()),
                    pct_change: pct,
                }
            })
            .collect()
    }
    /// Swap baseline and candidate sessions.
    pub fn swap(&mut self) {
        std::mem::swap(&mut self.baseline, &mut self.candidate);
        self.result = None;
        self.error = None;
        self.ran = false;
    }

    pub fn run(&mut self, client: &dyn DaemonClient) {
        self.ran = true;
        let (Some(a), Some(b)) = (self.baseline, self.candidate) else {
            self.error = Some("pick both a baseline and a candidate session".into());
            return;
        };
        if a == b {
            self.error = Some("baseline and candidate are the same session".into());
            return;
        }
        match client.compare(a, b) {
            Ok(v) => {
                self.result = Some(v);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.result = None;
            }
        }
    }

    /// Compare OFF the UI thread; the table lands via [`Self::apply_result`].
    /// Validation is synchronous (instant, no IPC); a dispatch clears the
    /// stale table so a re-run never shows old numbers as new.
    pub fn run_async(&mut self, client: std::sync::Arc<dyn DaemonClient + Send + Sync>) {
        self.ran = true;
        let (Some(a), Some(b)) = (self.baseline, self.candidate) else {
            self.error = Some("pick both a baseline and a candidate session".into());
            return;
        };
        if a == b {
            self.error = Some("baseline and candidate are the same session".into());
            return;
        }
        self.result = None;
        self.error = None;
        self.result_slot
            .spawn(move || client.compare(a, b).map_err(|e| e.to_string()));
    }

    /// Apply a finished comparison (non-blocking).
    pub fn apply_result(&mut self) {
        if let Some(r) = self.result_slot.try_take() {
            match r {
                Ok(v) => {
                    self.result = Some(v);
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(e);
                    self.result = None;
                }
            }
        }
    }

    /// Human-readable metric table extracted from the comparison JSON.
    pub fn metrics_text(&self) -> Vec<(String, String, String, String)> {
        let Some(v) = &self.result else {
            return Vec::new();
        };
        let Some(metrics) = v.get("metrics").and_then(|m| m.as_object_pairs()) else {
            return Vec::new();
        };
        metrics
            .iter()
            .map(|(name, m)| {
                let fmt = |v: &JsonValue| {
                    v.as_f64()
                        .map(|x| format!("{x:.0}"))
                        .unwrap_or_else(|| "unavailable".into())
                };
                (
                    name.clone(),
                    m.get("baseline").map(fmt).unwrap_or_default(),
                    m.get("candidate").map(fmt).unwrap_or_default(),
                    m.get("delta").map(fmt).unwrap_or_else(|| "—".into()),
                )
            })
            .collect()
    }
}

fn now_duration(t: Instant) -> Duration {
    Instant::now().saturating_duration_since(t)
}

/// A scripted daemon for tests: returns queued results per call.
pub struct FakeDaemon {
    pub sessions: Vec<SessionRow>,
    pub process_rows: Vec<ProcessRow>,
    pub storage_health: StorageHealth,
    pub attach_result: Result<SessionId, PfError>,
    pub status_result: Result<SessionSnapshot, PfError>,
    pub samples_result: Vec<pf_types::CounterSnapshot>,
    pub stop_result: Result<String, PfError>,
    pub compare_result: Result<JsonValue, PfError>,
    pub attach_calls: std::sync::Mutex<Vec<(u32, u64, u64, String)>>,
    pub stop_calls: std::sync::Mutex<Vec<SessionId>>,
    pub delete_calls: std::sync::Mutex<Vec<SessionId>>,
    pub delete_result: Result<u64, PfError>,
}

impl Default for FakeDaemon {
    fn default() -> Self {
        FakeDaemon {
            sessions: Vec::new(),
            process_rows: Vec::new(),
            delete_calls: std::sync::Mutex::new(Vec::new()),
            delete_result: Ok(1024),
            compare_result: Err(PfError::new(
                ErrorCode::InvalidArgument,
                "no comparison queued in the fake",
            )),
            storage_health: StorageHealth {
                data_root: "C:/fake-data".into(),
                writable: true,
                session_count: 0,
                total_bytes: 0,
                counters_available: true,
                counters_reason: String::new(),
                stacks_available: false,
                stacks_reason: "NOT_IMPLEMENTED".into(),
                os_version: "fake-os".into(),
                logical_cpus: Some(8),
            },
            attach_result: Ok(SessionId::now_v7()),
            status_result: Ok(SessionSnapshot {
                id: SessionId::now_v7(),
                state: "RECORDING".into(),
                samples: 0,
                session_dir: None,
                warnings: Vec::new(),
            }),
            samples_result: Vec::new(),
            stop_result: Ok("COMPLETED".into()),
            attach_calls: std::sync::Mutex::new(Vec::new()),
            stop_calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl DaemonClient for FakeDaemon {
    fn capabilities(&self) -> Result<JsonValue, PfError> {
        Ok(JsonValue::Object(vec![]))
    }
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        Ok(self.sessions.clone())
    }
    fn list_processes(&self, limit: u32) -> Result<Vec<ProcessRow>, PfError> {
        Ok(self
            .process_rows
            .iter()
            .take(limit as usize)
            .cloned()
            .collect())
    }
    fn start_attach(
        &self,
        pid: u32,
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError> {
        self.attach_calls
            .lock()
            .map_err(|_| PfError::new(ErrorCode::Internal, "fake daemon poisoned"))?
            .push((pid, duration_ms, interval_ms, label.to_string()));
        self.attach_result.clone()
    }
    fn start_run(
        &self,
        exe: &str,
        args: &[String],
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError> {
        self.attach_calls.lock().expect("attach calls lock").push((
            0,
            duration_ms,
            interval_ms,
            format!("{exe} {args:?} {label}"),
        ));
        Ok(SessionId::now_v7())
    }
    fn status(&self, _id: SessionId) -> Result<SessionSnapshot, PfError> {
        self.status_result.clone()
    }
    fn stop(&self, id: SessionId) -> Result<String, PfError> {
        if let Ok(mut c) = self.stop_calls.lock() {
            c.push(id);
        }
        self.stop_result.clone()
    }
    fn delete_session(&self, id: SessionId) -> Result<u64, PfError> {
        if let Ok(mut c) = self.delete_calls.lock() {
            c.push(id);
        }
        self.delete_result.clone()
    }
    fn samples(&self, _id: SessionId) -> Result<Vec<pf_types::CounterSnapshot>, PfError> {
        Ok(self.samples_result.clone())
    }
    fn envelope(
        &self,
        _id: SessionId,
        field: pf_storage::EnvelopeField,
        from_ns: u64,
        to_ns: u64,
        buckets: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        Ok(pf_storage::envelope_of_samples(
            &self.samples_result,
            from_ns,
            to_ns,
            buckets,
            field,
        ))
    }
    fn storage_health(&self) -> Result<StorageHealth, PfError> {
        Ok(self.storage_health.clone())
    }
    fn compare(&self, _baseline: SessionId, _candidate: SessionId) -> Result<JsonValue, PfError> {
        self.compare_result.clone()
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn snap(mono: u64, cpu: u64, ws: u64, io: u64) -> pf_types::CounterSnapshot {
        pf_types::CounterSnapshot {
            mono_ns: mono,
            cpu_user_ns: Some(cpu),
            cpu_kernel_ns: Some(0),
            resident_bytes: Some(ws),
            io_read_bytes: Some(io),
            io_write_bytes: Some(0),
            ..pf_types::CounterSnapshot::default()
        }
    }

    #[test]
    fn attach_form_validates_and_reports() {
        let mut m = HomeModel::default();
        assert!(m.validate_attach_form().is_err()); // empty pid
        m.attach_pid = "abc".into();
        assert!(m.validate_attach_form().is_err());
        m.attach_pid = "4180".into();
        m.attach_duration_ms = "0".into();
        assert!(m.validate_attach_form().is_err());
        m.attach_duration_ms = "15000".into();
        m.attach_interval_ms = "50".into();
        assert!(m.validate_attach_form().is_err());
        m.attach_interval_ms = "250".into();
        assert_eq!(m.validate_attach_form().unwrap(), (4180, 15000, 250));
    }

    #[test]
    fn attach_flow_calls_daemon_and_records_outcome() {
        let mut m = HomeModel::default();
        m.attach_pid = "4180".into();
        let fake = FakeDaemon::default();
        let opened = m.submit_attach(&fake);
        assert!(opened.is_some());
        assert!(matches!(m.last_outcome, ActionOutcome::Started(_)));
        let calls = fake.attach_calls.lock().unwrap();
        assert_eq!(calls[0], (4180, 10000, 250, String::new()));
        // Failure path surfaces typed message.
        let failing = FakeDaemon {
            attach_result: Err(PfError::new(ErrorCode::PermissionDenied, "denied")),
            ..FakeDaemon::default()
        };
        m.attach_pid = "4".into();
        assert!(m.submit_attach(&failing).is_none());
        assert!(
            matches!(m.last_outcome, ActionOutcome::Failed(ref t) if t.contains("PERMISSION_DENIED"))
        );
    }

    #[test]
    fn session_filter_matches_id_image_state() {
        let mut m = HomeModel::default();
        m.sessions = vec![
            SessionRow {
                id: SessionId::now_v7(),
                state: "COMPLETED".into(),
                samples: 10,
                image: "app.exe".into(),
                stop_reason: None,
            },
            SessionRow {
                id: SessionId::now_v7(),
                state: "RECORDING".into(),
                samples: 3,
                image: "other.bin".into(),
                stop_reason: None,
            },
        ];
        assert_eq!(m.filter_rows("").len(), 2);
        assert_eq!(m.filter_rows("recording").len(), 1);
        assert_eq!(m.filter_rows("APP").len(), 1);
        assert_eq!(m.filter_rows("zzz").len(), 0);
    }

    #[test]
    fn dashboard_builds_series_with_report_semantics() {
        let id = SessionId::now_v7();
        let mut fake = FakeDaemon::default();
        fake.samples_result = vec![
            snap(0, 0, 4096, 0),
            snap(250_000_000, 250_000_000, 8192, 1000),
            snap(500_000_000, 500_000_000, 4096, 2000),
        ];
        let mut d = DashboardModel::new(id);
        d.refresh(&fake);
        // CPU rate: 100% across both intervals.
        assert_eq!(d.cpu_core_pct.points.len(), 2); // first sample unavailable
        assert!((d.cpu_core_pct.max.unwrap() - 100.0).abs() < 1e-6);
        // Resident series is a gauge: 3 points, peak 8192.
        assert_eq!(d.resident_bytes.points.len(), 3);
        assert_eq!(d.resident_bytes.max, Some(8192.0));
        // IO cumulative.
        assert_eq!(d.io_bytes.points.len(), 3);
        assert_eq!(d.io_bytes.max, Some(2000.0));
    }

    #[test]
    fn stop_records_state_and_survives_errors() {
        let id = SessionId::now_v7();
        let mut d = DashboardModel::new(id);
        let fake = FakeDaemon::default();
        d.stop(&fake);
        assert_eq!(d.stop_status.as_deref(), Some("COMPLETED"));
        let failing = FakeDaemon {
            stop_result: Err(PfError::new(ErrorCode::SessionNotFound, "gone")),
            ..FakeDaemon::default()
        };
        d.stop(&failing);
        assert!(
            d.stop_status
                .as_deref()
                .unwrap()
                .contains("SESSION_NOT_FOUND")
        );
    }

    #[test]
    fn refresh_cadence_throttles_daemon_calls() {
        let id = SessionId::now_v7();
        let mut d = DashboardModel::new(id);
        let fake = FakeDaemon::default();
        let t0 = Instant::now();
        d.refresh_if_due(&fake, t0); // first: due
        d.refresh_if_due(&fake, t0 + Duration::from_millis(100)); // not due
        d.refresh_if_due(&fake, t0 + Duration::from_millis(800)); // due
        assert!(d.last_refresh.is_some());
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod compare_tests {
    use super::*;
    use pf_types::SessionId;

    fn cmp_json(comparable: bool) -> JsonValue {
        JsonValue::Object(vec![
            ("comparable".into(), JsonValue::Bool(comparable)),
            (
                "metrics".into(),
                JsonValue::Object(vec![(
                    "wall_duration".into(),
                    JsonValue::Object(vec![
                        ("baseline".into(), JsonValue::Float(1.0)),
                        ("candidate".into(), JsonValue::Float(2.0)),
                        ("delta".into(), JsonValue::Float(1.0)),
                    ]),
                )]),
            ),
            ("caveats".into(), JsonValue::Array(vec![])),
        ])
    }

    #[test]
    fn compare_requires_both_sides_and_distinct() {
        let mut fake = FakeDaemon::default();
        fake.compare_result = Ok(cmp_json(true));
        let mut m = CompareModel::default();
        m.run(&fake);
        assert!(m.error.as_deref().unwrap().contains("pick both"));
        m.baseline = Some(SessionId::now_v7());
        m.candidate = m.baseline;
        m.run(&fake);
        assert!(m.error.as_deref().unwrap().contains("same session"));
    }

    #[test]
    fn compare_surfaces_result_and_metrics() {
        let mut fake = FakeDaemon::default();
        fake.compare_result = Ok(cmp_json(true));
        let mut m = CompareModel::default();
        m.baseline = Some(SessionId::now_v7());
        m.candidate = Some(SessionId::now_v7());
        m.run(&fake);
        assert!(m.error.is_none());
        assert!(m.ran);
        let rows = m.metrics_detailed();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "wall_duration");
        assert_eq!(rows[0].delta, "1"); // delta
        assert_eq!(rows[0].baseline, "1");
        assert_eq!(rows[0].candidate, "2");
    }

    #[test]
    fn compare_error_path_clears_result() {
        let mut fake = FakeDaemon::default();
        fake.compare_result = Err(PfError::new(ErrorCode::SessionNotFound, "session gone"));
        let mut m = CompareModel::default();
        m.baseline = Some(SessionId::now_v7());
        m.candidate = Some(SessionId::now_v7());
        m.run(&fake);
        assert!(m.result.is_none());
        assert!(m.error.as_deref().unwrap().contains("SESSION_NOT_FOUND"));
    }

    #[test]
    fn non_comparable_flag_visible_in_result() {
        let mut fake = FakeDaemon::default();
        fake.compare_result = Ok(cmp_json(false));
        let mut m = CompareModel::default();
        m.baseline = Some(SessionId::now_v7());
        m.candidate = Some(SessionId::now_v7());
        m.run(&fake);
        let v = m.result.as_ref().unwrap();
        assert_eq!(v.get("comparable").and_then(|c| c.as_bool()), Some(false));
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod live_processes_tests {
    use super::*;
    use pf_types::SessionId;

    #[test]
    fn process_sorting_covers_all_orders() {
        let mut procs = vec![
            ProcessRow {
                pid: 300,
                parent_pid: None,
                image: "zebra.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 100,
                parent_pid: None,
                image: "alpha.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 200,
                parent_pid: None,
                image: "beta.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];

        HomeModel::sort_processes(&mut procs, ProcessSort::PidAsc);
        assert_eq!(procs[0].pid, 100);
        assert_eq!(procs[1].pid, 200);
        assert_eq!(procs[2].pid, 300);

        HomeModel::sort_processes(&mut procs, ProcessSort::PidDesc);
        assert_eq!(procs[0].pid, 300);
        assert_eq!(procs[2].pid, 100);

        HomeModel::sort_processes(&mut procs, ProcessSort::NameAsc);
        assert_eq!(procs[0].image, "alpha.exe");
        assert_eq!(procs[2].image, "zebra.exe");

        HomeModel::sort_processes(&mut procs, ProcessSort::NameDesc);
        assert_eq!(procs[0].image, "zebra.exe");
        assert_eq!(procs[2].image, "alpha.exe");
    }

    #[test]
    fn cpu_sort_puts_hot_processes_first_and_absent_last() {
        let mut procs = vec![
            ProcessRow {
                pid: 1,
                parent_pid: None,
                image: "idle.exe".into(),
                window_title: None,
                cpu_pct: Some(0.5),
                rss_bytes: None,
            },
            ProcessRow {
                pid: 2,
                parent_pid: None,
                image: "hot.exe".into(),
                window_title: None,
                cpu_pct: Some(97.3),
                rss_bytes: None,
            },
            ProcessRow {
                pid: 3,
                parent_pid: None,
                image: "warm.exe".into(),
                window_title: None,
                cpu_pct: Some(12.0),
                rss_bytes: None,
            },
            // No baseline yet / access denied — must not masquerade as idle.
            ProcessRow {
                pid: 4,
                parent_pid: None,
                image: "new.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];

        HomeModel::sort_processes(&mut procs, ProcessSort::CpuDesc);
        assert_eq!(procs[0].image, "hot.exe");
        assert_eq!(procs[1].image, "warm.exe");
        assert_eq!(procs[2].image, "idle.exe");
        assert_eq!(procs[3].image, "new.exe", "absent delta sorts last");
    }

    #[test]
    fn process_sort_cycle_covers_cpu_first() {
        let mut s = ProcessSort::default();
        assert_eq!(s, ProcessSort::CpuDesc, "hot-first is the default view");
        let mut order = vec![s];
        for _ in 0..5 {
            order.push(s.cycle());
        }
        assert_eq!(order.len(), 6);
        assert_eq!(order[5], ProcessSort::CpuDesc, "cycle returns to start");
    }

    #[test]
    fn process_window_title_filtering_and_sorting() {
        let mut m = HomeModel::default();
        m.processes = vec![
            ProcessRow {
                pid: 14196,
                parent_pid: None,
                image: "chrome.exe".into(),
                window_title: Some("PulseForge PR #42 - Google Chrome".into()),
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 5644,
                parent_pid: None,
                image: "code.exe".into(),
                window_title: Some("Libery - Visual Studio Code".into()),
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 1024,
                parent_pid: None,
                image: "cargo.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];

        // Search by window title substring ("Visual Studio")
        let res = m.filter_processes("Visual Studio");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].pid, 5644);
        assert_eq!(res[0].display_title(), "Libery - Visual Studio Code");

        // Search by exe name ("chrome")
        let res = m.filter_processes("chrome");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].pid, 14196);

        // Search by PID ("1024")
        let res = m.filter_processes("1024");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].pid, 1024);
        assert_eq!(res[0].display_title(), "cargo.exe");

        // Sort by Name (Ascending): "cargo.exe" < "Libery - Visual Studio Code" < "PulseForge PR #42 - Google Chrome"
        HomeModel::sort_processes(&mut m.processes, ProcessSort::NameAsc);
        assert_eq!(m.processes[0].pid, 1024);
        assert_eq!(m.processes[1].pid, 5644);
        assert_eq!(m.processes[2].pid, 14196);

        // Pick process suggests window title as label
        let picked = m.processes[1].clone();
        m.pick_process(&picked);
        assert_eq!(m.attach_pid, "5644");
        assert_eq!(m.attach_label, "Libery - Visual Studio Code");
    }

    #[test]
    fn process_tree_builds_parent_child_hierarchy() {
        let mut m = HomeModel::default();
        m.processes = vec![
            ProcessRow {
                pid: 10,
                parent_pid: None,
                image: "cargo".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 20,
                parent_pid: Some(10),
                image: "rustc".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 30,
                parent_pid: Some(20),
                image: "link.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 40,
                parent_pid: None,
                image: "server".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];

        let nodes = m.visible_tree_nodes("");
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0].process.pid, 10);
        assert_eq!(nodes[0].depth, 0);
        assert!(nodes[0].has_children);

        assert_eq!(nodes[1].process.pid, 20);
        assert_eq!(nodes[1].depth, 1);
        assert!(nodes[1].has_children);

        assert_eq!(nodes[2].process.pid, 30);
        assert_eq!(nodes[2].depth, 2);
        assert!(!nodes[2].has_children);

        assert_eq!(nodes[3].process.pid, 40);
        assert_eq!(nodes[3].depth, 0);
        assert!(!nodes[3].has_children);

        // Collapse node 10
        m.toggle_process_expanded(10);
        let nodes = m.visible_tree_nodes("");
        // Only 10 and 40 visible now because children of 10 are collapsed
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].process.pid, 10);
        assert_eq!(nodes[1].process.pid, 40);

        // Expand all brings children back
        m.expand_all_processes();
        assert_eq!(m.visible_tree_nodes("").len(), 4);
    }

    #[test]
    fn process_tree_search_auto_expands_matching_descendants() {
        let mut m = HomeModel::default();
        m.processes = vec![
            ProcessRow {
                pid: 10,
                parent_pid: None,
                image: "cargo".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 20,
                parent_pid: Some(10),
                image: "rustc".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 30,
                parent_pid: Some(20),
                image: "link.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 40,
                parent_pid: None,
                image: "server".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];
        // Even if collapsed:
        m.collapse_all_processes();

        // Search for "link" matches child 30, and keeps ancestors 10 -> 20 visible
        let nodes = m.visible_tree_nodes("link");
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].process.pid, 10);
        assert_eq!(nodes[1].process.pid, 20);
        assert_eq!(nodes[2].process.pid, 30);
    }

    #[test]
    fn active_recording_detection_links_session_and_process() {
        let mut m = HomeModel::default();
        let sid = SessionId::now_v7();
        m.sessions = vec![SessionRow {
            id: sid,
            state: "RECORDING".into(),
            samples: 450,
            image: "build-server".into(),
            stop_reason: None,
        }];
        m.processes = vec![
            ProcessRow {
                pid: 818,
                parent_pid: None,
                image: "build-server.exe".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
            ProcessRow {
                pid: 4242,
                parent_pid: None,
                image: "spec-workload".into(),
                window_title: None,
                cpu_pct: None,
                rss_bytes: None,
            },
        ];

        let rec_session = m.is_process_recording(&m.processes[0]);
        assert!(rec_session.is_some());
        assert_eq!(rec_session.unwrap().id, sid);
        assert_eq!(m.recording_process_count(), 1);

        assert!(m.is_process_recording(&m.processes[1]).is_none());
    }
}
