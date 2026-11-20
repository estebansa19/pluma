//! GPU-free window: egui is rasterized on the CPU and copied to the window with GDI.
//!
//! With OpenGL/DirectX the window loaded the graphics card driver (~130 MB of RAM just to
//! paint a few rectangles). This triangle rasterizer is small, the window is under one
//! megapixel and it only redraws when something changes.

use egui::epaint::{ClippedPrimitive, ImageData, ImageDelta, Mesh, Primitive, TextureId, Vertex};
use egui::{Color32, Context, ViewportId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, ReleaseDC, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Icon, Window, WindowId};

pub trait AppLogic {
    fn update(&mut self, ctx: &Context);
}

pub struct Options {
    pub title: &'static str,
    pub size: (f64, f64),
    pub min_size: (f64, f64),
    pub icon: Option<(Vec<u8>, u32)>,
    /// Title bar color (Windows 11), as 0xRRGGBB.
    pub caption: u32,
    pub clear: Color32,
}

#[derive(Debug)]
enum Wake {
    Now,
    After(Duration),
}

pub fn run<A: AppLogic + 'static>(opts: Options, make_app: impl FnOnce(&Context) -> A) -> Result<(), String> {
    let event_loop = EventLoop::<Wake>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let ctx = Context::default();
    ctx.set_request_repaint_callback(move |info| {
        let _ = proxy.send_event(if info.delay.is_zero() { Wake::Now } else { Wake::After(info.delay) });
    });
    let app = make_app(&ctx);
    let mut shell = Shell {
        renderer: Renderer::new(opts.clear),
        opts,
        ctx,
        app,
        window: None,
        state: None,
        next_repaint: None,
        shown: false,
    };
    event_loop.run_app(&mut shell).map_err(|e| e.to_string())
}

struct Shell<A> {
    opts: Options,
    ctx: Context,
    app: A,
    window: Option<Arc<Window>>,
    state: Option<egui_winit::State>,
    renderer: Renderer,
    next_repaint: Option<Instant>,
    shown: bool,
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
        _ => None,
    }
}

impl<A: AppLogic> Shell<A> {
    fn schedule(&mut self, delay: Duration) {
        if delay == Duration::MAX {
            return;
        }
        let t = Instant::now() + delay;
        self.next_repaint = Some(self.next_repaint.map_or(t, |n| n.min(t)));
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let Some(state) = self.state.as_mut() else { return };
        let raw = state.take_egui_input(&window);
        let app = &mut self.app;
        let out = self.ctx.run(raw, |ctx| app.update(ctx));
        state.handle_platform_output(&window, out.platform_output);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let size = window.inner_size();
        self.renderer.render(size.width as usize, size.height as usize, out.pixels_per_point, &out.textures_delta.set, &prims);
        for id in &out.textures_delta.free {
            self.renderer.textures.remove(id);
        }
        if let Some(hwnd) = hwnd_of(&window) {
            self.renderer.present(hwnd);
        }
        if !self.shown {
            self.shown = true;
            window.set_visible(true);
        }
        let delay = out.viewport_output.get(&ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        if delay.is_zero() {
            window.request_redraw();
        } else {
            self.schedule(delay);
        }
    }
}

impl<A: AppLogic> ApplicationHandler<Wake> for Shell<A> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title(self.opts.title)
            .with_inner_size(LogicalSize::new(self.opts.size.0, self.opts.size.1))
            .with_min_inner_size(LogicalSize::new(self.opts.min_size.0, self.opts.min_size.1))
            .with_theme(Some(winit::window::Theme::Dark))
            .with_visible(false);
        if let Some((rgba, side)) = self.opts.icon.take() {
            attrs = attrs.with_window_icon(Icon::from_rgba(rgba, side, side).ok());
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                crate::sys::error_box(&format!("Couldn't create the window: {e}"));
                el.exit();
                return;
            }
        };
        if let Some(m) = window.current_monitor().or_else(|| window.primary_monitor()) {
            let (ms, mp, ws) = (m.size(), m.position(), window.outer_size());
            window.set_outer_position(PhysicalPosition::new(
                mp.x + (ms.width as i32 - ws.width as i32) / 2,
                mp.y + (ms.height as i32 - ws.height as i32) / 2,
            ));
        }
        if let Some(hwnd) = hwnd_of(&window) {
            // Dark title bar matching the background color (Windows 11; ignored on 10).
            let c = self.opts.caption;
            let colorref: u32 = ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF);
            let dark: i32 = 1;
            unsafe {
                DwmSetWindowAttribute(hwnd, 20, &dark as *const i32 as *const _, 4);
                DwmSetWindowAttribute(hwnd, 35, &colorref as *const u32 as *const _, 4);
                DwmSetWindowAttribute(hwnd, 34, &colorref as *const u32 as *const _, 4);
            }
        }
        let state = egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            Some(winit::window::Theme::Dark),
            Some(1024),
        );
        self.window = Some(window);
        self.state = Some(state);
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(state)) = (self.window.clone(), self.state.as_mut()) else { return };
        let resp = state.on_window_event(&window, &event);
        match event {
            WindowEvent::CloseRequested => {
                el.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => window.request_redraw(),
            // In the background there's no need to keep everything in RAM: Windows pages it back in.
            WindowEvent::Focused(false) => crate::sys::trim_memory(),
            _ => {}
        }
        if resp.repaint {
            window.request_redraw();
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::Now => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Wake::After(d) => self.schedule(d),
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match self.next_repaint {
            Some(t) if Instant::now() >= t => {
                self.next_repaint = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

// ---------- rasterizer ----------

enum Texels {
    /// Font atlas: coverage (premultiplied white).
    Alpha(Vec<u8>),
    Rgba(Vec<Color32>),
}

struct Texture {
    w: usize,
    h: usize,
    texels: Texels,
}

impl Texture {
    #[inline]
    fn texel(&self, x: usize, y: usize) -> [u32; 4] {
        let i = y * self.w + x;
        match &self.texels {
            Texels::Alpha(a) => {
                let v = a[i] as u32;
                [v, v, v, v]
            }
            Texels::Rgba(c) => {
                let c = c[i];
                [c.r() as u32, c.g() as u32, c.b() as u32, c.a() as u32]
            }
        }
    }

    /// Bilinear sampling at normalized UV coordinates; premultiplied channels 0..=255.
    fn sample(&self, u: f32, v: f32) -> [u32; 4] {
        let x = (u * self.w as f32 - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (v * self.h as f32 - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let fx = ((x - x0 as f32) * 256.0) as u32;
        let fy = ((y - y0 as f32) * 256.0) as u32;
        let (a, b, c, d) = (self.texel(x0, y0), self.texel(x1, y0), self.texel(x0, y1), self.texel(x1, y1));
        let mut out = [0u32; 4];
        for k in 0..4 {
            let top = a[k] * (256 - fx) + b[k] * fx;
            let bot = c[k] * (256 - fx) + d[k] * fx;
            out[k] = (top * (256 - fy) + bot * fy + (1 << 15)) >> 16;
        }
        out
    }
}

struct Renderer {
    w: usize,
    h: usize,
    px: Vec<u32>,
    clear: u32,
    textures: HashMap<TextureId, Texture>,
    /// Fingerprint of the last frame: if the next one is identical it isn't rasterized again.
    last: u64,
}

fn frame_hash(w: usize, h: usize, ppp: f32, prims: &[ClippedPrimitive]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut s = std::collections::hash_map::DefaultHasher::new();
    (w, h, ppp.to_bits()).hash(&mut s);
    for p in prims {
        let r = p.clip_rect;
        (r.min.x.to_bits(), r.min.y.to_bits(), r.max.x.to_bits(), r.max.y.to_bits()).hash(&mut s);
        if let Primitive::Mesh(m) = &p.primitive {
            m.texture_id.hash(&mut s);
            m.indices.hash(&mut s);
            for v in &m.vertices {
                (v.pos.x.to_bits(), v.pos.y.to_bits(), v.uv.x.to_bits(), v.uv.y.to_bits(), v.color.to_array()).hash(&mut s);
            }
        }
    }
    s.finish()
}

const SUB: i64 = 16; // sub-pixels per pixel

#[inline]
fn edge(a: (i64, i64), b: (i64, i64), p: (i64, i64)) -> i64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

#[inline]
fn is_top_left(a: (i64, i64), b: (i64, i64)) -> bool {
    (a.1 == b.1 && b.0 > a.0) || b.1 < a.1
}

#[inline]
fn blend(dst: &mut u32, s: [u32; 4]) {
    let a = s[3];
    if a == 0 && s[0] == 0 && s[1] == 0 && s[2] == 0 {
        return;
    }
    let inv = 255 - a.min(255);
    let d = *dst;
    let ch = |shift: u32, src: u32| -> u32 {
        let dc = (d >> shift) & 0xFF;
        (src + (dc * inv + 127) / 255).min(255)
    };
    *dst = (ch(16, s[0]) << 16) | (ch(8, s[1]) << 8) | ch(0, s[2]);
}

#[inline]
fn modulate(t: [u32; 4], c: [u32; 4]) -> [u32; 4] {
    [(t[0] * c[0] + 127) / 255, (t[1] * c[1] + 127) / 255, (t[2] * c[2] + 127) / 255, (t[3] * c[3] + 127) / 255]
}

fn rgba(c: Color32) -> [u32; 4] {
    [c.r() as u32, c.g() as u32, c.b() as u32, c.a() as u32]
}

impl Renderer {
    fn new(clear: Color32) -> Renderer {
        Renderer {
            w: 0,
            h: 0,
            px: Vec::new(),
            clear: ((clear.r() as u32) << 16) | ((clear.g() as u32) << 8) | clear.b() as u32,
            textures: HashMap::new(),
            last: 0,
        }
    }

    fn set_texture(&mut self, id: TextureId, delta: &ImageDelta) {
        let (w, h) = (delta.image.width(), delta.image.height());
        let texels = match &delta.image {
            ImageData::Font(f) => Texels::Alpha(f.pixels.iter().map(|c| (c.powf(0.55) * 255.0 + 0.5) as u8).collect()),
            ImageData::Color(c) => Texels::Rgba(c.pixels.clone()),
        };
        match delta.pos {
            None => {
                self.textures.insert(id, Texture { w, h, texels });
            }
            Some([x0, y0]) => {
                let Some(t) = self.textures.get_mut(&id) else { return };
                for row in 0..h {
                    let (dy, di) = (y0 + row, (y0 + row) * t.w + x0);
                    if dy >= t.h || x0 + w > t.w {
                        continue;
                    }
                    match (&mut t.texels, &texels) {
                        (Texels::Alpha(dst), Texels::Alpha(src)) => dst[di..di + w].copy_from_slice(&src[row * w..row * w + w]),
                        (Texels::Rgba(dst), Texels::Rgba(src)) => dst[di..di + w].copy_from_slice(&src[row * w..row * w + w]),
                        _ => {}
                    }
                }
            }
        }
    }

    fn render(
        &mut self,
        w: usize,
        h: usize,
        ppp: f32,
        set: &[(TextureId, ImageDelta)],
        prims: &[ClippedPrimitive],
    ) {
        for (id, delta) in set {
            self.set_texture(*id, delta);
        }
        let hash = frame_hash(w, h, ppp, prims);
        if set.is_empty() && hash == self.last && w == self.w && h == self.h {
            return; // same frame as before: copying it to the window again is enough
        }
        self.last = hash;
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.px = vec![self.clear; w * h];
            self.px.shrink_to_fit();
        } else {
            self.px.fill(self.clear);
        }
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let clip = (
                ((p.clip_rect.min.x * ppp).round() as i64).clamp(0, w as i64),
                ((p.clip_rect.min.y * ppp).round() as i64).clamp(0, h as i64),
                ((p.clip_rect.max.x * ppp).round() as i64).clamp(0, w as i64),
                ((p.clip_rect.max.y * ppp).round() as i64).clamp(0, h as i64),
            );
            if clip.0 >= clip.2 || clip.1 >= clip.3 {
                continue;
            }
            let Some(tex) = self.textures.get(&mesh.texture_id) else { continue };
            draw_mesh(&mut self.px, self.w, tex, mesh, ppp, clip);
        }
    }
}

fn draw_mesh(px: &mut [u32], stride: usize, tex: &Texture, mesh: &Mesh, ppp: f32, clip: (i64, i64, i64, i64)) {
    for tri in mesh.indices.chunks_exact(3) {
        let (Some(a), Some(b), Some(c)) = (
            mesh.vertices.get(tri[0] as usize),
            mesh.vertices.get(tri[1] as usize),
            mesh.vertices.get(tri[2] as usize),
        ) else {
            continue;
        };
        triangle(px, stride, [a, b, c], ppp, clip, tex);
    }
}

fn triangle(px: &mut [u32], stride: usize, v: [&Vertex; 3], ppp: f32, clip: (i64, i64, i64, i64), tex: &Texture) {
    {
        let fix = |v: &Vertex| (((v.pos.x * ppp) * SUB as f32).round() as i64, ((v.pos.y * ppp) * SUB as f32).round() as i64);
        let (p0, mut p1, mut p2) = (fix(v[0]), fix(v[1]), fix(v[2]));
        let (v0, mut v1, mut v2) = (v[0], v[1], v[2]);
        let mut area = edge(p0, p1, p2);
        if area == 0 {
            return;
        }
        if area < 0 {
            std::mem::swap(&mut p1, &mut p2);
            std::mem::swap(&mut v1, &mut v2);
            area = -area;
        }
        let min_x = (p0.0.min(p1.0).min(p2.0) / SUB).max(clip.0);
        let max_x = ((p0.0.max(p1.0).max(p2.0) + SUB - 1) / SUB).min(clip.2);
        let min_y = (p0.1.min(p1.1).min(p2.1) / SUB).max(clip.1);
        let max_y = ((p0.1.max(p1.1).max(p2.1) + SUB - 1) / SUB).min(clip.3);
        if min_x >= max_x || min_y >= max_y {
            return;
        }

        // If the whole triangle uses the same color and texel, the color is constant.
        let same_uv = v0.uv == v1.uv && v1.uv == v2.uv;
        let same_col = v0.color == v1.color && v1.color == v2.color;
        let constant = if same_uv && same_col {
            Some(modulate(tex.sample(v0.uv.x, v0.uv.y), rgba(v0.color)))
        } else {
            None
        };
        let flat_tex = same_uv.then(|| tex.sample(v0.uv.x, v0.uv.y));

        let bias = |a, b| if is_top_left(a, b) { 0 } else { -1 };
        let (b0, b1, b2) = (bias(p1, p2), bias(p2, p0), bias(p0, p1));
        // Per-pixel increments of the edge functions.
        let (dx0, dy0) = (-(p2.1 - p1.1) * SUB, (p2.0 - p1.0) * SUB);
        let (dx1, dy1) = (-(p0.1 - p2.1) * SUB, (p0.0 - p2.0) * SUB);
        let (dx2, dy2) = (-(p1.1 - p0.1) * SUB, (p1.0 - p0.0) * SUB);
        let start = (min_x * SUB + SUB / 2, min_y * SUB + SUB / 2);
        let mut r0 = edge(p1, p2, start);
        let mut r1 = edge(p2, p0, start);
        let mut r2 = edge(p0, p1, start);
        let inv_area = 1.0 / area as f32;
        let (c0, c1, c2) = (rgba(v0.color), rgba(v1.color), rgba(v2.color));

        for y in min_y..max_y {
            let (mut w0, mut w1, mut w2) = (r0, r1, r2);
            let row = y as usize * stride;
            for x in min_x..max_x {
                if w0 + b0 >= 0 && w1 + b1 >= 0 && w2 + b2 >= 0 {
                    let src = match constant {
                        Some(s) => s,
                        None => {
                            let (l0, l1, l2) = (w0 as f32 * inv_area, w1 as f32 * inv_area, w2 as f32 * inv_area);
                            let col = if same_col {
                                c0
                            } else {
                                let m = |k: usize| (c0[k] as f32 * l0 + c1[k] as f32 * l1 + c2[k] as f32 * l2 + 0.5) as u32;
                                [m(0), m(1), m(2), m(3)]
                            };
                            let t = match flat_tex {
                                Some(t) => t,
                                None => {
                                    let u = v0.uv.x * l0 + v1.uv.x * l1 + v2.uv.x * l2;
                                    let vv = v0.uv.y * l0 + v1.uv.y * l1 + v2.uv.y * l2;
                                    tex.sample(u, vv)
                                }
                            };
                            modulate(t, col)
                        }
                    };
                    blend(&mut px[row + x as usize], src);
                }
                w0 += dx0;
                w1 += dx1;
                w2 += dx2;
            }
            r0 += dy0;
            r1 += dy1;
            r2 += dy2;
        }
    }
}

impl Renderer {
    fn present(&self, hwnd: HWND) {
        if self.w == 0 || self.h == 0 {
            return;
        }
        unsafe {
            let hdc = GetDC(hwnd);
            if hdc.is_null() {
                return;
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = self.w as i32;
            bmi.bmiHeader.biHeight = -(self.h as i32); // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            SetDIBitsToDevice(
                hdc,
                0,
                0,
                self.w as u32,
                self.h as u32,
                0,
                0,
                0,
                self.h as u32,
                self.px.as_ptr() as *const _,
                &bmi,
                DIB_RGB_COLORS,
            );
            ReleaseDC(hwnd, hdc);
        }
    }
}
