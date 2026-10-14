// SPDX-License-Identifier: Apache-2.0
//! Browser host: a `<canvas>` rendered with WebGPU plus a hidden `<textarea>`
//! overlay that owns focus, so IME composition, dead keys, the clipboard and
//! screen readers all go through the platform text field. Exposed to JS as
//! `WebEditor` through wasm-bindgen; `demo/index.html` shows the wiring.

use crate::input::{HostRequest, InputEvent, Key, KeyCode, Modifiers};
use crate::render::Placement;
use crate::theme::Theme;
use crate::view::{DiffMode, EditorView, ViewConfig};
use frontier_editor::provenance::Author;
use frontier_editor::source::SyntheticLog;
use frontier_editor::syntax::Language;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, HtmlElement, HtmlTextAreaElement};

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

struct Host {
    view: EditorView,
    container: HtmlElement,
    canvas: HtmlCanvasElement,
    textarea: HtmlTextAreaElement,
    gpu: Option<Gpu>,
    on_event: Option<js_sys::Function>,
    dirty: bool,
    host_scale: f32,
    last_size: (u32, u32),
    composing: bool,
    frames: u64,
    frame_ms_ewma: f64,
}

fn now_ms() -> u64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now() as u64)
        .unwrap_or(0)
}

fn mods(shift: bool, alt: bool, ctrl: bool, meta: bool) -> Modifiers {
    let mac = web_sys::window()
        .map(|w| {
            w.navigator()
                .platform()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("mac")
        })
        .unwrap_or(false);
    Modifiers {
        shift,
        alt,
        primary: if mac { meta } else { ctrl },
        ctrl: if mac { ctrl } else { false },
    }
}

fn key_from_event(e: &web_sys::KeyboardEvent) -> Option<Key> {
    let m = mods(e.shift_key(), e.alt_key(), e.ctrl_key(), e.meta_key());
    let k = e.key();
    let code = match k.as_str() {
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Escape" => KeyCode::Escape,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        f if f.len() > 1 && f.starts_with('F') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            KeyCode::F(f[1..].parse().ok()?)
        }
        s => {
            let mut it = s.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            // plain characters reach the editor through the textarea's `input` event
            if !(m.primary || m.alt || m.ctrl) {
                return None;
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
    };
    Some(Key { code, mods: m })
}

impl Host {
    fn emit(&self, kind: &str, payload: JsValue) {
        if let Some(f) = &self.on_event {
            let obj = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&obj, &"type".into(), &kind.into());
            let _ = js_sys::Reflect::set(&obj, &"data".into(), &payload);
            let _ = f.call1(&JsValue::NULL, &obj);
        }
    }

    fn apply_requests(&mut self, reqs: Vec<HostRequest>) {
        for r in reqs {
            match r {
                HostRequest::Redraw => self.dirty = true,
                HostRequest::CaretMoved { x, y, w, h } => {
                    let st = self.textarea.style();
                    let _ = st.set_property("left", &format!("{x}px"));
                    let _ = st.set_property("top", &format!("{y}px"));
                    let _ = st.set_property("width", &format!("{}px", w.max(1.0)));
                    let _ = st.set_property("height", &format!("{}px", h.max(1.0)));
                }
                HostRequest::Copy(text) => {
                    if let Some(cb) = web_sys::window().map(|w| w.navigator().clipboard()) {
                        let _ = cb.write_text(&text);
                    }
                }
                HostRequest::Paste => {
                    // the textarea's `paste` event delivers the text; nothing to do here
                }
                HostRequest::Save => self.emit("save", JsValue::NULL),
                HostRequest::Apply => self.emit("apply", JsValue::NULL),
                HostRequest::Leave => self.emit("leave", JsValue::NULL),
                HostRequest::Custom(id) => self.emit("command", id.into()),
                HostRequest::Approval(ev) => {
                    self.emit("approval", serde_wasm_json(&ev));
                }
            }
        }
    }

    fn sync_size(&mut self) {
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0) as f32;
        let rect = self.container.get_bounding_client_rect();
        let (w, h) = (rect.width() as f32, rect.height() as f32);
        let (pw, ph) = ((w * dpr).round() as u32, (h * dpr).round() as u32);
        if (pw, ph) != self.last_size && pw > 0 && ph > 0 {
            self.last_size = (pw, ph);
            self.canvas.set_width(pw);
            self.canvas.set_height(ph);
            let _ = self.canvas.style().set_property("width", &format!("{w}px"));
            let _ = self
                .canvas
                .style()
                .set_property("height", &format!("{h}px"));
            if let Some(g) = &mut self.gpu {
                g.config.width = pw;
                g.config.height = ph;
                g.surface.configure(&g.device, &g.config);
            }
            self.dirty = true;
        }
        self.view.resize(w, h, dpr);
    }

    fn frame(&mut self) {
        self.sync_size();
        let now = now_ms();
        if !(self.dirty || self.view.needs_redraw(now)) {
            return;
        }
        self.dirty = false;
        let Some(g) = &self.gpu else { return };
        let t0 = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        let frame = match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                g.surface.configure(&g.device, &g.config);
                return;
            }
            _ => return,
        };
        let (w, h) = (g.config.width, g.config.height);
        let placement = Placement {
            scale: self.host_scale,
            ..Placement::fill(w, h)
        };
        self.view
            .prepare(&g.device, &g.queue, g.config.format, 1, &placement, now);
        let g = self.gpu.as_ref().expect("gpu");
        let view = frame.texture.create_view(&Default::default());
        let mut enc = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("editor"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
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
        g.queue.submit([enc.finish()]);
        g.queue.present(frame);
        let t1 = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        self.frames += 1;
        self.frame_ms_ewma = if self.frames == 1 {
            t1 - t0
        } else {
            self.frame_ms_ewma * 0.9 + (t1 - t0) * 0.1
        };
    }
}

fn serde_wasm_json<T: serde::Serialize>(v: &T) -> JsValue {
    match serde_json::to_string(v) {
        Ok(s) => js_sys::JSON::parse(&s).unwrap_or(JsValue::NULL),
        Err(_) => JsValue::NULL,
    }
}

/// The `<frontier-editor>` host object. One per editor surface.
#[wasm_bindgen]
pub struct WebEditor {
    host: Rc<RefCell<Host>>,
}

#[wasm_bindgen]
impl WebEditor {
    /// Mounts a canvas and the hidden input overlay inside `container`.
    #[wasm_bindgen(constructor)]
    pub fn new(container: HtmlElement, theme: &str) -> Result<WebEditor, JsValue> {
        console_error_panic_hook::set_once();
        let document = web_sys::window()
            .ok_or("no window")?
            .document()
            .ok_or("no document")?;
        let canvas: HtmlCanvasElement = document.create_element("canvas")?.dyn_into()?;
        let textarea: HtmlTextAreaElement = document.create_element("textarea")?.dyn_into()?;
        // the overlay is positioned inside the container; keep a host-set position
        let positioned = web_sys::window()
            .and_then(|w| w.get_computed_style(&container).ok().flatten())
            .and_then(|cs| cs.get_property_value("position").ok())
            .is_some_and(|p| p != "static" && !p.is_empty());
        if !positioned {
            let _ = container.style().set_property("position", "relative");
        }
        let _ = container.style().set_property("overflow", "hidden");
        let _ = canvas.style().set_property("display", "block");
        let _ = canvas.style().set_property("touch-action", "none");
        let st = textarea.style();
        for (k, v) in [
            ("position", "absolute"),
            ("left", "0px"),
            ("top", "0px"),
            ("width", "1px"),
            ("height", "1px"),
            ("opacity", "0.01"),
            ("padding", "0"),
            ("border", "0"),
            ("outline", "none"),
            ("resize", "none"),
            ("overflow", "hidden"),
            ("color", "transparent"),
            ("background", "transparent"),
            ("caret-color", "transparent"),
            ("z-index", "1"),
            ("pointer-events", "none"),
        ] {
            let _ = st.set_property(k, v);
        }
        textarea.set_attribute("aria-label", "editor")?;
        textarea.set_attribute("autocapitalize", "off")?;
        textarea.set_attribute("autocomplete", "off")?;
        textarea.set_attribute("spellcheck", "false")?;
        textarea.set_attribute("data-frontier-editor-input", "")?;
        container.append_child(&canvas)?;
        container.append_child(&textarea)?;
        let mut view = EditorView::new(ViewConfig::default(), Theme::by_name(theme));
        view.resize(640.0, 400.0, 1.0);
        let host = Rc::new(RefCell::new(Host {
            view,
            container,
            canvas,
            textarea,
            gpu: None,
            on_event: None,
            dirty: true,
            host_scale: 1.0,
            last_size: (0, 0),
            composing: false,
            frames: 0,
            frame_ms_ewma: 0.0,
        }));
        let me = WebEditor { host };
        me.wire_events()?;
        me.start_loop();
        Ok(me)
    }

    /// Requests a WebGPU adapter for the canvas and configures the surface.
    pub async fn init_gpu(&self) -> Result<(), JsValue> {
        let canvas = self.host.borrow().canvas.clone();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter: {e}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("frontier-editor-view"),
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let (w, h) = (canvas.width().max(1), canvas.height().max(1));
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        let mut h = self.host.borrow_mut();
        h.gpu = Some(Gpu {
            device,
            queue,
            surface,
            config,
        });
        h.last_size = (0, 0);
        h.dirty = true;
        Ok(())
    }

    pub fn open(&self, path: &str, text: &str) {
        let mut h = self.host.borrow_mut();
        h.view.open(path, text);
        if let Some(mut e) = h.view.editor_mut() {
            e.use_registry(&frontier_editor::schema::SchemaRegistry::builtin());
        }
        h.dirty = true;
    }

    /// Opens a synthetic `lines`-line log through the chunked source (demo / bench).
    pub fn open_synthetic_log(&self, lines: u32) -> Result<(), JsValue> {
        let mut h = self.host.borrow_mut();
        h.view
            .open_log(
                Box::new(SyntheticLog {
                    line_count: lines as u64,
                }),
                Language::Log,
            )
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        h.dirty = true;
        Ok(())
    }

    pub fn review(&self, proposed: &str) {
        let mut h = self.host.borrow_mut();
        if let Some(mut e) = h.view.editor_mut() {
            e.review(proposed);
        }
        h.dirty = true;
    }

    /// Applies the accepted hunks; returns the ApprovalEvent as a JS object.
    pub fn finish_review(&self, approver: &str) -> JsValue {
        let mut h = self.host.borrow_mut();
        h.dirty = true;
        let Some(mut e) = h.view.editor_mut() else {
            return JsValue::NULL;
        };
        match e.finish_review(
            Author::User {
                id: approver.to_string(),
            },
            None,
        ) {
            Ok(ev) => serde_wasm_json(&ev),
            Err(_) => JsValue::NULL,
        }
    }

    /// Marks the text saved; returns the EditEvent as a JS object.
    pub fn save(&self, author: &str) -> JsValue {
        let mut h = self.host.borrow_mut();
        h.dirty = true;
        let Some(mut e) = h.view.editor_mut() else {
            return JsValue::NULL;
        };
        serde_wasm_json(&e.save(Author::User {
            id: author.to_string(),
        }))
    }

    pub fn text(&self) -> String {
        self.host
            .borrow()
            .view
            .editor()
            .map(|e| e.text())
            .unwrap_or_default()
    }

    pub fn is_dirty(&self) -> bool {
        self.host
            .borrow()
            .view
            .editor()
            .is_some_and(|e| e.is_dirty())
    }

    pub fn diagnostics(&self) -> JsValue {
        self.host
            .borrow()
            .view
            .editor()
            .map(|e| serde_wasm_json(&e.diagnostics()))
            .unwrap_or(JsValue::NULL)
    }

    pub fn set_theme(&self, name: &str) {
        let mut h = self.host.borrow_mut();
        h.view.set_theme(Theme::by_name(name));
        h.dirty = true;
    }

    pub fn set_diff_mode(&self, mode: &str) {
        let mut h = self.host.borrow_mut();
        h.view.config.diff_mode = if mode == "side-by-side" {
            DiffMode::SideBySide
        } else {
            DiffMode::Inline
        };
        h.dirty = true;
    }

    /// Host zoom (e.g. the board's camera scale): text is re-rasterised at it.
    pub fn set_transform(&self, scale: f32) {
        let mut h = self.host.borrow_mut();
        h.host_scale = scale.max(0.1);
        h.dirty = true;
    }

    pub fn set_search(&self, query: &str, regex: bool) -> u32 {
        let mut h = self.host.borrow_mut();
        h.dirty = true;
        let Some(mut e) = h.view.editor_mut() else {
            return 0;
        };
        e.set_search(
            query,
            &frontier_editor::search::SearchOptions {
                regex,
                ..Default::default()
            },
        )
        .unwrap_or(0) as u32
    }

    pub fn set_on_event(&self, f: js_sys::Function) {
        self.host.borrow_mut().on_event = Some(f);
    }

    pub fn focus(&self) {
        let h = self.host.borrow();
        let _ = h.textarea.focus();
    }

    pub fn redraw(&self) {
        self.host.borrow_mut().dirty = true;
    }

    pub fn stats(&self) -> JsValue {
        let h = self.host.borrow();
        let s = h.view.stats();
        let obj = js_sys::Object::new();
        for (k, v) in [
            ("rows", s.rows as f64),
            ("instances", s.instances as f64),
            ("glyphs", s.glyphs as f64),
            ("build_ms", s.build_ms),
            ("frame_ms", h.frame_ms_ewma),
            ("atlas_glyphs", s.atlas_glyphs as f64),
            ("atlas_kb", s.atlas_bytes as f64 / 1024.0),
            ("frames", h.frames as f64),
            ("scroll_row", h.view.scroll_row()),
        ] {
            let _ = js_sys::Reflect::set(&obj, &k.into(), &v.into());
        }
        obj.into()
    }

    pub fn scroll_to_row(&self, row: f64) {
        let mut h = self.host.borrow_mut();
        h.view.scroll_to_row(row, false);
        h.dirty = true;
    }

    fn start_loop(&self) {
        let host = self.host.clone();
        let f: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let g = f.clone();
        *g.borrow_mut() = Some(Closure::wrap(Box::new(move || {
            host.borrow_mut().frame();
            if let Some(w) = web_sys::window()
                && let Some(cb) = f.borrow().as_ref()
            {
                let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
            }
        }) as Box<dyn FnMut()>));
        if let Some(w) = web_sys::window()
            && let Some(cb) = g.borrow().as_ref()
        {
            let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }

    fn wire_events(&self) -> Result<(), JsValue> {
        let (canvas, textarea) = {
            let h = self.host.borrow();
            (h.canvas.clone(), h.textarea.clone())
        };
        macro_rules! listen {
            ($target:expr, $name:literal, $ty:ty, |$h:ident, $e:ident| $body:block) => {{
                let host = self.host.clone();
                let cb = Closure::wrap(Box::new(move |$e: $ty| {
                    #[allow(unused_mut)]
                    let mut $h = host.borrow_mut();
                    $body
                }) as Box<dyn FnMut($ty)>);
                $target.add_event_listener_with_callback($name, cb.as_ref().unchecked_ref())?;
                cb.forget();
            }};
        }
        let pointer_pos = |h: &Host, e: &web_sys::MouseEvent| {
            let r = h.canvas.get_bounding_client_rect();
            (
                (e.client_x() as f64 - r.left()) as f32,
                (e.client_y() as f64 - r.top()) as f32,
            )
        };
        listen!(canvas, "pointerdown", web_sys::PointerEvent, |h, e| {
            let (x, y) = pointer_pos(&h, &e);
            let m = mods(e.shift_key(), e.alt_key(), e.ctrl_key(), e.meta_key());
            let clicks = e.detail().clamp(1, 3) as u8;
            let r = h.view.handle(
                InputEvent::PointerDown {
                    x,
                    y,
                    button: e.button() as u8,
                    clicks,
                    mods: m,
                },
                now_ms(),
            );
            h.apply_requests(r);
            let _ = h.textarea.focus();
            let _ = h.canvas.set_pointer_capture(e.pointer_id());
            e.prevent_default();
        });
        listen!(canvas, "pointermove", web_sys::PointerEvent, |h, e| {
            let (x, y) = pointer_pos(&h, &e);
            let r = h.view.handle(InputEvent::PointerMove { x, y }, now_ms());
            h.apply_requests(r);
        });
        listen!(canvas, "pointerup", web_sys::PointerEvent, |h, e| {
            let (x, y) = pointer_pos(&h, &e);
            let r = h.view.handle(InputEvent::PointerUp { x, y }, now_ms());
            h.apply_requests(r);
        });
        listen!(canvas, "wheel", web_sys::WheelEvent, |h, e| {
            let k = match e.delta_mode() {
                1 => 16.0,
                2 => 400.0,
                _ => 1.0,
            };
            let m = mods(e.shift_key(), e.alt_key(), e.ctrl_key(), e.meta_key());
            let r = h.view.handle(
                InputEvent::Wheel {
                    dx: e.delta_x() as f32 * k,
                    dy: e.delta_y() as f32 * k,
                    mods: m,
                },
                now_ms(),
            );
            h.apply_requests(r);
            e.prevent_default();
        });
        listen!(textarea, "keydown", web_sys::KeyboardEvent, |h, e| {
            if h.composing || e.is_composing() {
                return;
            }
            let Some(key) = key_from_event(&e) else {
                return;
            };
            // native clipboard events must reach the textarea
            if key.mods.primary
                && matches!(
                    key.code,
                    KeyCode::Char('c') | KeyCode::Char('v') | KeyCode::Char('x')
                )
            {
                return;
            }
            let r = h.view.handle(InputEvent::Key(key), now_ms());
            h.apply_requests(r);
            e.prevent_default();
        });
        listen!(textarea, "input", web_sys::InputEvent, |h, e| {
            if h.composing || e.is_composing() {
                return;
            }
            let value = h.textarea.value();
            if !value.is_empty() {
                let r = h.view.handle(InputEvent::Text(value), now_ms());
                h.apply_requests(r);
                h.textarea.set_value("");
            }
        });
        listen!(
            textarea,
            "compositionstart",
            web_sys::CompositionEvent,
            |h, _e| {
                h.composing = true;
                let r = h.view.handle(InputEvent::CompositionStart, now_ms());
                h.apply_requests(r);
            }
        );
        listen!(
            textarea,
            "compositionupdate",
            web_sys::CompositionEvent,
            |h, e| {
                let r = h.view.handle(
                    InputEvent::CompositionUpdate(e.data().unwrap_or_default()),
                    now_ms(),
                );
                h.apply_requests(r);
            }
        );
        listen!(
            textarea,
            "compositionend",
            web_sys::CompositionEvent,
            |h, e| {
                h.composing = false;
                let r = h.view.handle(
                    InputEvent::CompositionEnd(e.data().unwrap_or_default()),
                    now_ms(),
                );
                h.apply_requests(r);
                h.textarea.set_value("");
            }
        );
        listen!(textarea, "copy", web_sys::ClipboardEvent, |h, e| {
            if let Some(dt) = e.clipboard_data() {
                let _ = dt.set_data("text/plain", &h.view.selected_text());
                e.prevent_default();
            }
        });
        listen!(textarea, "cut", web_sys::ClipboardEvent, |h, e| {
            if let Some(dt) = e.clipboard_data() {
                let _ = dt.set_data("text/plain", &h.view.selected_text());
                let r = h.view.handle(
                    InputEvent::Key(Key::parse("delete").expect("key")),
                    now_ms(),
                );
                h.apply_requests(r);
                e.prevent_default();
            }
        });
        listen!(textarea, "paste", web_sys::ClipboardEvent, |h, e| {
            if let Some(dt) = e.clipboard_data() {
                let text = dt.get_data("text/plain").unwrap_or_default();
                let r = h.view.handle(InputEvent::Text(text), now_ms());
                h.apply_requests(r);
                e.prevent_default();
            }
        });
        listen!(textarea, "focus", web_sys::FocusEvent, |h, _e| {
            let r = h.view.handle(InputEvent::FocusIn, now_ms());
            h.apply_requests(r);
        });
        listen!(textarea, "blur", web_sys::FocusEvent, |h, _e| {
            let r = h.view.handle(InputEvent::FocusOut, now_ms());
            h.apply_requests(r);
        });
        Ok(())
    }
}
