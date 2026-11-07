//! Real Gaussian blur, which egui doesn't have: CSS's `filter: blur()` and `backdrop-filter: blur()`,
//! drawn on the GPU in egui_glow paint callbacks. A job draws its source into a texture of its own (a
//! mesh such as a line of text, or what is already on screen under it), blurs that across and then
//! down, and lays it back where it came from: tinted, saturated (CSS `saturate()`) and, for frosted
//! glass or a shadow, kept inside or outside a rounded rect. A wide blur works on a smaller copy,
//! which the blur hides.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui_glow;
use eframe::glow::{self, HasContext as _};
use egui::{Color32, Galley, Mesh, PaintCallback, PaintCallbackInfo, Pos2, Rect, epaint::Vertex};

/// What a job blurs.
pub enum Source {
    /// A mesh in points with egui's vertices. `texels`: its UVs count texels of its texture (a
    /// galley's own rows, see [`galley_mesh`]) instead of going 0-1.
    Mesh { mesh: Mesh, texels: bool },
    /// What has been drawn under the job's area so far (frosted glass).
    Backdrop,
}

/// Where a job's result shows: all of it, or only inside or outside a rounded rect (points, and its
/// corner radius).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    All,
    Inside(Rect, f32),
    Outside(Rect, f32),
}

pub struct Job {
    pub source: Source,
    /// The area worked on, in points: the source and the room its blur spreads into.
    pub area: Rect,
    /// The blur's standard deviation in points (CSS `blur(r)` is `r`).
    pub sigma: f32,
    /// Multiplies the result, premultiplied (white text at opacity `o`: `[o; 4]`).
    pub tint: [f32; 4],
    /// CSS `saturate()`: 1 leaves the colours as they are.
    pub saturate: f32,
    pub shape: Shape,
}

/// Set once the GPU couldn't build the blur: callers then draw without it.
static BROKEN: AtomicBool = AtomicBool::new(false);

/// Whether the blur couldn't be built on this GPU (callers draw their things unblurred then).
pub fn broken() -> bool {
    BROKEN.load(Ordering::Relaxed)
}

/// Adds `job` to what `painter` draws, in order with its other shapes.
pub fn paint(painter: &egui::Painter, job: Job) {
    if !job.area.is_positive() || job.tint.iter().all(|&t| t <= 0.0) || broken() {
        return;
    }
    let area = job.area;
    let callback = egui_glow::CallbackFn::new(move |info, painter| {
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: egui_glow runs callbacks with its GL context current, and sets its own state up
        // again after each one.
        unsafe { run(&mut state, painter, &info, &job) };
    });
    painter.add(PaintCallback { rect: area, callback: Arc::new(callback) });
}

/// `galley` drawn at `pos`, as one mesh of its rows' own vertices (UVs in texels of the font
/// texture: [`Source::Mesh`] with `texels`).
pub fn galley_mesh(galley: &Galley, pos: Pos2) -> Mesh {
    let mut mesh = Mesh::default();
    for row in &galley.rows {
        let base = mesh.vertices.len() as u32;
        let at = pos + row.pos.to_vec2();
        mesh.indices.extend(row.visuals.mesh.indices.iter().map(|i| i + base));
        mesh.vertices.extend(row.visuals.mesh.vertices.iter().map(|v| Vertex { pos: at + v.pos.to_vec2(), uv: v.uv, color: v.color }));
    }
    mesh
}

/// A rounded rect filled with `color`, as a mesh (a fan from its middle) for [`Source::Mesh`].
pub fn rounded_rect_mesh(rect: Rect, radius: f32, color: Color32) -> Mesh {
    const STEPS: usize = 8;
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.center(), color);
    // The corners, clockwise from the top right, each a quarter circle.
    let corners = [
        (Pos2::new(rect.right() - r, rect.top() + r), -90.0_f32),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), 90.0),
        (Pos2::new(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (c, start) in corners {
        for s in 0..=STEPS {
            let a = (start + 90.0 * s as f32 / STEPS as f32).to_radians();
            mesh.colored_vertex(c + egui::vec2(a.cos(), a.sin()) * r, color);
        }
    }
    let n = mesh.vertices.len() as u32 - 1;
    for i in 1..=n {
        mesh.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    mesh
}

enum State {
    Untried,
    Ready(Gpu),
    Failed,
}

static STATE: Mutex<State> = Mutex::new(State::Untried);

/// # Safety
/// egui_glow's GL context has to be current (as it is in a paint callback).
unsafe fn run(state: &mut State, painter: &egui_glow::Painter, info: &PaintCallbackInfo, job: &Job) {
    let gl = painter.gl();
    if let State::Untried = state {
        *state = match unsafe { Gpu::new(gl) } {
            Ok(gpu) => State::Ready(gpu),
            Err(e) => {
                log::warn!("blur unavailable, drawing without it: {e}");
                BROKEN.store(true, Ordering::Relaxed);
                State::Failed
            }
        };
    }
    if let State::Ready(gpu) = state {
        unsafe { gpu.run(gl, painter, info, job) };
    }
}

/// The blur's GL objects: its three programs, a texture pair to work in and what draws into them.
struct Gpu {
    /// Draws a mesh into a work texture.
    draw: glow::Program,
    /// One direction of the Gaussian, from one work texture into the other.
    blur: glow::Program,
    /// The result onto the screen.
    show: glow::Program,
    /// Draws egui's vertices from `vbo`, with its own index buffer.
    mesh_vao: glow::VertexArray,
    /// Has no attributes: the quad's corners come from the vertex index.
    quad_vao: glow::VertexArray,
    vbo: glow::Buffer,
    fbo: glow::Framebuffer,
    work: [glow::Texture; 2],
    /// The work textures' size in pixels (they only grow).
    size: (i32, i32),
    max_side: i32,
}

/// Puts a mesh into the work area (`u_area`: its left, top, width and height in points).
const DRAW_VS: &str = r"
uniform vec4 u_area;
uniform bool u_texels;
uniform sampler2D u_sampler;
in vec2 a_pos;
in vec2 a_tc;
in vec4 a_srgba;
out vec2 v_tc;
out vec4 v_rgba;
void main() {
    vec2 p = (a_pos - u_area.xy) / u_area.zw;
    gl_Position = vec4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    v_tc = u_texels ? a_tc / vec2(textureSize(u_sampler, 0)) : a_tc;
    v_rgba = a_srgba;
}
";

/// As egui's own: the colour times the texture, both in gamma space and premultiplied.
const DRAW_FS: &str = r"
uniform sampler2D u_sampler;
in vec2 v_tc;
in vec4 v_rgba;
out vec4 f_color;
void main() {
    f_color = v_rgba * texture(u_sampler, v_tc);
}
";

/// A quad over `u_dst` (left, bottom, right, top in clip space), its corners from the vertex index.
const QUAD_VS: &str = r"
uniform vec4 u_dst;
out vec2 v_uv;
void main() {
    vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = c;
    gl_Position = vec4(mix(u_dst.xy, u_dst.zw, c), 0.0, 1.0);
}
";

/// One direction of the Gaussian (`u_step`: a texel along it), kept to the used part of the work
/// texture (`u_used`), where the source is.
const BLUR_FS: &str = r"
uniform sampler2D u_sampler;
uniform vec2 u_used;
uniform vec2 u_step;
uniform float u_sigma;
uniform int u_radius;
in vec2 v_uv;
out vec4 f_color;
void main() {
    vec2 texel = 1.0 / vec2(textureSize(u_sampler, 0));
    vec2 lo = 0.5 * texel;
    vec2 hi = u_used - 0.5 * texel;
    vec2 uv = v_uv * u_used;
    vec4 sum = texture(u_sampler, clamp(uv, lo, hi));
    float total = 1.0;
    for (int i = 1; i <= u_radius; i++) {
        float x = float(i);
        float w = exp(-x * x / (2.0 * u_sigma * u_sigma));
        sum += w * (texture(u_sampler, clamp(uv + u_step * x, lo, hi)) + texture(u_sampler, clamp(uv - u_step * x, lo, hi)));
        total += 2.0 * w;
    }
    f_color = sum / total;
}
";

/// The result on screen: CSS `saturate()` (the Filter Effects matrix), tinted, kept inside or outside
/// a rounded rect (`u_rect`: window pixels from the bottom left), and dithered as egui's own colours.
const SHOW_FS: &str = r"
uniform sampler2D u_sampler;
uniform vec2 u_used;
uniform vec4 u_tint;
uniform float u_saturate;
uniform int u_shape;
uniform vec4 u_rect;
uniform float u_radius;
in vec2 v_uv;
out vec4 f_color;
float noise(vec2 n) {
    float f = 0.06711056 * n.x + 0.00583715 * n.y;
    return fract(52.9829189 * fract(f));
}
void main() {
    vec2 texel = 1.0 / vec2(textureSize(u_sampler, 0));
    vec4 c = texture(u_sampler, clamp(v_uv * u_used, 0.5 * texel, u_used - 0.5 * texel));
    if (u_saturate != 1.0 && c.a > 0.0) {
        float s = u_saturate;
        mat3 m = mat3(
            0.213 + 0.787 * s, 0.213 - 0.213 * s, 0.213 - 0.213 * s,
            0.715 - 0.715 * s, 0.715 + 0.285 * s, 0.715 - 0.715 * s,
            0.072 - 0.072 * s, 0.072 - 0.072 * s, 0.072 + 0.928 * s);
        c.rgb = clamp(m * (c.rgb / c.a), 0.0, 1.0) * c.a;
    }
    c *= u_tint;
    if (u_shape != 0) {
        vec2 half_size = 0.5 * (u_rect.zw - u_rect.xy);
        vec2 q = abs(gl_FragCoord.xy - 0.5 * (u_rect.xy + u_rect.zw)) - half_size + u_radius;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - u_radius;
        float inside = clamp(0.5 - d, 0.0, 1.0);
        c *= u_shape == 1 ? inside : 1.0 - inside;
    }
    c.rgb += (noise(gl_FragCoord.xy) - 0.5) * 0.95 / 255.0 * c.a;
    f_color = c;
}
";

impl Gpu {
    /// # Safety
    /// The GL context has to be current.
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let version = egui_glow::ShaderVersion::get(gl);
        if !version.is_new_shader_interface() {
            return Err(format!("it needs GLSL 1.40 or ES 3.00, the driver has {version:?}"));
        }
        let head = format!("{}{}", version.version_declaration(), if version.is_embedded() { "precision highp float;\nprecision highp int;\n" } else { "" });
        unsafe {
            let draw = program(gl, &head, DRAW_VS, DRAW_FS, &["a_pos", "a_tc", "a_srgba"], version.is_embedded())?;
            let blur = program(gl, &head, QUAD_VS, BLUR_FS, &[], version.is_embedded())?;
            let show = program(gl, &head, QUAD_VS, SHOW_FS, &[], version.is_embedded())?;
            let mesh_vao = gl.create_vertex_array()?;
            let quad_vao = gl.create_vertex_array()?;
            let vbo = gl.create_buffer()?;
            let ebo = gl.create_buffer()?;
            let fbo = gl.create_framebuffer()?;
            let work = [work_texture(gl)?, work_texture(gl)?];
            // egui's vertices: the position and the UV (two floats each), then the colour (four bytes).
            gl.bind_vertex_array(Some(mesh_vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 20, 0);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 20, 8);
            gl.vertex_attrib_pointer_f32(2, 4, glow::UNSIGNED_BYTE, true, 20, 16);
            for i in 0..3 {
                gl.enable_vertex_attrib_array(i);
            }
            gl.bind_vertex_array(None);
            let max_side = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(1024);
            match gl.get_error() {
                glow::NO_ERROR => Ok(Self { draw, blur, show, mesh_vao, quad_vao, vbo, fbo, work, size: (0, 0), max_side }),
                e => Err(format!("GL error {e:#x} while setting up")),
            }
        }
    }

    /// # Safety
    /// The GL context has to be current, in an egui_glow paint callback.
    unsafe fn run(&mut self, gl: &glow::Context, painter: &egui_glow::Painter, info: &PaintCallbackInfo, job: &Job) {
        let ppp = info.pixels_per_point;
        let [screen_w, screen_h] = info.screen_size_px.map(|v| v as i32);
        // The area in window pixels, from the top left (what is on screen can only be read there).
        let (mut x0, mut y0) = ((job.area.min.x * ppp).floor() as i32, (job.area.min.y * ppp).floor() as i32);
        let (mut x1, mut y1) = ((job.area.max.x * ppp).ceil() as i32, (job.area.max.y * ppp).ceil() as i32);
        if matches!(job.source, Source::Backdrop) {
            (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(screen_w), y1.min(screen_h));
        }
        let (aw, ah) = (x1 - x0, y1 - y0);
        if aw <= 0 || ah <= 0 {
            return;
        }
        // A wide blur works on a copy that many times smaller, so it spreads 8 texels at most.
        let sigma_px = job.sigma * ppp;
        let down = ((sigma_px / 8.0).floor() as i32).max(1).max((aw + self.max_side - 1) / self.max_side).max((ah + self.max_side - 1) / self.max_side);
        let (w, h) = ((aw + down - 1) / down, (ah + down - 1) / down);
        unsafe {
            self.reserve(gl, w, h);
            let used = [w as f32 / self.size.0 as f32, h as f32 / self.size.1 as f32];
            let target = std::num::NonZeroU32::new(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32).map(glow::NativeFramebuffer);

            // 1: the source into work texture 0.
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            self.attach(gl, 0);
            gl.viewport(0, 0, w, h);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            match &job.source {
                Source::Mesh { mesh, texels } => {
                    let Some(texture) = painter.texture(mesh.texture_id) else {
                        return self.done(gl, target);
                    };
                    gl.enable(glow::BLEND);
                    gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                    gl.use_program(Some(self.draw));
                    let at = |name: &str| gl.get_uniform_location(self.draw, name);
                    gl.uniform_4_f32(at("u_area").as_ref(), x0 as f32 / ppp, y0 as f32 / ppp, aw as f32 / ppp, ah as f32 / ppp);
                    gl.uniform_1_i32(at("u_texels").as_ref(), i32::from(*texels));
                    gl.uniform_1_i32(at("u_sampler").as_ref(), 0);
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.bind_vertex_array(Some(self.mesh_vao));
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &vertex_bytes(mesh), glow::STREAM_DRAW);
                    let indices: Vec<u8> = mesh.indices.iter().flat_map(|i| i.to_ne_bytes()).collect();
                    gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, &indices, glow::STREAM_DRAW);
                    gl.draw_elements(glow::TRIANGLES, mesh.indices.len() as i32, glow::UNSIGNED_INT, 0);
                }
                Source::Backdrop => {
                    // Read where egui draws, into ours: shrunk on the way when the blur is wide.
                    gl.bind_framebuffer(glow::READ_FRAMEBUFFER, target);
                    gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.fbo));
                    let filter = if down > 1 { glow::LINEAR } else { glow::NEAREST };
                    gl.blit_framebuffer(x0, screen_h - y1, x1, screen_h - y0, 0, 0, w, h, glow::COLOR_BUFFER_BIT, filter);
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                }
            }

            // 2 and 3: across into texture 1, then down back into texture 0.
            let sigma = sigma_px / down as f32;
            let radius = (sigma * 3.0).ceil().min(64.0) as i32;
            if sigma > 0.05 && radius >= 1 {
                gl.disable(glow::BLEND);
                gl.use_program(Some(self.blur));
                gl.bind_vertex_array(Some(self.quad_vao));
                let at = |name: &str| gl.get_uniform_location(self.blur, name);
                gl.uniform_4_f32(at("u_dst").as_ref(), -1.0, -1.0, 1.0, 1.0);
                gl.uniform_2_f32(at("u_used").as_ref(), used[0], used[1]);
                gl.uniform_1_f32(at("u_sigma").as_ref(), sigma);
                gl.uniform_1_i32(at("u_radius").as_ref(), radius);
                gl.uniform_1_i32(at("u_sampler").as_ref(), 0);
                gl.active_texture(glow::TEXTURE0);
                for (from, to, step) in [(0, 1, [1.0 / self.size.0 as f32, 0.0]), (1, 0, [0.0, 1.0 / self.size.1 as f32])] {
                    self.attach(gl, to);
                    gl.bind_texture(glow::TEXTURE_2D, Some(self.work[from]));
                    gl.uniform_2_f32(at("u_step").as_ref(), step[0], step[1]);
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            // 4: onto the screen, inside egui's clip, blended as egui blends.
            gl.bind_framebuffer(glow::FRAMEBUFFER, target);
            gl.viewport(0, 0, screen_w, screen_h);
            let clip = info.clip_rect_in_pixels();
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(clip.left_px, clip.from_bottom_px, clip.width_px, clip.height_px);
            gl.enable(glow::BLEND);
            gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE_MINUS_DST_ALPHA, glow::ONE);
            gl.use_program(Some(self.show));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.work[0]));
            let at = |name: &str| gl.get_uniform_location(self.show, name);
            let ndc = |x: i32, y: i32| (x as f32 / screen_w as f32 * 2.0 - 1.0, 1.0 - y as f32 / screen_h as f32 * 2.0);
            let ((left, bottom), (right, top)) = (ndc(x0, y1), ndc(x1, y0));
            gl.uniform_4_f32(at("u_dst").as_ref(), left, bottom, right, top);
            gl.uniform_2_f32(at("u_used").as_ref(), used[0], used[1]);
            let [r, g, b, a] = job.tint;
            gl.uniform_4_f32(at("u_tint").as_ref(), r, g, b, a);
            gl.uniform_1_f32(at("u_saturate").as_ref(), job.saturate);
            gl.uniform_1_i32(at("u_sampler").as_ref(), 0);
            let (kind, rect, radius) = match job.shape {
                Shape::All => (0, Rect::NOTHING, 0.0),
                Shape::Inside(rect, radius) => (1, rect, radius),
                Shape::Outside(rect, radius) => (2, rect, radius),
            };
            gl.uniform_1_i32(at("u_shape").as_ref(), kind);
            if kind != 0 {
                let sh = screen_h as f32;
                gl.uniform_4_f32(at("u_rect").as_ref(), rect.left() * ppp, sh - rect.bottom() * ppp, rect.right() * ppp, sh - rect.top() * ppp);
                gl.uniform_1_f32(at("u_radius").as_ref(), radius * ppp);
            }
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.done(gl, target);
        }
    }

    /// Leaves the GL as egui left it where it doesn't set it up again itself.
    ///
    /// # Safety
    /// The GL context has to be current.
    unsafe fn done(&self, gl: &glow::Context, target: Option<glow::Framebuffer>) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, target);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Makes the work textures at least `w` × `h` pixels.
    ///
    /// # Safety
    /// The GL context has to be current.
    unsafe fn reserve(&mut self, gl: &glow::Context, w: i32, h: i32) {
        if w <= self.size.0 && h <= self.size.1 {
            return;
        }
        let grow = |need: i32, have: i32| ((need.max(have) + 255) / 256 * 256).min(self.max_side);
        self.size = (grow(w, self.size.0), grow(h, self.size.1));
        for texture in self.work {
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, self.size.0, self.size.1, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            }
        }
    }

    /// Draws into work texture `i` from now on.
    ///
    /// # Safety
    /// The GL context has to be current, with the blur's framebuffer bound.
    unsafe fn attach(&self, gl: &glow::Context, i: usize) {
        unsafe { gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.work[i]), 0) };
    }
}

/// # Safety
/// The GL context has to be current.
unsafe fn program(gl: &glow::Context, head: &str, vs: &str, fs: &str, attributes: &[&str], embedded: bool) -> Result<glow::Program, String> {
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, body) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, &format!("{head}{body}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(gl.get_shader_info_log(shader));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        for (i, name) in attributes.iter().enumerate() {
            gl.bind_attrib_location(program, i as u32, name);
        }
        if !embedded {
            gl.bind_frag_data_location(program, 0, "f_color");
        }
        gl.link_program(program);
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if gl.get_program_link_status(program) { Ok(program) } else { Err(gl.get_program_info_log(program)) }
    }
}

/// # Safety
/// The GL context has to be current.
unsafe fn work_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    unsafe {
        let texture = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        for (key, value) in [
            (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
            (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, key, value as i32);
        }
        Ok(texture)
    }
}

/// egui's vertices as the GL takes them: position, UV, then the colour's four bytes.
fn vertex_bytes(mesh: &Mesh) -> Vec<u8> {
    let mut out = Vec::with_capacity(mesh.vertices.len() * 20);
    for v in &mesh.vertices {
        for f in [v.pos.x, v.pos.y, v.uv.x, v.uv.y] {
            out.extend_from_slice(&f.to_ne_bytes());
        }
        out.extend_from_slice(&v.color.to_array());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    #[test]
    fn a_rounded_rect_mesh_covers_its_rect_and_keeps_its_corners_round() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(100.0, 50.0));
        let mesh = rounded_rect_mesh(rect, 14.0, Color32::BLACK);
        let bounds = mesh.calc_bounds();
        assert!((bounds.min - rect.min).length() < 0.01 && (bounds.max - rect.max).length() < 0.01, "{bounds:?}");
        // No point sits in a corner's cut-off bit.
        let corner = rect.min + vec2(14.0, 14.0);
        for v in &mesh.vertices {
            if v.pos.x < corner.x && v.pos.y < corner.y {
                assert!((v.pos - corner).length() <= 14.0 + 0.01, "{:?}", v.pos);
            }
        }
        assert_eq!(mesh.indices.len() % 3, 0);
    }

    #[test]
    fn vertices_go_to_the_gpu_as_twenty_bytes_each() {
        let mut mesh = Mesh::default();
        mesh.colored_vertex(pos2(1.0, 2.0), Color32::from_rgba_premultiplied(1, 2, 3, 4));
        let bytes = vertex_bytes(&mesh);
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[0..4], &1.0_f32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &[1, 2, 3, 4]);
    }
}
