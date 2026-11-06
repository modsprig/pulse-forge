//! UI snapshot harness: renders the REAL app headlessly into BMP images
//! so UI/UX iterations can be reviewed pixel-exactly without a window.
//!
//! Pipeline: app.update(ctx) → tessellate → software rasterizer (egui
//! 0.27 meshes reference the font-atlas coverage map as their texture —
//! glyphs carry coverage, solid shapes sample the white region — so
//! triangle rasterization + alpha blending reproduces the intended look)
//! → 24-bit BMP on disk into target/ui-snapshots/ (gitignored).

// Snapshot loops use bounded counters; rasterizer index arithmetic is
// bounds-checked at the fb write.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::field_reassign_with_default,
    clippy::vec_init_then_push
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Pos2, Rect, Vec2};
use pf_gui::{PulseForgeApp, SessionRow, SessionSnapshot};
use pf_types::{CounterSnapshot, PfError, SessionId};

// --- BMP writer (24bpp, bottom-up rows, 4-byte row padding) ---

fn write_bmp(path: &Path, w: usize, h: usize, rgb: &[u8]) {
    let row_pad = (4 - (w * 3) % 4) % 4;
    let data_size = (w * 3 + row_pad) * h;
    let file_size = 54 + data_size;
    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(data_size as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let i = (y * w + x) * 4;
            out.push(rgb[i + 2]); // B
            out.push(rgb[i + 1]); // G
            out.push(rgb[i]); // R
        }
        out.resize(out.len() + row_pad, 0);
    }
    std::fs::write(path, out).expect("write bmp");
}

// --- Software rasterizer over egui's tessellated meshes ---

struct Fb {
    w: usize,
    h: usize,
    px: Vec<u8>, // RGBA
}

impl Fb {
    fn new(w: usize, h: usize, bg: [u8; 3]) -> Fb {
        let mut px = Vec::with_capacity(w * h * 4);
        let pixel = [bg[0], bg[1], bg[2], 255];
        let row = pixel.repeat(w);
        for _ in 0..h {
            px.extend_from_slice(&row);
        }
        Fb { w, h, px }
    }

    fn blend(&mut self, x: usize, y: usize, r: f32, g: f32, b: f32, a: f32) {
        if x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        // r/g/b arrive as raw 0-255 (Color32 channels); normalize.
        let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
        let i = (y * self.w + x) * 4;
        // Source-over with an OPAQUE destination: dst is weighted by
        // (1 - src_a), NOT by dst_a. The previous formula weighted dst by
        // dst_a (= 1 on the opaque fb), accumulating the destination at
        // full weight on every blend and washing everything toward white.
        let inv = 1.0 - a;
        for c in 0..3 {
            let sc = match c {
                0 => r,
                1 => g,
                _ => b,
            } * a;
            let dc = self.px[i + c] as f32 / 255.0 * inv;
            self.px[i + c] = ((sc + dc) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        self.px[i + 3] = 255; // the fb stays opaque
    }
}

/// egui 0.27's font atlas is an f32 COVERAGE map (glyphs covered, solid
/// shapes sample the white region with coverage 1.0). All meshes use it.
fn sample_cov(tex: &egui::FontImage, uv: Pos2) -> f32 {
    let w = tex.size[0];
    let h = tex.size[1];
    let x = ((uv.x * w as f32).floor() as i32).clamp(0, w as i32 - 1) as usize;
    let y = ((uv.y * h as f32).floor() as i32).clamp(0, h as i32 - 1) as usize;
    tex.pixels[y * w + x].clamp(0.0, 1.0)
}

fn rasterize_mesh(
    fb: &mut Fb,
    mesh: &egui::Mesh,
    clip: Rect,
    atlas: &egui::FontImage,
    textures: &std::collections::HashMap<egui::TextureId, std::sync::Arc<egui::ColorImage>>,
) {
    let verts = &mesh.vertices;
    let idx = &mesh.indices;
    let user_img = textures.get(&mesh.texture_id);

    for tri in idx.chunks_exact(3) {
        let (v0, v1, v2) = (
            verts[tri[0] as usize],
            verts[tri[1] as usize],
            verts[tri[2] as usize],
        );
        let min_x = v0.pos.x.min(v1.pos.x).min(v2.pos.x);
        let max_x = v0.pos.x.max(v1.pos.x).max(v2.pos.x);
        let min_y = v0.pos.y.min(v1.pos.y).min(v2.pos.y);
        let max_y = v0.pos.y.max(v1.pos.y).max(v2.pos.y);
        let x0 = (min_x.floor().max(clip.min.x).max(0.0)) as usize;
        let x1 = (max_x.ceil().min(clip.max.x).min(fb.w as f32)) as usize;
        let y0 = (min_y.floor().max(clip.min.y).max(0.0)) as usize;
        let y1 = (max_y.ceil().min(clip.max.y).min(fb.h as f32)) as usize;
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        let area = (v1.pos.x - v0.pos.x) * (v2.pos.y - v0.pos.y)
            - (v2.pos.x - v0.pos.x) * (v1.pos.y - v0.pos.y);
        if area.abs() < 1e-12 {
            continue;
        }
        let inv = 1.0 / area;
        for y in y0..y1 {
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let w0 =
                    ((v1.pos.x - px) * (v2.pos.y - py) - (v2.pos.x - px) * (v1.pos.y - py)) * inv;
                let w1 =
                    ((v2.pos.x - px) * (v0.pos.y - py) - (v0.pos.x - px) * (v2.pos.y - py)) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let uv = Pos2::new(
                    v0.uv.x * w0 + v1.uv.x * w1 + v2.uv.x * w2,
                    v0.uv.y * w0 + v1.uv.y * w1 + v2.uv.y * w2,
                );

                if let Some(img) = user_img {
                    let w_img = img.size[0];
                    let h_img = img.size[1];
                    let tx =
                        ((uv.x * w_img as f32).floor() as i32).clamp(0, w_img as i32 - 1) as usize;
                    let ty =
                        ((uv.y * h_img as f32).floor() as i32).clamp(0, h_img as i32 - 1) as usize;
                    let col = img.pixels[ty * w_img + tx];
                    let r = col.r() as f32;
                    let g = col.g() as f32;
                    let b = col.b() as f32;
                    let a = col.a() as f32 / 255.0;
                    fb.blend(x, y, r, g, b, a);
                } else {
                    let r = v0.color.r() as f32 * w0
                        + v1.color.r() as f32 * w1
                        + v2.color.r() as f32 * w2;
                    let g = v0.color.g() as f32 * w0
                        + v1.color.g() as f32 * w1
                        + v2.color.g() as f32 * w2;
                    let b = v0.color.b() as f32 * w0
                        + v1.color.b() as f32 * w1
                        + v2.color.b() as f32 * w2;
                    let a = v0.color.a() as f32 / 255.0 * w0
                        + v1.color.a() as f32 / 255.0 * w1
                        + v2.color.a() as f32 / 255.0 * w2;
                    let cov = sample_cov(atlas, uv);
                    fb.blend(x, y, r, g, b, a * cov);
                }
            }
        }
    }
}

fn render_to_bmp(
    path: &Path,
    ctx: &egui::Context,
    out: &egui::FullOutput,
    textures: &std::collections::HashMap<egui::TextureId, std::sync::Arc<egui::ColorImage>>,
    w: usize,
    h: usize,
    bg: [u8; 3],
) {
    let ppi = out.pixels_per_point;
    let shapes = ctx.tessellate(out.shapes.clone(), ppi);
    let mut fb = Fb::new(w, h, bg);
    let font_image = ctx.fonts(|f| f.image().clone());
    for clipped in shapes.iter() {
        let clip = clipped.clip_rect;
        let clip_px = Rect::from_min_max(
            Pos2::new(clip.min.x * ppi, clip.min.y * ppi),
            Pos2::new(clip.max.x * ppi, clip.max.y * ppi),
        );
        match &clipped.primitive {
            egui::epaint::Primitive::Mesh(m) => {
                rasterize_mesh(&mut fb, m, clip_px, &font_image, textures);
            }
            egui::epaint::Primitive::Callback(_) => {}
        }
    }
    write_bmp(path, w, h, &fb.px);
}

// --- Support: synthetic session with REAL chunk + stacks artifacts ---

const SID: &str = "00000000-0000-7000-8000-00000000abcd";

fn counter_snapshot(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 250_000_000,
        cpu_user_ns: Some(i * 1_200_000 + (i % 7) * 90_000),
        cpu_kernel_ns: Some(i * 300_000),
        resident_bytes: Some(48_000_000 + (i % 300) * 131_072),
        resident_peak_bytes: Some(64_000_000 + (i % 200) * 65_536),
        private_commit_bytes: Some(32_000_000 + (i % 150) * 65_536),
        private_commit_peak_bytes: Some(40_000_000),
        io_read_ops: Some(i * 3),
        io_write_ops: Some(i * 2),
        io_read_bytes: Some(i * 65_536),
        io_write_bytes: Some(i * 32_768),
        thread_count: Some(6),
        handle_count: Some(200),
        page_faults: Some(i * 41),
    }
}

/// Realistic stacks.jsonl: one hot loop (spec-like shapes), callee-first
/// frames with inline expansion and file/line, exactly as persist writes.
fn stacks_line(i: u64) -> String {
    let depth = 4 + (i % 3) as usize;
    let mut ips = String::new();
    for d in 0..depth {
        if d > 0 {
            ips.push(',');
        }
        ips.push_str(&format!("{}", 0x400000 + d * 16));
    }
    // Callee-first: innermost is the hot function most of the time.
    let names = ["spin_ms", "spec_next", "forward_unchecked", "main"];
    let mut frames = String::new();
    for d in (0..depth).rev() {
        if d != depth - 1 {
            frames.push(',');
        }
        let name = names[(i as usize + d) % names.len()];
        let inlined = d != depth - 1;
        frames.push_str(&format!(
            "{{\"module\":\"/usr/bin/spec\",\"symbol\":\"{name}\",\"offset\":{},\"inlined\":{},\"file\":\"src/hot.rs\",\"line\":{}}}",
            d * 8,
            inlined,
            10 + d
        ));
    }
    format!(
        "{{\"time_ns\":{},\"tid\":{},\"ips\":[{}],\"frames\":[{}]}}\n",
        (i + 1) * 250_000_000,
        4242,
        ips,
        frames
    )
}

/// A session dir with REAL artifacts: chunk store with 2k counter
/// snapshots, stacks.jsonl (2k samples), summary, maps.
fn synth_session(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-ui-snap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"schema":"pulseforge.session/0","session_id":"{SID}","state":"COMPLETED",
 "target":{{"os":"linux","pid":4242,"creation_identity":1,"boot_identity":"b","image":"spec-workload"}},
 "stop_reason":"duration","warnings":[]}}"#
        ),
    )
    .unwrap();

    // Chunk store: 2,000 counter snapshots (real density for the charts).
    let mut w = pf_storage::ChunkWriter::open(&dir).unwrap();
    for i in 0..2_000u64 {
        w.push(&counter_snapshot(i)).unwrap();
    }
    w.seal().unwrap();

    // Stacks artifacts.
    let mut text = String::new();
    for i in 0..2_000u64 {
        text.push_str(&stacks_line(i));
    }
    std::fs::write(dir.join("stacks.jsonl"), &text).unwrap();
    std::fs::write(
        dir.join("stacks-summary.json"),
        r#"{"schema":"pulseforge.stacks/0","samples":2000,"frames_total":28000,"frames_resolved":28000,"truncated":false,"leaf_symbols_resolved":2000,"leaf_unresolved":0}"#,
    )
    .unwrap();
    std::fs::write(dir.join("maps.json"), "[]").unwrap();
    dir
}

fn home_app() -> PulseForgeApp {
    let mut fake = pf_gui::viewmod::FakeDaemon::default();
    let mk = |i: u64, state: &str, image: &str, samples: u64| pf_gui::viewmod::SessionRow {
        id: SessionId::parse(&format!("00000000-0000-7000-8000-{i:012x}")).unwrap(),
        state: state.into(),
        samples,
        image: image.into(),
        stop_reason: None,
    };
    fake.sessions = vec![
        mk(1, "COMPLETED", "powershell.exe", 4_410),
        mk(2, "RECORDING", "explorer.exe", 12_884),
        mk(3, "COMPLETED", "cmd.exe", 931),
        mk(4, "FAILED", "cargo.exe", 77),
        mk(5, "COMPLETED", "svchost.exe", 15_002),
        mk(6, "RECORDING", "taskmgr.exe", 640),
    ];
    fake.process_rows = vec![
        pf_gui::viewmod::ProcessRow {
            pid: 9001,
            parent_pid: None,
            image: "rustc.exe".into(),
            window_title: None,
            cpu_pct: Some(97.3),
            rss_bytes: Some(812 * 1024 * 1024),
        },
        pf_gui::viewmod::ProcessRow {
            pid: 9002,
            parent_pid: None,
            image: "chrome.exe".into(),
            window_title: Some("PulseForge PR #42 - Google Chrome".into()),
            cpu_pct: Some(12.4),
            rss_bytes: Some(1_204 * 1024 * 1024),
        },
        pf_gui::viewmod::ProcessRow {
            pid: 4,
            parent_pid: None,
            image: "System".into(),
            window_title: None,

            cpu_pct: None,
            rss_bytes: None,
        },
        pf_gui::viewmod::ProcessRow {
            pid: 3388,
            parent_pid: Some(4),
            image: "explorer.exe".into(),
            window_title: Some("File Explorer".into()),
            cpu_pct: None,
            rss_bytes: None,
        },
        pf_gui::viewmod::ProcessRow {
            pid: 4120,
            parent_pid: Some(3388),
            image: "cmd.exe".into(),
            window_title: Some("Command Prompt - cargo test".into()),
            cpu_pct: None,
            rss_bytes: None,
        },
        pf_gui::viewmod::ProcessRow {
            pid: 5890,
            parent_pid: Some(4120),
            image: "powershell.exe".into(),
            window_title: Some("Windows PowerShell".into()),
            cpu_pct: None,
            rss_bytes: None,
        },
        pf_gui::viewmod::ProcessRow {
            pid: 7824,
            parent_pid: Some(3388),
            image: "taskmgr.exe".into(),
            window_title: Some("Task Manager".into()),
            cpu_pct: None,
            rss_bytes: None,
        },
    ];
    fake.storage_health.stacks_available = true;
    fake.storage_health.session_count = 6;
    fake.storage_health.total_bytes = 48 * 1024 * 1024;
    pf_gui::testing::hermetic_data_root();
    PulseForgeApp::new(Arc::new(fake))
}

fn dashboard_app() -> (PulseForgeApp, PathBuf) {
    let dir = synth_session("main");
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let app = PulseForgeApp::offline(model);
    (app, dir)
}

// --- Snapshots ---

fn out_dir() -> PathBuf {
    let d = PathBuf::from("target/ui-snapshots");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn render_app(app: &mut PulseForgeApp, name: &str, w: f32, h: f32, bg: [u8; 3]) {
    render_app_hover(app, name, w, h, bg, Pos2::ZERO);
}

/// Variant with the virtual pointer parked at `pointer` (hover states).
fn render_app_hover(
    app: &mut PulseForgeApp,
    name: &str,
    w: f32,
    h: f32,
    bg: [u8; 3],
    pointer: Pos2,
) {
    let ctx = egui::Context::default();
    let mk_input = |t: f64| egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
        time: Some(t),
        ..egui::RawInput::default()
    };
    // First frame moves the pointer; second frame renders the hover UI.
    let mut input = mk_input(1.0);
    input.events.push(egui::Event::PointerMoved(pointer));
    let out1 = ctx.run(input, |ctx| app.update(ctx));
    let mut input2 = mk_input(1.016);
    input2.events.push(egui::Event::PointerMoved(pointer));
    let out2 = ctx.run(input2, |ctx| app.update(ctx));

    let mut user_textures = std::collections::HashMap::new();
    for (id, delta) in out1
        .textures_delta
        .set
        .iter()
        .chain(out2.textures_delta.set.iter())
    {
        if let egui::ImageData::Color(ref c) = delta.image {
            user_textures.insert(*id, std::sync::Arc::clone(c));
        }
    }

    let dir = out_dir();
    let path = dir.join(format!("{name}.bmp"));
    render_to_bmp(
        &path,
        &ctx,
        &out2,
        &user_textures,
        w as usize,
        h as usize,
        bg,
    );
    println!("rendered {}", path.display());
}

#[test]
fn snapshot_all_views() {
    // FIRST RUN: no sessions, no daemon — the empty state a new user sees.
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(pf_gui::viewmod::FakeDaemon::default()));
    render_app(&mut app, "00-first-run", 1180.0, 780.0, [13, 17, 23]);

    // Home with a realistic session list.
    let mut app = home_app();
    render_app(&mut app, "01-home", 1180.0, 780.0, [13, 17, 23]);

    // Home with process tree view open.
    let mut app = home_app();
    app.home.process_view_mode = pf_gui::viewmod::ProcessViewMode::Tree;
    render_app(
        &mut app,
        "01b-home-process-tree",
        1180.0,
        780.0,
        [13, 17, 23],
    );

    // Diagnostics drawer open.
    let mut app = home_app();
    app.toggle_diagnostics();
    render_app(&mut app, "02-home-diagnostics", 1180.0, 780.0, [13, 17, 23]);

    // Compare panel open.
    let mut app = home_app();
    app.toggle_compare();
    render_app(&mut app, "03-home-compare", 1180.0, 780.0, [13, 17, 23]);

    // Dashboard (offline session: real chunks + stacks artifacts) —
    // charts, flame panel, metadata in one view. Tall render shows the
    // flame section below the fold.
    let (mut app, dir) = dashboard_app();
    render_app(&mut app, "04-dashboard", 1180.0, 780.0, [13, 17, 23]);

    // Hover state: pointer over the CPU chart — the tooltip must appear.
    let (mut app_h, _) = dashboard_app();
    render_app_hover(
        &mut app_h,
        "04b-dashboard-hover",
        1180.0,
        780.0,
        [13, 17, 23],
        Pos2::new(600.0, 175.0),
    );
    let (mut app_tall, _) = dashboard_app();
    render_app(
        &mut app_tall,
        "05-dashboard-tall",
        1180.0,
        1600.0,
        [13, 17, 23],
    );
    let _ = dir;

    // Light theme home.
    let mut app = home_app();
    app.theme = pf_gui::app::Theme::Light;
    render_app(&mut app, "06-home-light", 1180.0, 780.0, [240, 246, 252]);
}

/// A daemon that is not running: every call fails with the same typed
/// timeout the real pipe/UDS client produces (short absence budget).
struct DeadDaemon;
impl pf_gui::DaemonClient for DeadDaemon {
    fn capabilities(&self) -> Result<pf_types::json::JsonValue, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn list_processes(&self, _: u32) -> Result<Vec<pf_gui::ProcessRow>, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn start_attach(&self, _: u32, _: u64, _: u64, _: &str) -> Result<SessionId, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn start_run(
        &self,
        _exe: &str,
        _args: &[String],
        _duration_ms: u64,
        _interval_ms: u64,
        _label: &str,
    ) -> Result<SessionId, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn status(&self, _: SessionId) -> Result<SessionSnapshot, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn stop(&self, _: SessionId) -> Result<String, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn delete_session(&self, _: SessionId) -> Result<u64, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::UnsupportedPlatform,
            "dead daemon deletes nothing",
        ))
    }
    fn samples(&self, _: SessionId) -> Result<Vec<CounterSnapshot>, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn compare(&self, _: SessionId, _: SessionId) -> Result<pf_types::json::JsonValue, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn envelope(
        &self,
        _: SessionId,
        _: pf_storage::EnvelopeField,
        _: u64,
        _: u64,
        _: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
    fn storage_health(&self) -> Result<pf_gui::StorageHealth, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::Timeout,
            "TIMEOUT: daemon pipe not reachable (is the daemon running?)",
        ))
    }
}

#[test]
fn snapshot_daemon_down_state() {
    // The user-reported freeze state: daemon not running. The app must
    // render the error in the status line and stay interactive (the
    // responsiveness contract itself is asserted by the SlowDaemon test
    // in headless_ui.rs; this proves the PIXELS a user sees now).
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(DeadDaemon));
    // Let the background refresh fail and land: a few frames spaced out so
    // the status line deterministically shows the typed error.
    let ctx = egui::Context::default();
    for f in 0..4 {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.1);
        let _ = ctx.run(input, |ctx| app.update(ctx));
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    // With on-demand auto-start the honest end state is the failed
    // attempt plus the manual instruction — poll for the TERMINAL state
    // so the pixels show it, not the in-flight marker.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !app.home.status_line.contains("auto-start failed")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(0.2);
        let _ = ctx.run(input, |ctx| app.update(ctx));
    }
    render_app(&mut app, "07-daemon-down", 1180.0, 780.0, [13, 17, 23]);
    assert!(
        app.home.status_line.contains("auto-start failed"),
        "{}",
        app.home.status_line
    );
}

#[test]
fn offline_dashboard_survives_periodic_refresh() {
    // Bughunted in iteration 14: PulseForgeApp::offline used to store a
    // FakeDaemon as the client, so the periodic cadence refresh clobbered
    // the offline-loaded series with empty data ~700 ms after open. The
    // offline client must answer the periodic refreshes from the store.
    let (mut app, _dir) = dashboard_app();
    assert!(
        app.dashboard
            .as_ref()
            .is_some_and(|d| !d.resident_bytes.points.is_empty()),
        "offline open must load series"
    );
    // Span at least two refresh cadences with real frames.
    let ctx = egui::Context::default();
    for f in 0..8 {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.15);
        let _ = ctx.run(input, |ctx| app.update(ctx));
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let dash = app.dashboard.as_ref().expect("dashboard model");
    assert!(
        !dash.resident_bytes.points.is_empty(),
        "periodic refresh wiped the offline series"
    );
    assert!(dash.samples > 0, "periodic refresh wiped the sample count");
}
