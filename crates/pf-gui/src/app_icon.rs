//! Application icon resolution, native extraction, procedural rendering,
//! and desktop window title resolution for Live Processes.
//!
//! On Windows, extracts native 32x32 / 16x16 application icons from running
//! processes and their executables via Win32 Shell API (`QueryFullProcessImageNameW`,
//! `SHGetFileInfoW`, `GetIconInfo`, `GetDIBits`, `App Paths`, `Services`), and
//! resolves top-level desktop window titles via `EnumDesktopWindows` / `EnumWindows`.
//! When native icons are unavailable (mock tests, Linux, access-denied kernel
//! pseudo-processes), renders studio-grade procedural category icons.

use std::collections::{HashMap, HashSet};

use egui::{Color32, Pos2, Rect, Stroke, Vec2};

/// Semantic category of a running process, inferred from its executable name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppCategory {
    Terminal,
    Compiler,
    Server,
    Database,
    Code,
    System,
    GenericApp,
}

impl AppCategory {
    pub fn from_image_name(name: &str) -> Self {
        let lower = name.to_lowercase();
        let stem = lower.trim_end_matches(".exe");
        if stem == "system"
            || stem == "svchost"
            || stem == "smss"
            || stem == "csrss"
            || stem == "lsass"
            || stem == "services"
        {
            AppCategory::System
        } else if stem.contains("term")
            || stem == "cmd"
            || stem == "powershell"
            || stem == "pwsh"
            || stem == "bash"
            || stem == "zsh"
            || stem == "wt"
            || stem == "conhost"
        {
            AppCategory::Terminal
        } else if stem.contains("cargo")
            || stem.contains("rustc")
            || stem.contains("clippy")
            || stem.contains("clang")
            || stem.contains("gcc")
            || stem.contains("build")
            || stem.contains("ninja")
            || stem.contains("make")
        {
            AppCategory::Compiler
        } else if stem.contains("server")
            || stem.contains("daemon")
            || stem.contains("nginx")
            || stem.contains("http")
            || stem.contains("api")
            || stem.contains("gateway")
        {
            AppCategory::Server
        } else if stem.contains("sql")
            || stem.contains("db")
            || stem.contains("postgres")
            || stem.contains("redis")
            || stem.contains("mongo")
        {
            AppCategory::Database
        } else if stem.contains("code")
            || stem.contains("devenv")
            || stem.contains("idea")
            || stem.contains("sublime")
            || stem.contains("vim")
            || stem.contains("emacs")
        {
            AppCategory::Code
        } else {
            AppCategory::GenericApp
        }
    }
}

/// Cache of GPU textures extracted from native OS process icons.
#[derive(Default)]
pub struct ProcessIconCache {
    textures: HashMap<String, egui::TextureHandle>,
    failed: HashSet<String>,
}

impl ProcessIconCache {
    pub fn get_or_load(
        &mut self,
        ctx: &egui::Context,
        pid: u32,
        image: &str,
    ) -> Option<&egui::TextureHandle> {
        if self.textures.contains_key(image) {
            return self.textures.get(image);
        }
        if self.failed.contains(image) {
            return None;
        }

        #[cfg(windows)]
        {
            if let Some((w, h, rgba)) = native::extract_process_icon(pid, image) {
                let color_img =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                let tex = ctx.load_texture(
                    format!("proc-icon-{image}"),
                    color_img,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(image.to_string(), tex);
                return self.textures.get(image);
            }
        }

        let _ = (ctx, pid);
        self.failed.insert(image.to_string());
        None
    }
}

/// Renders either the native OS application icon (if loaded/extracted) or a
/// studio-grade procedural category vector icon.
pub fn paint_process_icon(
    ui: &mut egui::Ui,
    cache: &mut ProcessIconCache,
    pid: u32,
    image: &str,
    size: Vec2,
) {
    if let Some(tex) = cache.get_or_load(ui.ctx(), pid, image) {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter().image(
            tex.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let cat = AppCategory::from_image_name(image);
        paint_category_icon(ui, cat, image, size);
    }
}

/// Query the desktop for visible top-level window titles mapped to their process IDs.
pub fn get_desktop_window_titles() -> HashMap<u32, String> {
    #[cfg(windows)]
    {
        native::get_desktop_window_titles()
    }
    #[cfg(not(windows))]
    {
        HashMap::new()
    }
}

/// Paints a rich procedural category icon with vector glyphs.
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
pub fn paint_category_icon(ui: &mut egui::Ui, category: AppCategory, name: &str, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();

    let accent_col = match category {
        AppCategory::Terminal => Color32::from_rgb(0x3f, 0xb9, 0x50), // Green
        AppCategory::Compiler => Color32::from_rgb(0xd2, 0x99, 0x22), // Amber
        AppCategory::Server => Color32::from_rgb(0x39, 0xc5, 0xcf),   // Cyan
        AppCategory::Database => Color32::from_rgb(0xbc, 0x8c, 0xff), // Purple
        AppCategory::Code => Color32::from_rgb(0x58, 0xa6, 0xff),     // Blue
        AppCategory::System => Color32::from_rgb(0x76, 0xe3, 0xea),   // Teal
        AppCategory::GenericApp => category_accent_from_name(name),
    };

    // Rounded background tile
    let bg_col =
        Color32::from_rgba_unmultiplied(accent_col.r(), accent_col.g(), accent_col.b(), 36);
    painter.rect_filled(rect, 4.0, bg_col);
    painter.rect_stroke(rect, 4.0, Stroke::new(0.75_f32, accent_col));

    match category {
        AppCategory::Terminal => {
            // Mini console prompt: `>` and `_`
            let p1 = Pos2::new(center.x - 3.5, center.y - 3.0);
            let p2 = Pos2::new(center.x - 1.0, center.y - 0.5);
            let p3 = Pos2::new(center.x - 3.5, center.y + 2.0);
            painter.line_segment([p1, p2], Stroke::new(1.2_f32, accent_col));
            painter.line_segment([p2, p3], Stroke::new(1.2_f32, accent_col));

            let u1 = Pos2::new(center.x + 1.0, center.y + 2.0);
            let u2 = Pos2::new(center.x + 4.5, center.y + 2.0);
            painter.line_segment([u1, u2], Stroke::new(1.2_f32, accent_col));
        }
        AppCategory::Compiler => {
            // Gear / Cog wheel: center circle + 4 cardinal teeth
            painter.circle_stroke(center, 3.2, Stroke::new(1.2_f32, accent_col));
            painter.circle_filled(center, 1.4, accent_col);
            // 4 teeth
            painter.line_segment(
                [
                    Pos2::new(center.x, center.y - 4.5),
                    Pos2::new(center.x, center.y - 2.5),
                ],
                Stroke::new(1.4_f32, accent_col),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, center.y + 2.5),
                    Pos2::new(center.x, center.y + 4.5),
                ],
                Stroke::new(1.4_f32, accent_col),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x - 4.5, center.y),
                    Pos2::new(center.x - 2.5, center.y),
                ],
                Stroke::new(1.4_f32, accent_col),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x + 2.5, center.y),
                    Pos2::new(center.x + 4.5, center.y),
                ],
                Stroke::new(1.4_f32, accent_col),
            );
        }
        AppCategory::Server => {
            // Dual-tier rack with LED status dots
            let rack1 =
                Rect::from_center_size(Pos2::new(center.x, center.y - 2.5), Vec2::new(10.0, 3.5));
            let rack2 =
                Rect::from_center_size(Pos2::new(center.x, center.y + 2.5), Vec2::new(10.0, 3.5));
            painter.rect_stroke(rack1, 1.0, Stroke::new(1.0_f32, accent_col));
            painter.rect_stroke(rack2, 1.0, Stroke::new(1.0_f32, accent_col));
            // LED dots
            painter.circle_filled(
                Pos2::new(rack1.right() - 2.0, rack1.center().y),
                0.9,
                accent_col,
            );
            painter.circle_filled(
                Pos2::new(rack2.right() - 2.0, rack2.center().y),
                0.9,
                accent_col,
            );
        }
        AppCategory::Database => {
            // Stacked database cylinders
            let r1 =
                Rect::from_center_size(Pos2::new(center.x, center.y - 3.0), Vec2::new(9.0, 3.0));
            let r2 =
                Rect::from_center_size(Pos2::new(center.x, center.y + 0.5), Vec2::new(9.0, 3.0));
            let r3 =
                Rect::from_center_size(Pos2::new(center.x, center.y + 4.0), Vec2::new(9.0, 3.0));
            painter.rect_stroke(r1, 1.5, Stroke::new(1.0_f32, accent_col));
            painter.rect_stroke(r2, 1.5, Stroke::new(1.0_f32, accent_col));
            painter.rect_stroke(r3, 1.5, Stroke::new(1.0_f32, accent_col));
        }
        AppCategory::Code => {
            // Code brackets `< >`
            // Left `<`
            let l1 = Pos2::new(center.x - 1.5, center.y - 3.5);
            let l2 = Pos2::new(center.x - 4.5, center.y);
            let l3 = Pos2::new(center.x - 1.5, center.y + 3.5);
            painter.line_segment([l1, l2], Stroke::new(1.2_f32, accent_col));
            painter.line_segment([l2, l3], Stroke::new(1.2_f32, accent_col));

            // Right `>`
            let r1 = Pos2::new(center.x + 1.5, center.y - 3.5);
            let r2 = Pos2::new(center.x + 4.5, center.y);
            let r3 = Pos2::new(center.x + 1.5, center.y + 3.5);
            painter.line_segment([r1, r2], Stroke::new(1.2_f32, accent_col));
            painter.line_segment([r2, r3], Stroke::new(1.2_f32, accent_col));
        }
        AppCategory::System => {
            // CPU Chip: central die + pin lines
            let die = Rect::from_center_size(center, Vec2::new(6.0, 6.0));
            painter.rect_filled(die, 1.0, accent_col);
            // 8 pins
            for offset in [-2.0_f32, 2.0] {
                painter.line_segment(
                    [
                        Pos2::new(center.x + offset, die.top()),
                        Pos2::new(center.x + offset, die.top() - 2.5),
                    ],
                    Stroke::new(1.0_f32, accent_col),
                );
                painter.line_segment(
                    [
                        Pos2::new(center.x + offset, die.bottom()),
                        Pos2::new(center.x + offset, die.bottom() + 2.5),
                    ],
                    Stroke::new(1.0_f32, accent_col),
                );
                painter.line_segment(
                    [
                        Pos2::new(die.left(), center.y + offset),
                        Pos2::new(die.left() - 2.5, center.y + offset),
                    ],
                    Stroke::new(1.0_f32, accent_col),
                );
                painter.line_segment(
                    [
                        Pos2::new(die.right(), center.y + offset),
                        Pos2::new(die.right() + 2.5, center.y + offset),
                    ],
                    Stroke::new(1.0_f32, accent_col),
                );
            }
        }
        AppCategory::GenericApp => {
            // Modern desktop window frame: title bar + window body + dot
            let win = Rect::from_center_size(center, Vec2::new(11.0, 9.0));
            painter.rect_stroke(win, 1.5, Stroke::new(1.0_f32, accent_col));
            let header_y = win.top() + 2.5;
            painter.line_segment(
                [
                    Pos2::new(win.left(), header_y),
                    Pos2::new(win.right(), header_y),
                ],
                Stroke::new(0.8_f32, accent_col),
            );
            // Mini title dot
            painter.circle_filled(
                Pos2::new(win.left() + 2.0, win.top() + 1.2),
                0.6,
                accent_col,
            );
        }
    }
}

#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
fn category_accent_from_name(name: &str) -> Color32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    let palette = [
        Color32::from_rgb(0x58, 0xa6, 0xff), // Blue
        Color32::from_rgb(0x3f, 0xb9, 0x50), // Green
        Color32::from_rgb(0xd2, 0x99, 0x22), // Amber
        Color32::from_rgb(0xbc, 0x8c, 0xff), // Purple
        Color32::from_rgb(0x39, 0xc5, 0xcf), // Cyan
        Color32::from_rgb(0xf7, 0x78, 0xba), // Pink
        Color32::from_rgb(0x76, 0xe3, 0xea), // Teal
        Color32::from_rgb(0xff, 0xa6, 0x57), // Orange
    ];
    let idx = (h % palette.len() as u64) as usize;
    palette[idx]
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod native {
    use std::collections::HashMap;
    use std::ffi::c_void;

    #[repr(C)]
    struct ShFileInfoW {
        h_icon: *mut c_void,
        i_icon: i32,
        dw_attributes: u32,
        sz_display_name: [u16; 260],
        sz_type_name: [u16; 80],
    }

    #[repr(C)]
    struct IconInfo {
        f_icon: i32,
        x_hotspot: u32,
        y_hotspot: u32,
        hbm_mask: *mut c_void,
        hbm_color: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Bitmap {
        bm_type: i32,
        bm_width: i32,
        bm_height: i32,
        bm_width_bytes: i32,
        bm_planes: u16,
        bm_bits_pixel: u16,
        bm_bits: *mut c_void,
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_xpels_per_meter: i32,
        bi_ypels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }

    const SHGFI_ICON: u32 = 0x000000100;
    const SHGFI_LARGEICON: u32 = 0x000000000;
    const SHGFI_SMALLICON: u32 = 0x000000001;
    const DIB_RGB_COLORS: u32 = 0;
    const BI_RGB: u32 = 0;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetFileInfoW(
            psz_path: *const u16,
            dw_file_attributes: u32,
            psfi: *mut ShFileInfoW,
            cb_file_info: u32,
            u_flags: u32,
        ) -> usize;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetIconInfo(h_icon: *mut c_void, piconinfo: *mut IconInfo) -> i32;
        fn DestroyIcon(h_icon: *mut c_void) -> i32;
        fn GetDC(h_wnd: *mut c_void) -> *mut c_void;
        fn ReleaseDC(h_wnd: *mut c_void, h_dc: *mut c_void) -> i32;
        fn EnumWindows(
            lpfn: unsafe extern "system" fn(*mut c_void, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn EnumDesktopWindows(
            h_desktop: *mut c_void,
            lpfn: unsafe extern "system" fn(*mut c_void, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn OpenDesktopW(
            lpsz_desktop: *const u16,
            dw_flags: u32,
            f_inherit: i32,
            dw_desired_access: u32,
        ) -> *mut c_void;
        fn OpenInputDesktop(dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> *mut c_void;
        fn CloseDesktop(h_desktop: *mut c_void) -> i32;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        fn GetWindowTextLengthW(hwnd: *mut c_void) -> i32;
        fn GetWindowTextW(hwnd: *mut c_void, lp_string: *mut u16, n_max_count: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, lpdw_process_id: *mut u32) -> u32;
        fn GetWindow(hwnd: *mut c_void, u_cmd: u32) -> *mut c_void;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetObjectW(h: *mut c_void, c: i32, pv: *mut c_void) -> i32;
        fn DeleteObject(ho: *mut c_void) -> i32;
        fn GetDIBits(
            hdc: *mut c_void,
            hbm: *mut c_void,
            start: u32,
            c_lines: u32,
            lpv_bits: *mut c_void,
            lpbmi: *mut BitmapInfoHeader,
            usage: u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn QueryFullProcessImageNameW(
            h_process: *mut c_void,
            dw_flags: u32,
            lp_exe_name: *mut u16,
            lpdw_size: *mut u32,
        ) -> i32;
        fn SearchPathW(
            lp_path: *const u16,
            lp_file_name: *const u16,
            lp_extension: *const u16,
            n_buffer_length: u32,
            lp_buffer: *mut u16,
            lp_file_part: *mut *mut u16,
        ) -> u32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            h_key: *mut c_void,
            lp_sub_key: *const u16,
            ul_options: u32,
            sam_desired: u32,
            phk_result: *mut *mut c_void,
        ) -> i32;
        fn RegEnumKeyExW(
            h_key: *mut c_void,
            dw_index: u32,
            lp_name: *mut u16,
            lpcch_name: *mut u32,
            lp_reserved: *mut u32,
            lp_class: *mut u16,
            lpcch_class: *mut u32,
            lpft_last_write_time: *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            h_key: *mut c_void,
            lp_value_name: *const u16,
            lp_reserved: *mut u32,
            lp_type: *mut u32,
            lp_data: *mut u8,
            lpcb_data: *mut u32,
        ) -> i32;
        fn RegCloseKey(h_key: *mut c_void) -> i32;
    }

    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
    fn get_service_image_map() -> &'static HashMap<String, String> {
        static MAP: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        MAP.get_or_init(|| {
            let mut map = HashMap::new();
            let root = 0x8000_0002_usize as *mut c_void; // HKLM
            let subkey: Vec<u16> = r"SYSTEM\CurrentControlSet\Services"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut h_services: *mut c_void = std::ptr::null_mut();
            unsafe {
                if RegOpenKeyExW(root, subkey.as_ptr(), 0, 0x20019, &mut h_services) == 0 {
                    let mut index = 0u32;
                    let mut name_buf = [0u16; 256];
                    loop {
                        let mut name_len = name_buf.len() as u32;
                        let res = RegEnumKeyExW(
                            h_services,
                            index,
                            name_buf.as_mut_ptr(),
                            &mut name_len,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        );
                        if res != 0 {
                            break;
                        }
                        index += 1;

                        let mut h_svc: *mut c_void = std::ptr::null_mut();
                        if RegOpenKeyExW(h_services, name_buf.as_ptr(), 0, 0x20019, &mut h_svc) == 0
                        {
                            let val_name: Vec<u16> = "ImagePath"
                                .encode_utf16()
                                .chain(std::iter::once(0))
                                .collect();
                            let mut val_buf = [0u16; 1024];
                            let mut val_cb = (val_buf.len() * 2) as u32;
                            let mut typ = 0u32;
                            let q = RegQueryValueExW(
                                h_svc,
                                val_name.as_ptr(),
                                std::ptr::null_mut(),
                                &mut typ,
                                val_buf.as_mut_ptr() as *mut u8,
                                &mut val_cb,
                            );
                            RegCloseKey(h_svc);
                            if q == 0 && val_cb > 2 {
                                let chars = (val_cb / 2) as usize;
                                let end = if chars > 0 && val_buf[chars - 1] == 0 {
                                    chars - 1
                                } else {
                                    chars
                                };
                                let raw_str = String::from_utf16_lossy(&val_buf[..end]);
                                if let Some(parsed) = clean_service_image_path(&raw_str) {
                                    if let Some(leaf) = std::path::Path::new(&parsed)
                                        .file_name()
                                        .and_then(|f| f.to_str())
                                    {
                                        map.insert(leaf.to_lowercase(), parsed);
                                    }
                                }
                            }
                        }
                    }
                    RegCloseKey(h_services);
                }
            }
            map
        })
    }

    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
    pub fn get_desktop_window_titles() -> HashMap<u32, String> {
        let mut map: HashMap<u32, String> = HashMap::new();

        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap,
            clippy::arithmetic_side_effects,
            clippy::cast_possible_truncation
        )]
        unsafe extern "system" fn enum_proc(hwnd: *mut c_void, lparam: isize) -> i32 {
            let map_ptr = lparam as *mut HashMap<u32, String>;
            unsafe {
                if IsWindowVisible(hwnd) != 0 {
                    let len = GetWindowTextLengthW(hwnd);
                    if len > 0 {
                        let owner = GetWindow(hwnd, 4); // GW_OWNER = 4
                        let mut buf = vec![0u16; (len + 2) as usize];
                        let read_len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
                        if read_len > 0 {
                            let title = String::from_utf16_lossy(&buf[..read_len as usize])
                                .trim()
                                .to_string();
                            if !title.is_empty()
                                && title != "Program Manager"
                                && title != "Windows Input Experience"
                            {
                                let mut pid = 0u32;
                                GetWindowThreadProcessId(hwnd, &mut pid);
                                if pid > 0 {
                                    let map = &mut *map_ptr;
                                    if owner.is_null() || !map.contains_key(&pid) {
                                        map.insert(pid, title);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            1
        }

        unsafe {
            let map_ptr = &mut map as *mut HashMap<u32, String> as isize;
            EnumWindows(enum_proc, map_ptr);

            if map.is_empty() {
                let default_desk: Vec<u16> =
                    "default".encode_utf16().chain(std::iter::once(0)).collect();
                let mut desk = OpenDesktopW(default_desk.as_ptr(), 0, 0, 0x0100 | 0x0001);
                if desk.is_null() {
                    desk = OpenInputDesktop(0, 0, 0x0100 | 0x0001);
                }
                if !desk.is_null() {
                    EnumDesktopWindows(desk, enum_proc, map_ptr);
                    CloseDesktop(desk);
                }
            }
        }

        map
    }

    fn clean_service_image_path(raw: &str) -> Option<String> {
        let mut s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(stripped) = s.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                s = &stripped[..end];
            } else {
                s = stripped;
            }
        } else if let Some(space) = s.find(' ') {
            let first = &s[..space];
            if first.to_lowercase().ends_with(".exe") {
                s = first;
            }
        }

        let mut path_str = s.to_string();
        if let Ok(windir) = std::env::var("SystemRoot") {
            let lower = path_str.to_lowercase();
            if lower.starts_with(r"\systemroot\") || lower.starts_with("%systemroot%") {
                path_str = format!(r"{windir}\{}", &path_str[12..]);
            } else if lower.starts_with(r"system32\") {
                path_str = format!(r"{windir}\System32\{}", &path_str[9..]);
            }
        }

        if std::path::Path::new(&path_str).exists() {
            Some(path_str)
        } else {
            None
        }
    }

    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
    fn resolve_executable_path(pid: u32, image: &str) -> Option<Vec<u16>> {
        // 1. Direct PID query (running process in current or accessible session)
        if pid > 0 {
            unsafe {
                let h_proc = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
                if !h_proc.is_null() {
                    let mut path_buf = [0u16; 1024];
                    let mut path_len = path_buf.len() as u32;
                    let ok =
                        QueryFullProcessImageNameW(h_proc, 0, path_buf.as_mut_ptr(), &mut path_len);
                    CloseHandle(h_proc);
                    if ok != 0 && path_len > 0 && (path_len as usize) < path_buf.len() {
                        let mut v = path_buf[..path_len as usize].to_vec();
                        v.push(0);
                        return Some(v);
                    }
                }
            }
        }

        // 2. Absolute or relative path that actually exists
        if std::path::Path::new(image).exists() {
            let mut v: Vec<u16> = image.encode_utf16().collect();
            v.push(0);
            return Some(v);
        }

        let file_name = if image.to_lowercase().ends_with(".exe") {
            image.to_string()
        } else {
            format!("{image}.exe")
        };

        // 3. Search common Windows system paths (System32, Windows, wbem, PowerShell, SysWOW64)
        if let Ok(windir) = std::env::var("SystemRoot") {
            let candidates = [
                format!(r"{windir}\System32\{file_name}"),
                format!(r"{windir}\{file_name}"),
                format!(r"{windir}\System32\wbem\{file_name}"),
                format!(r"{windir}\System32\WindowsPowerShell\v1.0\{file_name}"),
                format!(r"{windir}\SysWOW64\{file_name}"),
            ];
            for cand in &candidates {
                if std::path::Path::new(cand).exists() {
                    let mut v: Vec<u16> = cand.encode_utf16().collect();
                    v.push(0);
                    return Some(v);
                }
            }
        }

        // 4. SearchPathW (Application Directory, System directories, and PATH)
        unsafe {
            let mut wide_file: Vec<u16> = file_name.encode_utf16().collect();
            wide_file.push(0);
            let mut found_buf = [0u16; 1024];
            let len = SearchPathW(
                std::ptr::null(),
                wide_file.as_ptr(),
                std::ptr::null(),
                found_buf.len() as u32,
                found_buf.as_mut_ptr(),
                std::ptr::null_mut(),
            );
            if len > 0 && (len as usize) < found_buf.len() {
                let mut v = found_buf[..len as usize].to_vec();
                v.push(0);
                return Some(v);
            }
        }

        // 5. Windows Registry: App Paths (HKLM & HKCU)
        for root in [
            0x8000_0002_usize as *mut c_void,
            0x8000_0001_usize as *mut c_void,
        ] {
            let subkey =
                format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{file_name}");
            let mut wide_key: Vec<u16> = subkey.encode_utf16().collect();
            wide_key.push(0);
            let mut hkey: *mut c_void = std::ptr::null_mut();
            unsafe {
                if RegOpenKeyExW(root, wide_key.as_ptr(), 0, 0x20019, &mut hkey) == 0 {
                    let mut buf = [0u16; 1024];
                    let mut cb = (buf.len() * 2) as u32;
                    let mut typ: u32 = 0;
                    let q_ok = RegQueryValueExW(
                        hkey,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        &mut typ,
                        buf.as_mut_ptr() as *mut u8,
                        &mut cb,
                    );
                    RegCloseKey(hkey);
                    if q_ok == 0 && (typ == 1 || typ == 2) && cb > 2 {
                        let chars = (cb / 2) as usize;
                        let end = if chars > 0 && buf[chars - 1] == 0 {
                            chars - 1
                        } else {
                            chars
                        };
                        let mut s = String::from_utf16_lossy(&buf[..end]);
                        s = s.trim().trim_matches('"').to_string();
                        if std::path::Path::new(&s).exists() {
                            let mut v: Vec<u16> = s.encode_utf16().collect();
                            v.push(0);
                            return Some(v);
                        }
                    }
                }
            }
        }

        // 6. Windows Services Registry Map (HKLM\SYSTEM\CurrentControlSet\Services)
        if let Some(path) = get_service_image_map().get(&file_name.to_lowercase()) {
            let mut v: Vec<u16> = path.encode_utf16().collect();
            v.push(0);
            return Some(v);
        }

        // 7. Direct Service Key lookup (in case service was added after init)
        let svc_name = file_name.trim_end_matches(".exe");
        let svc_key = format!(r"SYSTEM\CurrentControlSet\Services\{svc_name}");
        let mut wide_svc: Vec<u16> = svc_key.encode_utf16().collect();
        wide_svc.push(0);
        let mut hkey: *mut c_void = std::ptr::null_mut();
        unsafe {
            if RegOpenKeyExW(
                0x8000_0002_usize as *mut c_void,
                wide_svc.as_ptr(),
                0,
                0x20019,
                &mut hkey,
            ) == 0
            {
                let mut val_name: Vec<u16> = "ImagePath".encode_utf16().collect();
                val_name.push(0);
                let mut buf = [0u16; 1024];
                let mut cb = (buf.len() * 2) as u32;
                let mut typ: u32 = 0;
                let q_ok = RegQueryValueExW(
                    hkey,
                    val_name.as_ptr(),
                    std::ptr::null_mut(),
                    &mut typ,
                    buf.as_mut_ptr() as *mut u8,
                    &mut cb,
                );
                RegCloseKey(hkey);
                if q_ok == 0 && cb > 2 {
                    let chars = (cb / 2) as usize;
                    let end = if chars > 0 && buf[chars - 1] == 0 {
                        chars - 1
                    } else {
                        chars
                    };
                    let raw_str = String::from_utf16_lossy(&buf[..end]);
                    if let Some(parsed) = clean_service_image_path(&raw_str) {
                        let mut v: Vec<u16> = parsed.encode_utf16().collect();
                        v.push(0);
                        return Some(v);
                    }
                }
            }
        }

        None
    }

    #[allow(
        unsafe_code,
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn extract_process_icon(pid: u32, image: &str) -> Option<(u32, u32, Vec<u8>)> {
        let wide_path = resolve_executable_path(pid, image)?;

        let mut sfi: ShFileInfoW = unsafe { std::mem::zeroed() };
        let cb = std::mem::size_of::<ShFileInfoW>() as u32;

        // Query large 32x32 icon for high-DPI clarity and smooth downscaling
        let mut res = unsafe {
            SHGetFileInfoW(
                wide_path.as_ptr(),
                0,
                &mut sfi,
                cb,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };

        if res == 0 || sfi.h_icon.is_null() {
            // Fallback to small 16x16 icon
            res = unsafe {
                SHGetFileInfoW(
                    wide_path.as_ptr(),
                    0,
                    &mut sfi,
                    cb,
                    SHGFI_ICON | SHGFI_SMALLICON,
                )
            };
        }

        if res == 0 || sfi.h_icon.is_null() {
            return None;
        }

        extract_icon_pixels(sfi.h_icon)
    }

    #[allow(
        unsafe_code,
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    fn extract_icon_pixels(h_icon: *mut c_void) -> Option<(u32, u32, Vec<u8>)> {
        let mut icon_info: IconInfo = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetIconInfo(h_icon, &mut icon_info) };
        unsafe { DestroyIcon(h_icon) };
        if ok == 0 {
            return None;
        }

        let hbm_color = icon_info.hbm_color;
        let hbm_mask = icon_info.hbm_mask;

        if hbm_color.is_null() {
            if !hbm_mask.is_null() {
                unsafe { DeleteObject(hbm_mask) };
            }
            return None;
        }

        let mut bmp = Bitmap::default();
        let ok_obj = unsafe {
            GetObjectW(
                hbm_color,
                std::mem::size_of::<Bitmap>() as i32,
                &mut bmp as *mut _ as *mut c_void,
            )
        };
        if ok_obj == 0 || bmp.bm_width <= 0 || bmp.bm_height <= 0 {
            unsafe {
                DeleteObject(hbm_color);
                if !hbm_mask.is_null() {
                    DeleteObject(hbm_mask);
                }
            }
            return None;
        }

        let width = bmp.bm_width as u32;
        let height = bmp.bm_height as u32;
        let pixel_count = (width * height) as usize;
        let mut bgra: Vec<u8> = vec![0u8; pixel_count * 4];

        let mut bmi: BitmapInfoHeader = unsafe { std::mem::zeroed() };
        bmi.bi_size = std::mem::size_of::<BitmapInfoHeader>() as u32;
        bmi.bi_width = bmp.bm_width;
        bmi.bi_height = -bmp.bm_height; // Top-down DIB
        bmi.bi_planes = 1;
        bmi.bi_bit_count = 32;
        bmi.bi_compression = BI_RGB;

        let mut mask_bgra: Option<Vec<u8>> = None;

        unsafe {
            let hdc = GetDC(std::ptr::null_mut());
            if !hdc.is_null() {
                GetDIBits(
                    hdc,
                    hbm_color,
                    0,
                    height,
                    bgra.as_mut_ptr() as *mut c_void,
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                if !hbm_mask.is_null() {
                    let mut mask_buf = vec![0u8; pixel_count * 4];
                    let m_ok = GetDIBits(
                        hdc,
                        hbm_mask,
                        0,
                        height,
                        mask_buf.as_mut_ptr() as *mut c_void,
                        &mut bmi,
                        DIB_RGB_COLORS,
                    );
                    if m_ok != 0 {
                        mask_bgra = Some(mask_buf);
                    }
                }
                ReleaseDC(std::ptr::null_mut(), hdc);
            }
            DeleteObject(hbm_color);
            if !hbm_mask.is_null() {
                DeleteObject(hbm_mask);
            }
        }

        // Check if alpha channel is already present in hbm_color
        let mut has_non_zero_alpha = false;
        for i in 0..pixel_count {
            if bgra[i * 4 + 3] > 0 {
                has_non_zero_alpha = true;
                break;
            }
        }

        let mut rgba = vec![0u8; pixel_count * 4];
        for i in 0..pixel_count {
            let b = bgra[i * 4];
            let g = bgra[i * 4 + 1];
            let r = bgra[i * 4 + 2];
            let a = if has_non_zero_alpha {
                bgra[i * 4 + 3]
            } else if let Some(ref mask) = mask_bgra {
                // Monochrome mask: 0 = opaque, 255 = transparent
                if mask[i * 4] > 128 { 0 } else { 255 }
            } else {
                255
            };

            rgba[i * 4] = r;
            rgba[i * 4 + 1] = g;
            rgba[i * 4 + 2] = b;
            rgba[i * 4 + 3] = a;
        }

        Some((width, height, rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_category_categorizes_common_tools() {
        assert_eq!(
            AppCategory::from_image_name("powershell.exe"),
            AppCategory::Terminal
        );
        assert_eq!(AppCategory::from_image_name("pwsh"), AppCategory::Terminal);
        assert_eq!(
            AppCategory::from_image_name("cargo.exe"),
            AppCategory::Compiler
        );
        assert_eq!(AppCategory::from_image_name("rustc"), AppCategory::Compiler);
        assert_eq!(
            AppCategory::from_image_name("nginx.exe"),
            AppCategory::Server
        );
        assert_eq!(
            AppCategory::from_image_name("postgres"),
            AppCategory::Database
        );
        assert_eq!(AppCategory::from_image_name("code.exe"), AppCategory::Code);
        assert_eq!(AppCategory::from_image_name("System"), AppCategory::System);
        assert_eq!(
            AppCategory::from_image_name("my-app.exe"),
            AppCategory::GenericApp
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_native_icon_succeeds_on_system_executable() {
        // Current process PID always exists on Windows
        let pid = std::process::id();
        let res = native::extract_process_icon(pid, "cargo.exe");
        assert!(
            res.is_some(),
            "failed to extract icon for cargo.exe or current process"
        );
        let (w, h, rgba) = res.unwrap();
        assert!(w > 0 && h > 0);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[cfg(windows)]
    #[test]
    fn enumerate_and_extract_icons_report() {
        let procs = pf_windows::enumerate::processes(500).unwrap();
        let mut ok = 0;
        let mut fail = 0;
        for p in procs {
            if let Some((_w, _h, _)) = native::extract_process_icon(p.pid, &p.image_name) {
                ok += 1;
            } else {
                fail += 1;
            }
        }
        println!("TOTAL 500: ok={}, fail={}", ok, fail);
        assert!(
            ok > fail,
            "expected majority of real processes to extract native icons"
        );
    }

    #[cfg(windows)]
    #[test]
    fn get_desktop_window_titles_returns_map() {
        let titles = native::get_desktop_window_titles();
        println!("Found {} window titles", titles.len());
        for (pid, title) in &titles {
            println!("  PID {}: {}", pid, title);
        }
        assert!(
            !titles.is_empty(),
            "expected to find desktop windows on active Windows session"
        );
    }
}
