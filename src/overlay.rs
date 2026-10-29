//! Оверлей физического режима: нарисованные линии, верёвки, связи между
//! окнами, превью инструментов, пол и индикатор инструмента.
//!
//! Всё рисуется одним render-элементом и одним draw call: геометрия
//! собирается на CPU в список цветных треугольников (экранные физические
//! пиксели), а [`OverlayElement::draw`] отдаёт его в маленький собственный
//! GLES-шейдер через `GlesFrame::with_context`. Раньше декоративные точки
//! пола были сотнями отдельных render-элементов и давали микрофризы — здесь
//! число элементов не зависит от количества линий.
//!
//! Здесь же живёт [`scissored`] — отрисовка только внутри damage-прямоугольников.
//! Элементы, которые рисуют себя сами (повёрнутые окна, оверлей), обязаны
//! уважать damage: иначе при частичной перерисовке они затирают пиксели
//! окон, которые damage-tracker не перерисовывает.

use std::cell::RefCell;
use std::collections::HashMap;

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, ffi},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

// ── Отсечение по damage ────────────────────────────────────────────

/// Вызывает `draw` по разу на каждый damage-прямоугольник с выставленным
/// GL scissor. `dst` — геометрия элемента, `damage` — относительно `dst.loc`
/// (как их передаёт damage-tracker). Scissor считается через проекцию кадра,
/// поэтому корректен при любом output transform (winit = Flipped180).
pub fn scissored<F>(
    frame: &mut GlesFrame<'_, '_>,
    dst: Rectangle<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
    mut draw: F,
) -> Result<(), GlesError>
where
    F: FnMut(&mut GlesFrame<'_, '_>) -> Result<(), GlesError>,
{
    if damage.is_empty() {
        return Ok(());
    }
    let m = *frame.projection();
    let (viewport, old_scissor) = frame.with_context(|gl| unsafe {
        let mut v = [0i32; 4];
        let mut s = [0i32; 4];
        gl.GetIntegerv(ffi::VIEWPORT, v.as_mut_ptr());
        gl.GetIntegerv(ffi::SCISSOR_BOX, s.as_mut_ptr());
        (v, s)
    })?;
    let to_fb = |x: f32, y: f32| -> (f32, f32) {
        // Проекция smithay — column-major mat3: физические px → NDC.
        let nx = m[0] * x + m[3] * y + m[6];
        let ny = m[1] * x + m[4] * y + m[7];
        (
            viewport[0] as f32 + (nx + 1.0) * 0.5 * viewport[2] as f32,
            viewport[1] as f32 + (ny + 1.0) * 0.5 * viewport[3] as f32,
        )
    };
    let mut result = Ok(());
    for d in damage {
        let x0 = (dst.loc.x + d.loc.x) as f32;
        let y0 = (dst.loc.y + d.loc.y) as f32;
        let x1 = x0 + d.size.w as f32;
        let y1 = y0 + d.size.h as f32;
        let pts = [to_fb(x0, y0), to_fb(x1, y0), to_fb(x0, y1), to_fb(x1, y1)];
        let min_x = pts
            .iter()
            .map(|p| p.0)
            .fold(f32::INFINITY, f32::min)
            .floor() as i32;
        let max_x = pts
            .iter()
            .map(|p| p.0)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil() as i32;
        let min_y = pts
            .iter()
            .map(|p| p.1)
            .fold(f32::INFINITY, f32::min)
            .floor() as i32;
        let max_y = pts
            .iter()
            .map(|p| p.1)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil() as i32;
        // Не выходим за scissor, который выставил сам smithay.
        let sx0 = min_x.max(old_scissor[0]);
        let sy0 = min_y.max(old_scissor[1]);
        let sx1 = max_x.min(old_scissor[0] + old_scissor[2]);
        let sy1 = max_y.min(old_scissor[1] + old_scissor[3]);
        if sx1 <= sx0 || sy1 <= sy0 {
            continue;
        }
        frame.with_context(|gl| unsafe { gl.Scissor(sx0, sy0, sx1 - sx0, sy1 - sy0) })?;
        if let Err(e) = draw(frame) {
            result = Err(e);
            break;
        }
    }
    frame.with_context(|gl| unsafe {
        gl.Scissor(
            old_scissor[0],
            old_scissor[1],
            old_scissor[2],
            old_scissor[3],
        )
    })?;
    result
}

// ── Геометрия оверлея ──────────────────────────────────────────────

/// Список цветных треугольников в экранных физических пикселях.
/// Формат вершины: x, y, r, g, b, a (цвет premultiplied).
#[derive(Default, Clone)]
pub struct OverlayGeometry {
    verts: Vec<f32>,
    min: [f32; 2],
    max: [f32; 2],
}

impl OverlayGeometry {
    pub fn new() -> Self {
        Self {
            verts: Vec::new(),
            min: [f32::INFINITY; 2],
            max: [f32::NEG_INFINITY; 2],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.verts.is_empty()
    }

    fn vertex(&mut self, p: [f32; 2], c: [f32; 4]) {
        if !(p[0].is_finite() && p[1].is_finite()) {
            return;
        }
        let a = c[3];
        self.verts
            .extend_from_slice(&[p[0], p[1], c[0] * a, c[1] * a, c[2] * a, a]);
        self.min = [self.min[0].min(p[0]), self.min[1].min(p[1])];
        self.max = [self.max[0].max(p[0]), self.max[1].max(p[1])];
    }

    fn triangle(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], color: [f32; 4]) {
        if [a, b, c]
            .iter()
            .any(|p| !(p[0].is_finite() && p[1].is_finite()))
        {
            return;
        }
        self.vertex(a, color);
        self.vertex(b, color);
        self.vertex(c, color);
    }

    /// Отрезок толщины `width` (прямоугольник вдоль `a→b`).
    pub fn segment(&mut self, a: [f32; 2], b: [f32; 2], width: f32, color: [f32; 4]) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            return;
        }
        let h = width.max(1.0) * 0.5;
        let nx = -dy / len * h;
        let ny = dx / len * h;
        let p0 = [a[0] + nx, a[1] + ny];
        let p1 = [b[0] + nx, b[1] + ny];
        let p2 = [b[0] - nx, b[1] - ny];
        let p3 = [a[0] - nx, a[1] - ny];
        self.triangle(p0, p1, p2, color);
        self.triangle(p0, p2, p3, color);
    }

    /// Закрашенный круг.
    pub fn disc(&mut self, c: [f32; 2], r: f32, color: [f32; 4]) {
        let n = ((r * 0.8) as usize).clamp(8, 32);
        let mut prev = [c[0] + r, c[1]];
        for i in 1..=n {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            let p = [c[0] + r * t.cos(), c[1] + r * t.sin()];
            self.triangle(c, prev, p, color);
            prev = p;
        }
    }

    /// Ломаная со скруглёнными стыками.
    pub fn polyline(&mut self, pts: &[[f32; 2]], width: f32, color: [f32; 4]) {
        for w in pts.windows(2) {
            self.segment(w[0], w[1], width, color);
        }
        if width >= 3.0 {
            for p in pts {
                self.disc(*p, width * 0.5, color);
            }
        }
    }

    /// Пружина: зигзаг между двумя точками.
    pub fn zigzag(&mut self, a: [f32; 2], b: [f32; 2], width: f32, amp: f32, color: [f32; 4]) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1.0 {
            return;
        }
        let (ux, uy) = (dx / len, dy / len);
        let (nx, ny) = (-uy, ux);
        let turns = ((len / (amp * 1.5)).round() as usize).clamp(4, 40);
        let mut pts = Vec::with_capacity(turns + 3);
        pts.push(a);
        let lead = len * 0.1;
        for i in 0..=turns {
            let t = lead + (len - 2.0 * lead) * i as f32 / turns as f32;
            let side = if i == 0 || i == turns {
                0.0
            } else if i % 2 == 0 {
                amp
            } else {
                -amp
            };
            pts.push([a[0] + ux * t + nx * side, a[1] + uy * t + ny * side]);
        }
        pts.push(b);
        self.polyline(&pts, width, color);
    }

    /// Пунктир (превью ножа и т.п.).
    pub fn dashed(&mut self, a: [f32; 2], b: [f32; 2], width: f32, dash: f32, color: [f32; 4]) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            return;
        }
        let n = ((len / dash) as usize).min(512);
        for i in (0..=n).step_by(2) {
            let t0 = i as f32 * dash / len;
            let t1 = ((i + 1) as f32 * dash / len).min(1.0);
            if t0 >= 1.0 {
                break;
            }
            self.segment(
                [a[0] + dx * t0, a[1] + dy * t0],
                [a[0] + dx * t1, a[1] + dy * t1],
                width,
                color,
            );
        }
    }

    /// Ограничивающий прямоугольник с запасом на сглаживание.
    pub fn bounds(&self) -> Option<Rectangle<i32, Physical>> {
        if self.verts.is_empty() || !self.min[0].is_finite() {
            return None;
        }
        let x0 = self.min[0].floor() as i32 - 1;
        let y0 = self.min[1].floor() as i32 - 1;
        let x1 = self.max[0].ceil() as i32 + 1;
        let y1 = self.max[1].ceil() as i32 + 1;
        Some(Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()))
    }

    /// Дешёвый отпечаток содержимого — чтобы не будить damage-tracker,
    /// когда оверлей не изменился.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for v in &self.verts {
            h ^= v.to_bits() as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^ self.verts.len() as u64
    }
}

// ── Render-элемент ─────────────────────────────────────────────────

pub struct OverlayElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    verts: Vec<f32>,
}

impl OverlayElement {
    pub fn new(id: Id, commit: CommitCounter, geometry: OverlayGeometry) -> Option<Self> {
        let bounds = geometry.bounds()?;
        Some(Self {
            id,
            commit,
            geometry: bounds,
            verts: geometry.verts,
        })
    }
}

impl Element for OverlayElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.geometry.size.w as f64, self.geometry.size.h as f64).into())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)])
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

struct OverlayProgram {
    program: ffi::types::GLuint,
    a_pos: ffi::types::GLint,
    a_color: ffi::types::GLint,
    u_matrix: ffi::types::GLint,
}

thread_local! {
    /// Программа на каждый GL-контекст (ключ — EGLContext handle).
    static PROGRAMS: RefCell<HashMap<usize, Option<OverlayProgram>>> = RefCell::new(HashMap::new());
}

const VERTEX_SHADER: &str = "#version 100
attribute vec2 a_pos;
attribute vec4 a_color;
uniform mat3 u_matrix;
varying vec4 v_color;
void main() {
    vec3 p = u_matrix * vec3(a_pos, 1.0);
    gl_Position = vec4(p.xy, 0.0, 1.0);
    v_color = a_color;
}
";

const FRAGMENT_SHADER: &str = "#version 100
precision mediump float;
varying vec4 v_color;
void main() {
    gl_FragColor = v_color;
}
";

unsafe fn compile_shader(gl: &ffi::Gles2, kind: ffi::types::GLenum, src: &str) -> Option<u32> {
    unsafe {
        let shader = gl.CreateShader(kind);
        let ptr = src.as_ptr() as *const ffi::types::GLchar;
        let len = src.len() as ffi::types::GLint;
        gl.ShaderSource(shader, 1, &ptr, &len);
        gl.CompileShader(shader);
        let mut ok = 0;
        gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            gl.DeleteShader(shader);
            eprintln!("[overlay] не удалось скомпилировать шейдер");
            return None;
        }
        Some(shader)
    }
}

unsafe fn link_program(gl: &ffi::Gles2) -> Option<OverlayProgram> {
    unsafe {
        let vs = compile_shader(gl, ffi::VERTEX_SHADER, VERTEX_SHADER)?;
        let Some(fs) = compile_shader(gl, ffi::FRAGMENT_SHADER, FRAGMENT_SHADER) else {
            gl.DeleteShader(vs);
            return None;
        };
        let program = gl.CreateProgram();
        gl.AttachShader(program, vs);
        gl.AttachShader(program, fs);
        gl.LinkProgram(program);
        gl.DetachShader(program, vs);
        gl.DetachShader(program, fs);
        gl.DeleteShader(vs);
        gl.DeleteShader(fs);
        let mut ok = 0;
        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut ok);
        if ok == 0 {
            gl.DeleteProgram(program);
            eprintln!("[overlay] не удалось слинковать программу");
            return None;
        }
        Some(OverlayProgram {
            program,
            a_pos: gl.GetAttribLocation(program, c"a_pos".as_ptr()),
            a_color: gl.GetAttribLocation(program, c"a_color".as_ptr()),
            u_matrix: gl.GetUniformLocation(program, c"u_matrix".as_ptr()),
        })
    }
}

impl RenderElement<GlesRenderer> for OverlayElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        if self.verts.is_empty() {
            return Ok(());
        }
        let key = frame.egl_context().get_context_handle() as usize;
        let matrix = *frame.projection();
        let verts = &self.verts;
        scissored(frame, dst, damage, |frame| {
            frame.with_context(|gl| {
                PROGRAMS.with(|programs| {
                    let mut programs = programs.borrow_mut();
                    let program = programs
                        .entry(key)
                        .or_insert_with(|| unsafe { link_program(gl) });
                    let Some(p) = program.as_ref() else {
                        return;
                    };
                    if p.a_pos < 0 || p.a_color < 0 {
                        return;
                    }
                    let stride = (6 * std::mem::size_of::<f32>()) as ffi::types::GLsizei;
                    unsafe {
                        gl.UseProgram(p.program);
                        gl.UniformMatrix3fv(p.u_matrix, 1, ffi::FALSE, matrix.as_ptr());
                        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                        gl.EnableVertexAttribArray(p.a_pos as u32);
                        gl.EnableVertexAttribArray(p.a_color as u32);
                        // smithay рисует текстуры instanced и оставляет divisor=1
                        // на своих атрибутах; если наш атрибут попал в тот же
                        // слот, цвет читался бы один на все вершины.
                        if gl.VertexAttribDivisor.is_loaded() {
                            gl.VertexAttribDivisor(p.a_pos as u32, 0);
                            gl.VertexAttribDivisor(p.a_color as u32, 0);
                        }
                        gl.VertexAttribPointer(
                            p.a_pos as u32,
                            2,
                            ffi::FLOAT,
                            ffi::FALSE,
                            stride,
                            verts.as_ptr() as *const _,
                        );
                        gl.VertexAttribPointer(
                            p.a_color as u32,
                            4,
                            ffi::FLOAT,
                            ffi::FALSE,
                            stride,
                            verts.as_ptr().add(2) as *const _,
                        );
                        gl.DrawArrays(ffi::TRIANGLES, 0, (verts.len() / 6) as i32);
                        gl.DisableVertexAttribArray(p.a_pos as u32);
                        gl.DisableVertexAttribArray(p.a_color as u32);
                        gl.UseProgram(0);
                    }
                });
            })
        })
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// Перевод мировой точки в экранную (физические px).
pub fn to_screen(p: rapier2d::math::Vector, cam: (f64, f64), zoom: f64) -> [f32; 2] {
    [
        ((p.x as f64 - cam.0) * zoom) as f32,
        ((p.y as f64 - cam.1) * zoom) as f32,
    ]
}
