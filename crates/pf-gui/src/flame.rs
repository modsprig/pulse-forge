//! CPU-stacks flamegraph panel (PF-906 bounded slice): an icicle view over
//! the PF-904 aggregation engine's bounded call tree.
//!
//! Bounds: the tree is capped (nodes/depth, omissions counted), the painted
//! rect budget is capped per frame, and the search string is capped — the
//! widget can never grow unbounded with data. Zoom re-roots the view at a
//! clicked node (one owned bounded subtree); no time filtering or
//! thread-filtering yet (honestly absent, not faked).

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use pf_analysis::stacks::FlameNode;

/// View mode for the CPU stacks panel: visual icicle flamegraph or tabular hotspots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StacksViewMode {
    #[default]
    Flamegraph,
    Hotspots,
}

/// One hotspot row summarizing exclusive CPU time of a single function.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HotspotRow {
    pub function: String,
    pub module: String,
    pub samples: u64,
    pub percent: f64,
}

/// Rect paint budget per frame (matches the analysis tree cap).
const MAX_RECTS: usize = 2_000;
const ROW_H: f32 = 20.0;
const MAX_ROWS: usize = 40;
const MAX_SEARCH: usize = 128;
/// Threads shown in the filter dropdown (bounded; the rest are counted).
const MAX_TIDS: usize = 64;
/// Curated 12-color high-legibility profiler palette (hash of frame key picks a color).
const PALETTE: [Color32; 12] = [
    Color32::from_rgb(45, 114, 210), // Deep Azure
    Color32::from_rgb(28, 144, 153), // Teal Cyan
    Color32::from_rgb(43, 147, 104), // Emerald
    Color32::from_rgb(198, 125, 38), // Amber Ochre
    Color32::from_rgb(196, 81, 68),  // Coral Terracotta
    Color32::from_rgb(106, 90, 185), // Indigo Violet
    Color32::from_rgb(56, 132, 188), // Cerulean Blue
    Color32::from_rgb(178, 138, 48), // Warm Gold
    Color32::from_rgb(175, 72, 108), // Crimson Rose
    Color32::from_rgb(48, 138, 128), // Ocean Green
    Color32::from_rgb(68, 102, 196), // Royal Cobalt
    Color32::from_rgb(176, 98, 64),  // Burnished Copper
];

/// Aggregated stacks for one session, plus the flame view state.
#[derive(Default)]
pub struct StacksModel {
    /// Path the stacks were loaded from (reload guard).
    pub loaded_from: Option<std::path::PathBuf>,
    pub available: bool,
    pub error: Option<String>,
    pub samples: u64,
    pub total_weight: u64,
    pub coverage: Option<f64>,
    pub frames_resolved: Option<u64>,
    pub frames_total: Option<u64>,
    /// Bounded tree from the aggregation engine (current window).
    pub tree: pf_analysis::stacks::FlameTree,
    /// All loaded samples (bounded by the recorder's persisted cap) with
    /// their perf timestamps — the source for time-window re-aggregation.
    pub all: Vec<pf_analysis::stacks_io::TimedStack>,
    /// Data time range [min, max] in ns, when samples carry timestamps.
    pub t_range: Option<(u64, u64)>,
    /// Selected window relative to `t_range.0`, in milliseconds.
    pub window_ms: (u64, u64),
    /// The window actually aggregated (clamped absolute ns); re-aggregation
    /// fires only when this differs from the UI window.
    applied_window: Option<(u64, u64)>,
    /// Selected thread (`None` = all threads), with the tid actually
    /// aggregated (guard against redundant re-aggregation).
    pub tid_filter: Option<u64>,
    applied_tid: Option<u64>,
    applied_bottom_up: bool,
    /// Threads by sample count, descending, bounded (top [`MAX_TIDS`]).
    pub tids: Vec<(u64, u64)>,
    /// Threads hidden by the [`MAX_TIDS`] bound (honest count).
    pub tids_hidden: u64,
    /// Samples outside the current window/thread selection (honest count).
    pub filtered_out: u64,
    /// Time filtering is active (window narrower than the data range).
    pub time_filtered: bool,
    /// PF-907: bottom-up (callee-first) aggregation is active.
    pub bottom_up: bool,
    /// Thread filtering is active (a single tid selected).
    pub thread_filtered: bool,
    pub search: String,
    pub view_mode: StacksViewMode,
    pub hotspots: Vec<HotspotRow>,
    pub hotspot_search: String,
    /// Currently hovered frame in the icicle canvas: (symbol, inclusive, exclusive, pct).
    pub hovered_frame: Option<(String, u64, u64, f64)>,
    /// Zoom root: an owned bounded subtree (clone of ≤ MAX_RECTS nodes).
    pub zoom: Option<FlameNode>,
    /// Diagnostics for tests: rects painted in the last frame.
    pub last_painted_rects: usize,
    /// Frames hidden by the per-frame rect budget in the last paint.
    pub last_painted_omitted: usize,
}

// Model arithmetic is bounded by the caps above.
#[allow(clippy::arithmetic_side_effects)]
impl StacksModel {
    /// Load (or reload) session stacks from the session dir. Errors are
    /// kept as honest unavailability — never zeros.
    pub fn maybe_reload(&mut self, dir: Option<&std::path::Path>) {
        let Some(dir) = dir else { return };
        if self.loaded_from.as_deref().is_some_and(|p| p == dir) {
            return;
        }
        *self = StacksModel {
            loaded_from: Some(dir.to_path_buf()),
            ..StacksModel::default()
        };
        match pf_analysis::stacks_io::load_session_stacks_timed(dir) {
            Ok(timed) => {
                self.all = timed;
                let t_min = self.all.iter().map(|t| t.time_ns).min();
                let t_max = self.all.iter().map(|t| t.time_ns).max();
                self.t_range = t_min.zip(t_max);
                if let Some((lo, hi)) = self.t_range {
                    self.window_ms = (0, (hi.saturating_sub(lo)) / 1_000_000);
                }
                // Threads by sample count, descending; bounded display list.
                let mut counts: Vec<(u64, u64)> = {
                    let mut m = std::collections::HashMap::new();
                    for t in &self.all {
                        *m.entry(t.tid).or_insert(0) += 1;
                    }
                    m.into_iter().collect()
                };
                counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                self.tids_hidden = counts.len().saturating_sub(MAX_TIDS) as u64;
                counts.truncate(MAX_TIDS);
                self.tids = counts;
                let q = pf_analysis::stacks_io::coverage_quality(dir);
                self.coverage = q.frame_identity_coverage;
                self.frames_resolved = q.frames_resolved;
                self.frames_total = q.frames_total;
                self.available = !self.all.is_empty();
                if !self.available {
                    self.error = Some("session carries no aggregatable stack samples".to_string());
                }
                self.applied_window = None;
                self.reaggregate();
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }

    /// Re-aggregate the tree over the selected time window (PF-906). The
    /// full range aggregates everything; a narrower window re-roots the
    /// weights on the samples inside it. Bounded by the same tree caps.
    pub fn reaggregate(&mut self) {
        let window = match self.t_range {
            Some((lo, hi)) => {
                let full_ms = hi.saturating_sub(lo) / 1_000_000;
                let to_ns = |ms: u64| lo.saturating_add(ms.saturating_mul(1_000_000));
                // The full-range end must include the tail samples that the
                // ms truncation would otherwise drop below `hi`.
                let a = if self.window_ms.0 >= full_ms {
                    hi
                } else {
                    to_ns(self.window_ms.0)
                };
                let b = if self.window_ms.1 >= full_ms {
                    hi
                } else {
                    to_ns(self.window_ms.1)
                };
                // Order the ends and clamp to the data range; the UI also
                // keeps them ordered, but the model must be safe alone.
                let start = a.min(b).max(lo);
                let end = a.max(b).min(hi);
                Some((start.min(end), end.max(start)))
            }
            None => None,
        };
        if self.applied_window.is_some()
            && self.applied_window == window
            && self.applied_tid == self.tid_filter
            && self.applied_bottom_up == self.bottom_up
        {
            return; // nothing changed
        }
        // Stream the selected stacks straight into the aggregation — no
        // clone-collect of the selected sub-batch (at recorder stress
        // scale that clone deep-copied every selected stack per pan/zoom).
        // The honest counters (filtered-out, time-filtered) are computed
        // during the same single pass.
        let bottom_up = self.bottom_up;
        let tid_filter = self.tid_filter;
        let mut selected: u64 = 0;
        let mut saw_outside_window = false;
        let mut trie = pf_analysis::stacks::StackTrie::new();
        {
            let iter = self.all.iter().filter(|t| {
                let in_window = window
                    .map(|(start, end)| t.time_ns >= start && t.time_ns <= end)
                    .unwrap_or(true);
                let in_thread = tid_filter.map(|tid| t.tid == tid).unwrap_or(true);
                if !in_window {
                    saw_outside_window = true;
                }
                let keep = in_window && in_thread;
                if keep {
                    selected += 1;
                }
                keep
            });
            if bottom_up {
                trie.aggregate_bottom_up_iter(iter.map(|t| &t.stack));
            } else {
                trie.aggregate_iter(iter.map(|t| &t.stack));
            }
        }
        self.filtered_out = (self.all.len() as u64).saturating_sub(selected);
        self.time_filtered = saw_outside_window;
        self.thread_filtered = tid_filter.is_some();
        self.applied_tid = tid_filter;
        self.applied_bottom_up = bottom_up;
        self.samples = trie.sample_count() as u64;
        self.total_weight = trie.total_weight();
        self.hotspots = trie
            .top_by_exclusive(64)
            .into_iter()
            .map(|(func, module, samples, pct)| HotspotRow {
                function: func.to_string(),
                module: module.to_string(),
                samples,
                percent: pct,
            })
            .collect();
        self.tree = trie.flame_tree(MAX_RECTS, MAX_ROWS);
        self.applied_window = window;
    }

    fn effective_root(&self) -> &[FlameNode] {
        match &self.zoom {
            Some(z) => std::slice::from_ref(z),
            None => &self.tree.root_children,
        }
    }

    fn root_weight(&self) -> u64 {
        match &self.zoom {
            Some(z) => z.inclusive,
            None => self.tree.root_children.iter().map(|c| c.inclusive).sum(),
        }
    }

    /// Percent of the root weight a node represents (for labels/tooltips).
    fn pct_of(&self, inclusive: u64) -> f64 {
        let root = self.root_weight();
        if root > 0 {
            inclusive as f64 / root as f64 * 100.0
        } else {
            0.0
        }
    }

    fn color_for(key: &str) -> Color32 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
        for b in key.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        // h % len < len, so the usize narrowing cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let idx = (h % PALETTE.len() as u64) as usize;
        PALETTE[idx]
    }

    fn matches_search(&self, key: &str) -> bool {
        !self.search.is_empty() && key.contains(self.search.as_str())
    }

    /// One UI frame of the panel: honest availability, bounded search,
    /// icicle render, zoom reset.
    #[allow(clippy::cast_possible_truncation)]
    pub fn ui(&mut self, ui: &mut Ui) {
        if !self.available {
            if let Some(err) = &self.error {
                ui.label(format!("CPU stacks unavailable: {err}"));
            } else {
                ui.label("CPU stacks unavailable for this session (counters-only recording).");
            }
            return;
        }
        // Single consolidated control bar for CPU Stacks
        ui.horizontal(|h| {
            if h.selectable_label(self.view_mode == StacksViewMode::Flamegraph, "🔥 Flamegraph").clicked() {
                self.view_mode = StacksViewMode::Flamegraph;
            }
            if h.selectable_label(self.view_mode == StacksViewMode::Hotspots, format!("⚡ Top Hotspots ({})", self.hotspots.len())).clicked() {
                self.view_mode = StacksViewMode::Hotspots;
            }
            h.separator();
            if h.button(if self.bottom_up {
                "◀ Bottom-Up (Callee)"
            } else {
                "▶ Top-Down (Caller)"
            })
            .on_hover_text("Toggle call-tree direction between caller-first (top-down) and callee-first (bottom-up)")
            .clicked()
            {
                self.bottom_up = !self.bottom_up;
                self.zoom = None; // direction change invalidates the zoom subtree
                self.reaggregate();
            }
            h.separator();
            h.weak("Thread:");
            let tids = self.tids.clone();
            let tids_hidden = self.tids_hidden;
            let selected_label = match self.tid_filter {
                None => format!("All threads ({})", tids.len()),
                Some(tid) => {
                    let n = tids
                        .iter()
                        .find(|(t, _)| *t == tid)
                        .map(|(_, c)| *c)
                        .unwrap_or(0);
                    format!("TID {tid} ({n})")
                }
            };
            egui::ComboBox::from_id_source("tid-filter")
                .selected_text(selected_label)
                .width(125.0)
                .show_ui(h, |ui| {
                    if ui
                        .selectable_label(self.tid_filter.is_none(), "All threads")
                        .clicked()
                    {
                        self.tid_filter = None;
                        self.reaggregate();
                    }
                    for (tid, count) in &tids {
                        if ui
                            .selectable_label(
                                self.tid_filter == Some(*tid),
                                format!("TID {tid} ({count} samples)"),
                            )
                            .clicked()
                        {
                            self.tid_filter = Some(*tid);
                            self.reaggregate();
                        }
                    }
                    if tids_hidden > 0 {
                        ui.small(format!("{tids_hidden} more threads hidden (bound)"));
                    }
                });
            if self.thread_filtered && h.small_button("×").on_hover_text("Reset thread filter").clicked() {
                self.tid_filter = None;
                self.reaggregate();
            }
            if let Some((lo, hi)) = self.t_range {
                let range_ms = (hi.saturating_sub(lo)) / 1_000_000;
                h.separator();
                h.weak("Window:");
                let mut w = self.window_ms;
                let start = h.add(
                    egui::DragValue::new(&mut w.0)
                        .clamp_range(0..=range_ms)
                        .suffix(" ms"),
                );
                h.weak("–");
                let end = h.add(
                    egui::DragValue::new(&mut w.1)
                        .clamp_range(0..=range_ms)
                        .suffix(" ms"),
                );
                if start.changed() || end.changed() {
                    if w.0 > w.1 {
                        std::mem::swap(&mut w.0, &mut w.1);
                    }
                    self.window_ms = w;
                    self.reaggregate();
                }
                if self.time_filtered && h.small_button("×").on_hover_text("Reset time window").clicked() {
                    self.window_ms = (0, range_ms);
                    self.reaggregate();
                }
            }
            h.with_layout(egui::Layout::right_to_left(egui::Align::Center), |h| {
                if self.zoom.is_some() && h.small_button("↻ Reset Zoom").clicked() {
                    self.zoom = None;
                }
                if !self.search.is_empty() && h.small_button("×").on_hover_text("Clear frame search").clicked() {
                    self.search.clear();
                }
                let search_resp = h.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search frames…")
                        .desired_width(150.0),
                );
                if search_resp.changed() {
                    self.search.truncate(MAX_SEARCH);
                }
            });
        });

        if self.view_mode == StacksViewMode::Hotspots {
            ui.add_space(4.0);
            ui.horizontal(|h| {
                h.weak("Filter:");
                h.add(
                    egui::TextEdit::singleline(&mut self.hotspot_search)
                        .hint_text("Filter functions or modules…")
                        .desired_width(220.0),
                );
                if !self.hotspot_search.is_empty() && h.small_button("×").clicked() {
                    self.hotspot_search.clear();
                }
                h.weak(format!(
                    "· {} hotspot functions ranked by exclusive CPU samples",
                    self.hotspots.len()
                ));
            });
            ui.add_space(4.0);
            let hs_filter = self.hotspot_search.trim().to_lowercase();
            let rows: Vec<&HotspotRow> = self
                .hotspots
                .iter()
                .filter(|r| {
                    hs_filter.is_empty()
                        || r.function.to_lowercase().contains(&hs_filter)
                        || r.module.to_lowercase().contains(&hs_filter)
                })
                .collect();
            egui::ScrollArea::vertical()
                .id_source("hotspots-scroll")
                .max_height(260.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("hotspots-grid")
                        .striped(true)
                        .num_columns(6)
                        .spacing([14.0, 5.0])
                        .show(ui, |ui| {
                            ui.strong("Rank");
                            ui.strong("Function Symbol");
                            ui.strong("Module");
                            ui.strong("Self Samples");
                            ui.strong("CPU Share");
                            ui.strong("Actions");
                            ui.end_row();
                            for (idx, row) in rows.iter().enumerate() {
                                ui.weak(format!("#{}", idx + 1));
                                ui.monospace(egui::RichText::new(row.function.as_str()).strong());
                                ui.weak(row.module.as_str());
                                ui.monospace(format!("{}", row.samples));
                                ui.horizontal(|ui| {
                                    let (bar_rect, _) = ui
                                        .allocate_exact_size(Vec2::new(72.0, 10.0), Sense::hover());
                                    ui.painter().rect_filled(
                                        bar_rect,
                                        2.0,
                                        Color32::from_rgba_unmultiplied(88, 166, 255, 35),
                                    );
                                    let fill_w = (bar_rect.width()
                                        * (row.percent as f32 / 100.0).clamp(0.0, 1.0))
                                    .max(2.0);
                                    let fill_rect = Rect::from_min_size(
                                        bar_rect.min,
                                        Vec2::new(fill_w, bar_rect.height()),
                                    );
                                    ui.painter().rect_filled(
                                        fill_rect,
                                        2.0,
                                        Self::color_for(&row.function),
                                    );
                                    ui.monospace(format!("{:>5.1}%", row.percent));
                                });
                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("📋 Copy")
                                        .on_hover_text("Copy symbol name")
                                        .clicked()
                                    {
                                        ui.output_mut(|o| o.copied_text = row.function.clone());
                                    }
                                    if ui
                                        .small_button("▶ Locate")
                                        .on_hover_text("Highlight in Flamegraph")
                                        .clicked()
                                    {
                                        self.search = row.function.clone();
                                        self.view_mode = StacksViewMode::Flamegraph;
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });
            return;
        }

        // Frame Inspector & Coverage Status Strip
        ui.horizontal(|h| {
            if let Some((ref sym, inc, exc, pct)) = self.hovered_frame {
                h.label(
                    egui::RichText::new(format!(
                        "▶ Frame: {}  ·  Inclusive: {} ({:.1}%)  ·  Self: {}",
                        shorten_symbol(sym),
                        inc,
                        pct,
                        exc
                    ))
                    .size(11.0)
                    .strong(),
                );
            } else if let Some(ref z) = self.zoom {
                h.label(
                    egui::RichText::new(format!(
                        "▶ Subtree Root: {} ({} samples) — Click any child to zoom deeper",
                        shorten_symbol(&z.key),
                        z.inclusive
                    ))
                    .size(11.0)
                    .strong(),
                );
            } else if let Some(top) = self.hotspots.first() {
                h.weak(
                    egui::RichText::new(format!(
                        "Top hotspot: {} ({:.1}% self, {} samples) · Click bar to zoom · Right-click to copy",
                        shorten_symbol(&top.function),
                        top.percent,
                        top.samples
                    ))
                    .size(11.0),
                );
            }
            h.with_layout(egui::Layout::right_to_left(egui::Align::Center), |h| {
                let cov_pct = self.coverage.unwrap_or(1.0) * 100.0;
                let mut meta = format!("{} samples · {:.0}% symbol coverage", self.samples, cov_pct);
                if self.time_filtered || self.thread_filtered {
                    meta.push_str(&format!(" ({} filtered)", self.filtered_out));
                }
                if self.tree.depth_capped {
                    meta.push_str(&format!(" · depth cap {MAX_ROWS}"));
                }
                h.weak(egui::RichText::new(meta).size(10.5));
            });
        });

        let avail = ui.available_size();
        let actual_depth = tree_max_depth(self.effective_root()).clamp(4, MAX_ROWS);
        let flame_h = (actual_depth as f32 * ROW_H + 4.0).min(avail.y.max(ROW_H * 4.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(avail.x, flame_h), Sense::hover());
        let mut painted = 0usize;
        let mut omitted = 0usize;
        let mut clicked: Option<FlameNode> = None;
        let mut hovered_now: Option<(String, u64, u64, f64)> = None;
        let root_w = rect.width();
        let weight_root = self.root_weight().max(1);
        let roots = self.effective_root();
        let mut x = rect.left();
        for child in roots {
            let w = root_w * (child.inclusive as f32 / weight_root as f32);
            let node_rect = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(w, ROW_H));
            paint_node(
                &mut *ui,
                self,
                child,
                node_rect,
                &mut painted,
                &mut omitted,
                &mut clicked,
                &mut hovered_now,
            );
            x += w;
        }
        self.hovered_frame = hovered_now;
        self.last_painted_rects = painted;
        self.last_painted_omitted = omitted;
        // A click anywhere zooms to that node (last click wins); the clone
        // is bounded by the tree caps.
        if let Some(node) = clicked {
            self.zoom = Some(node);
        }
        if omitted > 0 {
            ui.small(format!(
                "{omitted} frames hidden by the per-frame paint budget"
            ));
        }

        // Compact Top Exclusive Hotspots summary strip below the Flamegraph
        if !self.hotspots.is_empty() && ui.available_height() >= 68.0 {
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);
            let preview_rows: Vec<(usize, String, String, u64, f64)> = self
                .hotspots
                .iter()
                .take(4)
                .enumerate()
                .map(|(i, r)| {
                    (
                        i + 1,
                        r.function.clone(),
                        r.module.clone(),
                        r.samples,
                        r.percent,
                    )
                })
                .collect();
            let mut locate_sym: Option<String> = None;
            egui::Grid::new("flame-hotspots-preview")
                .striped(true)
                .num_columns(6)
                .spacing([14.0, 3.0])
                .show(ui, |ui| {
                    ui.weak(
                        egui::RichText::new("TOP SELF-TIME HOTSPOTS")
                            .size(10.0)
                            .strong(),
                    );
                    ui.weak(egui::RichText::new("MODULE").size(10.0).strong());
                    ui.weak(egui::RichText::new("SELF SAMPLES").size(10.0).strong());
                    ui.weak(egui::RichText::new("CPU SHARE").size(10.0).strong());
                    ui.weak(egui::RichText::new("ACTION").size(10.0).strong());
                    ui.end_row();
                    for (rank, func, module, samples, pct) in preview_rows {
                        ui.horizontal(|ui| {
                            ui.weak(egui::RichText::new(format!("#{rank}")).size(10.5));
                            ui.monospace(egui::RichText::new(&func).size(11.0).strong());
                        });
                        ui.weak(egui::RichText::new(&module).size(10.5));
                        ui.monospace(egui::RichText::new(format!("{samples}")).size(11.0));
                        ui.horizontal(|ui| {
                            let (bar_rect, _) =
                                ui.allocate_exact_size(Vec2::new(68.0, 8.0), Sense::hover());
                            ui.painter().rect_filled(
                                bar_rect,
                                2.0,
                                Color32::from_rgba_unmultiplied(88, 166, 255, 30),
                            );
                            let fill_w =
                                (bar_rect.width() * (pct as f32 / 100.0).clamp(0.0, 1.0)).max(2.0);
                            let fill_rect = Rect::from_min_size(
                                bar_rect.min,
                                Vec2::new(fill_w, bar_rect.height()),
                            );
                            ui.painter()
                                .rect_filled(fill_rect, 2.0, Self::color_for(&func));
                            ui.monospace(egui::RichText::new(format!("{pct:>5.1}%")).size(10.5));
                        });
                        if ui
                            .small_button("▶ Highlight")
                            .on_hover_text("Highlight all occurrences of this symbol in Flamegraph")
                            .clicked()
                        {
                            locate_sym = Some(func);
                        }
                        ui.end_row();
                    }
                });
            if let Some(sym) = locate_sym {
                if self.search == sym {
                    self.search.clear();
                } else {
                    self.search = sym;
                }
            }
        }
    }
}

fn tree_max_depth(nodes: &[FlameNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1usize.saturating_add(tree_max_depth(&n.children)))
        .max()
        .unwrap_or(0)
}

fn shorten_symbol(key: &str) -> String {
    if let Some((_, func)) = key.split_once('!') {
        return func.to_string();
    }
    if key.contains("::") {
        let parts: Vec<&str> = key.split("::").collect();
        if parts.len() >= 2 {
            return format!(
                "{}::{}",
                parts[parts.len().saturating_sub(2)],
                parts[parts.len().saturating_sub(1)]
            );
        }
    }
    key.to_string()
}

// Painter recursion is bounded by MAX_RECTS and MAX_ROWS.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_arguments
)]
fn paint_node(
    ui: &mut Ui,
    model: &StacksModel,
    node: &FlameNode,
    rect: Rect,
    painted: &mut usize,
    omitted: &mut usize,
    clicked: &mut Option<FlameNode>,
    hovered_out: &mut Option<(String, u64, u64, f64)>,
) {
    if *painted >= MAX_RECTS {
        *omitted += 1;
        return;
    }
    if rect.width() < 0.5 || node.inclusive == 0 {
        return;
    }
    let hit = model.matches_search(&node.key);
    let base = StacksModel::color_for(&node.key);
    let color = if model.search.is_empty() || hit {
        base
    } else {
        Color32::from_rgb(base.r() / 4 + 20, base.g() / 4 + 20, base.b() / 4 + 20)
    };
    let inner = rect.shrink(0.5);
    ui.painter().rect_filled(inner, 2.0, color);
    ui.painter().rect_stroke(
        inner,
        2.0,
        Stroke::new(0.75_f32, Color32::from_rgba_unmultiplied(10, 14, 20, 130)),
    );
    if hit {
        ui.painter().rect_stroke(
            rect,
            2.0,
            Stroke::new(1.5_f32, Color32::from_rgb(250, 224, 110)),
        );
    }
    *painted += 1;

    let pct = model.pct_of(node.inclusive);

    // Render legible symbol text + percentage inside the rectangle if width permits
    if rect.width() >= 28.0 {
        let short_name = shorten_symbol(&node.key);
        let display_name = if rect.width() >= 150.0 {
            format!("{short_name}  ({pct:.1}%, {})", node.inclusive)
        } else if rect.width() >= 85.0 {
            format!("{short_name} ({pct:.0}%)")
        } else {
            short_name
        };
        let font_id = egui::FontId::proportional(10.5);
        let max_text_width = rect.width() - 8.0;
        let mut text = display_name;
        let max_chars = (max_text_width / 5.6) as usize;
        if text.len() > max_chars && max_chars > 2 {
            text.truncate(max_chars.saturating_sub(1));
            text.push('…');
        }
        if max_chars > 1 {
            let lum = (u32::from(color.r()) * 299
                + u32::from(color.g()) * 587
                + u32::from(color.b()) * 114)
                / 1000;
            let text_color = if lum > 150 {
                Color32::from_rgb(15, 20, 25)
            } else {
                Color32::from_rgb(245, 248, 252)
            };
            ui.painter().text(
                Pos2::new(rect.left() + 4.0, rect.top() + 3.5),
                egui::Align2::LEFT_TOP,
                text,
                font_id,
                text_color,
            );
        }
    }

    // Interaction: hover tooltip + click-to-zoom + right-click copy.
    let inter = ui
        .allocate_rect(rect, Sense::click())
        .on_hover_text(format!(
            "{}\nInclusive: {} samples ({:.1}%)\nSelf / Exclusive: {} samples\n(Left-click to zoom subtree · Right-click to copy symbol)",
            node.key,
            node.inclusive,
            pct,
            node.exclusive
        ));
    if inter.hovered() {
        *hovered_out = Some((node.key.clone(), node.inclusive, node.exclusive, pct));
    }
    if inter.clicked() {
        *clicked = Some(node.clone());
    }
    if inter.secondary_clicked() {
        ui.output_mut(|o| o.copied_text = node.key.clone());
    }

    // Children: fill the parent's width from the left (icicle layout).
    let mut x = rect.left();
    let child_weight = node.inclusive.max(1);
    for child in &node.children {
        if *painted >= MAX_RECTS {
            *omitted += 1;
            return;
        }
        let w = rect.width() * (child.inclusive as f32 / child_weight as f32);
        let child_rect =
            Rect::from_min_size(egui::pos2(x, rect.top() + ROW_H), Vec2::new(w, ROW_H));
        paint_node(
            ui,
            model,
            child,
            child_rect,
            painted,
            omitted,
            clicked,
            hovered_out,
        );
        x += w;
    }
}

/// Test helper: does this subtree contain a node whose key contains `key`?
pub fn tree_contains(node: &FlameNode, key: &str) -> bool {
    if node.key.contains(key) {
        return true;
    }
    node.children.iter().any(|c| tree_contains(c, key))
}

/// Test helper: count nodes in a bounded tree.
pub fn count_nodes(nodes: &[FlameNode]) -> usize {
    nodes.len().saturating_add(
        nodes
            .iter()
            .map(|n| count_nodes(&n.children))
            .sum::<usize>(),
    )
}
