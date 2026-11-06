//! PF-008 GUI feasibility spike (DEV-ONLY, not a workspace member).
//!
//! Produces measured evidence for the v0.4 native-GUI decision WITHOUT a
//! window:
//! (a) headless `egui` passes over a 100 000-point series + a 10 000-row
//!     virtualized table — pass time and tessellation time p50/p95;
//! (b) offscreen `wgpu` device: adapter, triangle render to texture, GPU
//!     readback — proving a WGPU device initializes on this machine.
//! Results: one JSON document on stdout.

use std::time::Instant;

fn percentiles(mut samples: Vec<f64>) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = |q: f64| samples[((q * (samples.len() - 1) as f64).round()) as usize];
    (idx(0.5), idx(0.95), *samples.last().unwrap())
}

fn main() {
    let mut report = String::from("{\n");

    // ------------------------------------------------------------------
    // (a) egui headless dense-UI pass timing
    // ------------------------------------------------------------------
    {
        use egui::{pos2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0));

        // Deterministic synthetic 100k-point metric series.
        let n_points = 100_000usize;
        let series: Vec<Pos2> = (0..n_points)
            .map(|i| {
                let x = i as f32 / n_points as f32 * 1600.0;
                let y = 450.0
                    + 200.0 * ((i as f32 * 0.13).sin() * 0.4 + (i as f32 * 0.011).cos());
                pos2(x, y)
            })
            .collect();
        let table_rows_total = 10_000usize;

        let mut pass_ms: Vec<f64> = Vec::new();
        let mut tess_ms: Vec<f64> = Vec::new();
        let mut vertices_peak = 0usize;

        for frame in 0..60 {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(screen);
            input.time = Some(frame as f64 * 0.016);
            let t0 = Instant::now();
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                // Chart: ONE polyline shape for 100k points (the realistic
                // way a dense timeline is drawn).
                let painter = ui.painter().clone();
                painter.add(egui::Shape::line(
                    series.clone(),
                    Stroke::new(1.0, Color32::LIGHT_BLUE),
                ));
                // Virtualized 10k-row table: layout cost must depend on
                // VISIBLE rows (~30), not on table_rows_total. The clip
                // rect is the viewport; rows below are never laid out.
                egui::ScrollArea::vertical().show_rows(
                    ui,
                    18.0,
                    table_rows_total,
                    |ui, range| {
                        for row in range {
                            ui.add(
                                egui::Label::new(format!("row {row}: value {:.3}", series
                                    .get(row % n_points)
                                    .map(|p| p.y)
                                    .unwrap_or(0.0)))
                                .sense(Sense::click()),
                            );
                        }
                    },
                );
                });
            });
            let tess_start = Instant::now();
            let clipped = ctx.tessellate(output.shapes, 1.0);
            let frame_tess_ms = tess_start.elapsed().as_secs_f64() * 1000.0;
            let _ = &clipped;
            pass_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            tess_ms.push(frame_tess_ms);
            vertices_peak = vertices_peak
                .max(clipped.iter().map(|c| match &c.primitive { egui::epaint::Primitive::Mesh(m) => m.vertices.len(), _ => 0 }).sum());
        }

        let (pass_p50, pass_p95, pass_max) = percentiles(pass_ms.clone());
        let (tess_p50, tess_p95, tess_max) = percentiles(tess_ms);
        report.push_str(&format!(
            "  \"egui_headless\": {{\n    \"frames\": {},\n    \"chart_points\": {},\n    \"table_rows_total\": {},\n    \"pass_ms\": {{\"p50\": {:.3}, \"p95\": {:.3}, \"max\": {:.3}}},\n    \"tessellation_ms\": {{\"p50\": {:.3}, \"p95\": {:.3}, \"max\": {:.3}}},\n    \"vertices_peak\": {}\n  }},\n",
            pass_ms.len(),
            n_points,
            table_rows_total,
            pass_p50, pass_p95, pass_max,
            tess_p50, tess_p95, tess_max,
            vertices_peak
        ));
    }

    // ------------------------------------------------------------------
    // (b) wgpu offscreen device + triangle render
    // ------------------------------------------------------------------
    {
        let runtime = tokio_wgpu_report();
        report.push_str(&runtime);
    }

    report.push_str("  \"done\": true\n}\n");
    println!("{report}");
}

fn tokio_wgpu_report() -> String {
    use std::borrow::Cow;
    pollster::block_on(wgpu_report())
}

async fn wgpu_report() -> String {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapter = match instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
    {
        Some(a) => a,
        None => {
            return "  \"wgpu_offscreen\": {\"adapter\": \"none available\"}\n".into();
        }
    };
    let info = adapter.get_info();
    let device_result = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await;
    let Ok((device, queue)) = device_result.map(|d| (d.0, d.1)) else {
        return format!(
            "  \"wgpu_offscreen\": {{\"adapter\": {:?}, \"device\": \"request failed\"}}\n",
            info.name
        );
    };

    // Offscreen render: fullbright triangle into a 256x256 RGBA texture.
    let size = 256u32;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("spike-target"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("spike-shader"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(
            "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> { \
             var p = array<vec2<f32>,3>(vec2(-0.7,-0.7), vec2(0.7,-0.7), vec2(0.0,0.7)); \
             return vec4<f32>(p[i], 0.0, 1.0); } \
             @fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(0.2,0.6,0.9,1.0); }",
        )),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spike-pipeline"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs",
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs",
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview: None,
    });

 

    let t0 = Instant::now();
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spike-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        rpass.set_pipeline(&pipeline);
        rpass.draw(0..3, 0..1);
    }
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike-readback"),
        size: u64::from(size * size) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(size * 4),
                rows_per_image: Some(size),
            },
        },
        wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));
    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = sender.send(r);
    });
    device.poll(wgpu::Maintain::wait()).panic_on_timeout();
    let _ = receiver.recv();
    let data = slice.get_mapped_range();
    let nonzero = data.chunks_exact(4).any(|px| px[2] > 0);
    let render_ms = t0.elapsed().as_secs_f64() * 1000.0;

    format!(
        "  \"wgpu_offscreen\": {{
    \"adapter\": {:?},
    \"backend\": {:?},
    \"triangle_pixels_nonzero\": {},
    \"render_and_readback_ms\": {:.3}
  }},
",
        info.name,
        format!("{:?}", info.backend),
        nonzero,
        render_ms
    )
}
