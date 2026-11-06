// GUI product binary: never allocate a console window (user-reported:
// a console came up alongside the app). Windows-subsystem apps show no
// console; the diagnostic surfaces stay in the UI and typed exit codes
// remain for automation.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! The `pulseforge-gui` binary (roadmap P7, ADR-002): native egui/eframe
//! window over WGPU (DX12 on Windows, Vulkan/GL on Linux) with a Glow
//! (OpenGL) fallback. The same `PulseForgeApp::update` that the headless
//! tests exercise renders here on a real GPU surface.

fn main() -> eframe::Result<(), eframe::Error> {
    // PF-708 deep link: `pulseforge-gui --open <session-dir>` opens the
    // dashboard for a recorded session without a daemon connection.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut offline_dir: Option<std::path::PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--open" {
            match iter.next() {
                Some(dir) => offline_dir = Some(std::path::PathBuf::from(dir)),
                None => {
                    eprintln!("pulseforge-gui: --open requires a session directory");
                    std::process::exit(2);
                }
            }
        }
    }

    if let Some(dir) = offline_dir {
        match pf_gui::open_session_offline(&dir) {
            Ok(model) => {
                let title = format!("PulseForge — {}", dir.display());
                return run_app(pf_gui::PulseForgeApp::offline(model), &title);
            }
            Err(e) => {
                eprintln!("pulseforge-gui: cannot open session: {e}");
                std::process::exit(3);
            }
        }
    }

    #[cfg(windows)]
    let client: Result<
        std::sync::Arc<dyn pf_gui::DaemonClient + Send + Sync>,
        pf_types::PfError,
    > = pf_gui::PipeDaemonClient::connect()
        .map(|c| std::sync::Arc::new(c) as std::sync::Arc<dyn pf_gui::DaemonClient + Send + Sync>);
    #[cfg(not(windows))]
    let client: Result<
        std::sync::Arc<dyn pf_gui::DaemonClient + Send + Sync>,
        pf_types::PfError,
    > = Err(pf_types::PfError::new(
        pf_types::ErrorCode::UnsupportedPlatform,
        "the GUI daemon transport is Windows-first; Linux UDS lands with P3 parity",
    ));

    let app = match client {
        Ok(c) => pf_gui::PulseForgeApp::new(c),
        Err(e) => {
            eprintln!("pulseforge-gui: {e}");
            eprintln!("hint: start the daemon with `pulseforge daemon` first");
            std::process::exit(2);
        }
    };

    run_app(app, "PulseForge")
}

/// Launch the app with WGPU (DX12 on Windows, Vulkan/GL on Linux) as the
/// primary renderer for GPU-accelerated performance. If WGPU fails to
/// initialize (no adapter, broken drivers), fall back to Glow (OpenGL)
/// which is universally supported.
fn run_app(app: pf_gui::PulseForgeApp, title: &str) -> eframe::Result<(), eframe::Error> {
    // Platform-appropriate WGPU backends:
    // - Windows: DX12 only (skip Vulkan — broken/missing ICDs cause hangs
    //   during adapter enumeration on systems without Vulkan drivers).
    // - Linux: Vulkan + GL (Vulkan if available, GL as fallback).
    #[cfg(target_os = "windows")]
    let backends = eframe::wgpu::Backends::DX12;
    #[cfg(not(target_os = "windows"))]
    let backends = eframe::wgpu::Backends::VULKAN | eframe::wgpu::Backends::GL;

    let wgpu_config = egui_wgpu::WgpuConfiguration {
        supported_backends: backends,
        ..Default::default()
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_title(title),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: wgpu_config,
        ..Default::default()
    };

    let result = eframe::run_native(
        title,
        options,
        Box::new(move |_cc| {
            Box::new(AppShell {
                inner: app,
                last_title: None,
            })
        }),
    );

    if result.is_err() {
        // WGPU failed — the user should retry with Glow.
        // In practice this is unlikely: DX12 is guaranteed on Windows 10/11
        // and Vulkan/GL are widely available on Linux.
        eprintln!("pulseforge-gui: WGPU init failed — try setting WGPU_BACKEND=gl and relaunching");
    }

    result
}

/// eframe requires `App`; delegate to `PulseForgeApp::update` so the logic
/// stays framework-callable for tests. The OS window title follows the
/// live state (version, open session, recording indicator) — sent only
/// when the string actually changes so idle frames cost nothing.
struct AppShell {
    inner: pf_gui::PulseForgeApp,
    last_title: Option<String>,
}

impl eframe::App for AppShell {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let title = self.inner.window_title();
        if self.last_title.as_deref() != Some(title.as_str()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = Some(title);
        }
        self.inner.update(ctx);
    }
}
