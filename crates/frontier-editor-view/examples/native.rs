// SPDX-License-Identifier: Apache-2.0
//! Native demo: a winit window driving the editor core through the wgpu view.
//!
//!     cargo run -p frontier-editor-view --release --example native -- [path]
//!     cargo run -p frontier-editor-view --release --example native -- --log        # 5M-line log
//!     cargo run -p frontier-editor-view --release --example native -- --review     # proposal
//!     cargo run -p frontier-editor-view --release --example native -- --bench      # headless numbers
//!
//! Keys: everything from the core keymap; `F2` toggles light/dark, `F3` toggles
//! side-by-side diff, `F5` zooms the host transform (simulates a zoomed board node),
//! `Cmd/Ctrl+S` prints the EditEvent, `Cmd/Ctrl+Enter` in review mode applies the
//! accepted hunks and prints the ApprovalEvent.

use frontier_editor::provenance::Author;
use frontier_editor::source::SyntheticLog;
use frontier_editor::syntax::Language;
use frontier_editor_view::{
    DiffMode, EditorView, HostRequest, InputEvent, Key, KeyCode, Modifiers, Placement, Theme,
    ViewConfig,
};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{Window, WindowId};

const SAMPLE_CONFIG: &str = r#"# Frontier training config (frontier_train.config.Config)
task: surface
data:
  path: datasets/ahmedml-500
  target_field: pMean
  num_points: 32768
  point_features: [normals]
  batch_size: 4
model:
  name: physicsnemo.figconvnet
  params:
    hidden_channels: [48, 96, 128]
    num_levels: 2
optim:
  lr: 0.001
  schedule: cosine
  warmup_steps: 300
train:
  epochs: 400
  max_steps: 12000
  out_dir: runs/demo
  amp: true
logging:
  project: ahmedml-500
  tags: [ahmedml, surface-pressure]
"#;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    view: EditorView,
    start: Instant,
    mods: Modifiers,
    clipboard: String,
    host_scale: f32,
    mode: Mode,
    last_click: (Instant, u8),
    frame_times: Vec<f64>,
    last_cursor: (f32, f32),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    File,
    Log,
    Review,
}

fn now_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

impl App {
    fn load(&mut self, args: &[String]) {
        let path = args.iter().find(|a| !a.starts_with("--")).cloned();
        self.mode = if args.iter().any(|a| a == "--log") {
            Mode::Log
        } else if args.iter().any(|a| a == "--review") {
            Mode::Review
        } else {
            Mode::File
        };
        match self.mode {
            Mode::Log => {
                self.view
                    .open_log(
                        Box::new(SyntheticLog {
                            line_count: 5_000_000,
                        }),
                        Language::Log,
                    )
                    .expect("log");
            }
            _ => {
                let (name, text) = match path {
                    Some(p) => (
                        p.clone(),
                        std::fs::read_to_string(&p).unwrap_or_else(|e| format!("# {e}\n")),
                    ),
                    None => ("configs/demo.yaml".to_string(), SAMPLE_CONFIG.to_string()),
                };
                self.view.open(&name, &text);
                if let Some(mut e) = self.view.editor_mut() {
                    e.use_registry(&frontier_editor::schema::SchemaRegistry::builtin());
                    e.register_plugin(Box::new(frontier_editor::plugin::WhitespaceLint));
                    if self.mode == Mode::Review {
                        let proposed = text
                            .replace("lr: 0.001", "lr: 0.0005")
                            .replace("epochs: 400", "epochs: 600\n  val_every: 10");
                        e.review(&proposed);
                    }
                }
            }
        }
    }

    fn requests(&mut self, reqs: Vec<HostRequest>) {
        for r in reqs {
            match r {
                HostRequest::Copy(t) => self.clipboard = t,
                HostRequest::Paste => {
                    let t = self.clipboard.clone();
                    let r = self.view.handle(InputEvent::Text(t), now_ms(self.start));
                    self.requests(r);
                }
                HostRequest::Save => {
                    if let Some(mut e) = self.view.editor_mut() {
                        let ev = e.save(Author::User { id: "demo".into() });
                        println!("{}", serde_json::to_string_pretty(&ev).expect("json"));
                    }
                }
                HostRequest::Apply => {
                    if let Some(mut e) = self.view.editor_mut()
                        && e.mode() == frontier_editor::Mode::Review
                    {
                        match e.finish_review(Author::User { id: "demo".into() }, None) {
                            Ok(ev) => {
                                println!("{}", serde_json::to_string_pretty(&ev).expect("json"))
                            }
                            Err(err) => eprintln!("{err}"),
                        }
                    }
                }
                HostRequest::Leave => println!(
                    "leave requested (dirty={})",
                    self.view.editor().is_some_and(|e| e.is_dirty())
                ),
                HostRequest::Custom(id) => println!("custom command {id}"),
                HostRequest::Approval(ev) => println!("{ev:?}"),
                HostRequest::CaretMoved { .. } | HostRequest::Redraw => {}
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn render(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let Some(window) = &self.window else { return };
        let now = now_ms(self.start);
        let t0 = Instant::now();
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            _ => return,
        };
        let (w, h) = (gpu.config.width, gpu.config.height);
        let placement = Placement {
            scale: self.host_scale,
            ..Placement::fill(w, h)
        };
        let stats = self.view.prepare(
            &gpu.device,
            &gpu.queue,
            gpu.config.format,
            1,
            &placement,
            now,
        );
        let view = frame.texture.create_view(&Default::default());
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor demo"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.view.render(&mut pass);
        }
        gpu.queue.submit([enc.finish()]);
        window.pre_present_notify();
        gpu.queue.present(frame);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        self.frame_times.push(ms);
        if self.frame_times.len().is_multiple_of(120) {
            let mut v = self.frame_times.clone();
            v.sort_by(|a, b| a.partial_cmp(b).expect("ord"));
            let p50 = v[v.len() / 2];
            let p95 = v[v.len() * 95 / 100];
            window.set_title(&format!(
                "frontier editor — rows {} glyphs {} build {:.2} ms | frame p50 {p50:.2} ms p95 {p95:.2} ms | zoom {:.2}",
                stats.rows, stats.glyphs, stats.build_ms, self.host_scale
            ));
            self.frame_times.clear();
        }
        if self.view.needs_redraw(now) {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("frontier editor")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("device");
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        let dpr = window.scale_factor() as f32;
        self.view
            .resize(size.width as f32 / dpr, size.height as f32 / dpr, dpr);
        self.view.handle(InputEvent::FocusIn, 0);
        self.gpu = Some(Gpu {
            device,
            queue,
            surface,
            config,
        });
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let now = now_ms(self.start);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let (Some(g), Some(w)) = (&mut self.gpu, &self.window) {
                    g.config.width = size.width.max(1);
                    g.config.height = size.height.max(1);
                    g.surface.configure(&g.device, &g.config);
                    let dpr = w.scale_factor() as f32;
                    self.view
                        .resize(size.width as f32 / dpr, size.height as f32 / dpr, dpr);
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                let mac = cfg!(target_os = "macos");
                self.mods = Modifiers {
                    shift: s.shift_key(),
                    alt: s.alt_key(),
                    primary: if mac { s.super_key() } else { s.control_key() },
                    ctrl: if mac { s.control_key() } else { false },
                };
            }
            WindowEvent::CursorMoved { position, .. } => {
                let dpr = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0) as f32;
                let r = self.view.handle(
                    InputEvent::PointerMove {
                        x: position.x as f32 / dpr,
                        y: position.y as f32 / dpr,
                    },
                    now,
                );
                self.last_cursor = (position.x as f32 / dpr, position.y as f32 / dpr);
                self.requests(r);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.last_cursor;
                let ev = if state == ElementState::Pressed {
                    let clicks = if self.last_click.0.elapsed().as_millis() < 400 {
                        (self.last_click.1 % 3) + 1
                    } else {
                        1
                    };
                    self.last_click = (Instant::now(), clicks);
                    InputEvent::PointerDown {
                        x,
                        y,
                        button: if button == MouseButton::Left { 0 } else { 1 },
                        clicks,
                        mods: self.mods,
                    }
                } else {
                    InputEvent::PointerUp { x, y }
                };
                let r = self.view.handle(ev, now);
                self.requests(r);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 16.0, -y * 16.0),
                    MouseScrollDelta::PixelDelta(p) => (-p.x as f32, -p.y as f32),
                };
                let r = self.view.handle(
                    InputEvent::Wheel {
                        dx,
                        dy,
                        mods: self.mods,
                    },
                    now,
                );
                self.requests(r);
            }
            WindowEvent::Ime(ime) => {
                let ev = match ime {
                    winit::event::Ime::Enabled => return,
                    winit::event::Ime::Preedit(s, _) => {
                        if s.is_empty() {
                            InputEvent::CompositionEnd(String::new())
                        } else {
                            InputEvent::CompositionUpdate(s)
                        }
                    }
                    winit::event::Ime::Commit(s) => InputEvent::CompositionEnd(s),
                    winit::event::Ime::Disabled => return,
                };
                let r = self.view.handle(ev, now);
                self.requests(r);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let ev = match &event.logical_key {
                    WKey::Named(n) => {
                        let code = match n {
                            NamedKey::ArrowLeft => KeyCode::Left,
                            NamedKey::ArrowRight => KeyCode::Right,
                            NamedKey::ArrowUp => KeyCode::Up,
                            NamedKey::ArrowDown => KeyCode::Down,
                            NamedKey::Enter => KeyCode::Enter,
                            NamedKey::Tab => KeyCode::Tab,
                            NamedKey::Backspace => KeyCode::Backspace,
                            NamedKey::Delete => KeyCode::Delete,
                            NamedKey::Escape => KeyCode::Escape,
                            NamedKey::Home => KeyCode::Home,
                            NamedKey::End => KeyCode::End,
                            NamedKey::PageUp => KeyCode::PageUp,
                            NamedKey::PageDown => KeyCode::PageDown,
                            NamedKey::Space => KeyCode::Char(' '),
                            NamedKey::F2 => {
                                let t = if self.view.theme().name == "dark" {
                                    Theme::light()
                                } else {
                                    Theme::dark()
                                };
                                self.view.set_theme(t);
                                self.requests(vec![HostRequest::Redraw]);
                                return;
                            }
                            NamedKey::F3 => {
                                self.view.config.diff_mode =
                                    if self.view.config.diff_mode == DiffMode::Inline {
                                        DiffMode::SideBySide
                                    } else {
                                        DiffMode::Inline
                                    };
                                self.requests(vec![HostRequest::Redraw]);
                                return;
                            }
                            NamedKey::F5 => {
                                self.host_scale = if self.host_scale > 1.4 {
                                    0.6
                                } else {
                                    self.host_scale + 0.3
                                };
                                self.requests(vec![HostRequest::Redraw]);
                                return;
                            }
                            _ => return,
                        };
                        InputEvent::Key(Key::new(code, self.mods))
                    }
                    WKey::Character(s) => {
                        if self.mods.primary || self.mods.alt || self.mods.ctrl {
                            let c = s.chars().next().unwrap_or(' ');
                            InputEvent::Key(Key::ch(c, self.mods))
                        } else {
                            match &event.text {
                                Some(t) => InputEvent::Text(t.to_string()),
                                None => InputEvent::Text(s.to_string()),
                            }
                        }
                    }
                    _ => return,
                };
                let r = self.view.handle(ev, now);
                self.requests(r);
            }
            _ => {}
        }
    }
}

/// Headless: render N frames into an offscreen texture and print timings.
fn bench(args: &[String]) {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    println!("adapter: {}", adapter.get_info().name);
    let (w, h) = (2200u32, 1440u32); // 1100×720 logical at 2x
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());
    let run = |label: &str,
               view: &mut EditorView,
               frames: usize,
               per_frame: &mut dyn FnMut(&mut EditorView, usize)| {
        let mut times = Vec::with_capacity(frames);
        let mut build = Vec::with_capacity(frames);
        for i in 0..frames {
            per_frame(view, i);
            let t0 = Instant::now();
            let stats = view.prepare(
                &device,
                &queue,
                format,
                1,
                &Placement::fill(w, h),
                (i * 16) as u64,
            );
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                view.render(&mut pass);
            }
            queue.submit([enc.finish()]);
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            times.push(t0.elapsed().as_secs_f64() * 1e3);
            build.push(stats.build_ms);
        }
        times.sort_by(|a, b| a.partial_cmp(b).expect("ord"));
        build.sort_by(|a, b| a.partial_cmp(b).expect("ord"));
        println!(
            "{label:<44} frame p50 {:>7.3} ms  p95 {:>7.3} ms  (build p50 {:>6.3} ms)  first {:>7.3} ms",
            times[times.len() / 2],
            times[times.len() * 95 / 100],
            build[build.len() / 2],
            times.first().copied().unwrap_or(0.0)
        );
    };
    let dpr = 2.0;
    // 1 MB config: first paint
    let mut cfg = String::new();
    for i in 0..28_000 {
        if i % 10 == 0 {
            cfg.push_str(&format!("section_{i}:\n"))
        } else {
            cfg.push_str(&format!("  key_{i}: value_{i}  # comment {i}\n"))
        }
    }
    let mut view = EditorView::new(ViewConfig::default(), Theme::dark());
    view.resize(w as f32 / dpr, h as f32 / dpr, dpr);
    let t = Instant::now();
    view.warm_up(&device, &queue, format, 1);
    println!(
        "{:<44} {:.1} ms (pipeline + ASCII atlas, at mount)",
        "warm-up before any document",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    view.open("bench.yaml", &cfg);
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    view.prepare(&device, &queue, format, 1, &Placement::fill(w, h), 0);
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        view.render(&mut pass);
    }
    queue.submit([enc.finish()]);
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    println!(
        "{:<44} open {open_ms:.1} ms + first frame {:.1} ms (after warm-up)",
        "1 MB YAML first paint",
        t.elapsed().as_secs_f64() * 1e3
    );
    view.handle(InputEvent::FocusIn, 0);
    run("1 MB YAML steady frame", &mut view, 120, &mut |_, _| {});
    run(
        "1 MB YAML typing (key → frame)",
        &mut view,
        120,
        &mut |v, i| {
            v.handle(
                InputEvent::Text(if i % 7 == 0 { "\n".into() } else { "x".into() }),
                (i * 16) as u64,
            );
        },
    );
    run(
        "1 MB YAML scrolling (wheel → frame)",
        &mut view,
        240,
        &mut |v, i| {
            v.handle(
                InputEvent::Wheel {
                    dx: 0.0,
                    dy: 240.0,
                    mods: Modifiers::NONE,
                },
                (i * 16) as u64,
            );
        },
    );
    run(
        "1 MB YAML zoomed host transform ×1.7",
        &mut view,
        60,
        &mut |v, i| {
            v.set_zoom(1.7);
            if i == 0 {
                v.scroll_to_row(100.0, true);
            }
        },
    );
    view.set_zoom(1.0);
    // review mode
    let proposed = cfg
        .replace("value_15 ", "changed_15 ")
        .replace("key_27:", "key_27_renamed:");
    view.editor_mut().expect("e").review(&proposed);
    run(
        "1 MB YAML review (inline diff) frame",
        &mut view,
        60,
        &mut |_, _| {},
    );
    // 5M-line log
    let t = Instant::now();
    let mut log = EditorView::new(ViewConfig::default(), Theme::dark());
    log.resize(w as f32 / dpr, h as f32 / dpr, dpr);
    log.open_log(
        Box::new(SyntheticLog {
            line_count: 5_000_000,
        }),
        Language::Log,
    )
    .expect("log");
    println!(
        "{:<44} open {:.1} ms",
        "5M-line log",
        t.elapsed().as_secs_f64() * 1e3
    );
    run(
        "5M-line log scrolling 4 rows/frame",
        &mut log,
        240,
        &mut |v, i| {
            v.scroll_to_row(i as f64 * 4.0, true);
        },
    );
    run("5M-line log random jumps", &mut log, 120, &mut |v, i| {
        v.scroll_to_row(((i as u64 * 2_654_435_761) % 4_999_000) as f64, true);
    });
    println!(
        "{:<44} {:.1} MB",
        "5M-line log resident (index + chunk cache)",
        log.log().expect("log").memory_bytes() as f64 / 1e6
    );
    let s = log.stats();
    println!(
        "{:<44} glyphs {} instances {} atlas {} KB",
        "5M-line log frame content",
        s.glyphs,
        s.instances,
        s.atlas_bytes / 1024
    );
    let _ = args;
}

fn main() {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--bench") {
        bench(&args);
        return;
    }
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App {
        window: None,
        gpu: None,
        view: EditorView::new(ViewConfig::default(), Theme::dark()),
        start: Instant::now(),
        mods: Modifiers::NONE,
        clipboard: String::new(),
        host_scale: 1.0,
        mode: Mode::File,
        last_click: (Instant::now(), 0),
        frame_times: Vec::new(),
        last_cursor: (0.0, 0.0),
    };
    app.load(&args);
    event_loop.run_app(&mut app).expect("run");
}
