//! Отрисовка текстуры окна на деформированной сетке (концепт §5).
//!
//! - [`MeshElement`] — render element smithay, который рисует текстуру на
//!   произвольной треугольной сетке собственным GLSL-шейдером через сырой GL
//!   (`GlesFrame::with_context`). Вершины — в ФИЗИЧЕСКИХ пикселях выхода (камера
//!   уже применена вызывающим), texcoords — 0..1 по текстуре, начало слева
//!   сверху, как окно выглядит на экране.
//! - [`SnapshotCache`] — снимок всего дерева поверхностей окна (toplevel +
//!   subsurface, без popup) в обычную 2D RGBA-текстуру через штатный пайплайн
//!   smithay. Так шейдер сетки всегда сэмплит простую `sampler2D`-текстуру,
//!   какой бы ни был буфер клиента (SHM xrgb/argb, dmabuf, external OES,
//!   y-inverted), а subsurface'ы деформируются вместе с окном.
//!
//! Ориентация (проверена тестами ниже на реальном GL): у текстур smithay без
//! флага y_inverted texcoord v = 0 — ВЕРХНИЙ ряд картинки. Offscreen-текстура,
//! в которую smithay рисует с `Transform::Normal`, получается именно такой
//! (физический y = 0 → ряд 0 текстуры), поэтому снимок «прямой», и его можно
//! сэмплить texcoord'ами (0,0) = левый верхний угол окна. Для y-inverted
//! текстур (бывают у dmabuf) `MeshElement` переворачивает v сам.

// Интеграция в render/state идёт отдельно — часть API пока не вызывается.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::format::has_alpha;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    Capability, GlesError, GlesFrame, GlesRenderer, GlesTexture, ffi, link_program,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Bind, Color32F, Frame, Offscreen, Renderer, Texture};
use smithay::desktop::Window;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::seat::WaylandFocus;

// --- Шейдер сетки ---

const MESH_VERT: &str = r#"#version 100
uniform mat3 matrix;
uniform vec4 tex_xform;
attribute vec2 position;
attribute vec2 texcoord;
varying vec2 v_tex;
void main() {
    v_tex = texcoord * tex_xform.xy + tex_xform.zw;
    gl_Position = vec4((matrix * vec3(position, 1.0)).xy, 0.0, 1.0);
}
"#;

const MESH_FRAG: &str = r#"#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform sampler2D tex;
uniform float alpha;
uniform float opaque;
varying vec2 v_tex;
void main() {
    vec4 color = texture2D(tex, v_tex);
    // Текстура без альфы (xrgb): мусор в альфа-канале игнорируем, как smithay.
    if (opaque > 0.5) {
        color.a = 1.0;
    }
    // Premultiplied alpha, как во всём пайплайне smithay.
    gl_FragColor = color * alpha;
}
"#;

/// Скомпилированная программа и её локации.
#[derive(Clone, Copy, Debug)]
struct MeshProgram {
    program: ffi::types::GLuint,
    attr_position: ffi::types::GLuint,
    attr_texcoord: ffi::types::GLuint,
    uni_matrix: ffi::types::GLint,
    uni_tex_xform: ffi::types::GLint,
    uni_tex: ffi::types::GLint,
    uni_alpha: ffi::types::GLint,
    uni_opaque: ffi::types::GLint,
}

#[derive(Clone, Copy, Debug)]
enum ProgramSlot {
    Unset,
    Ready(MeshProgram),
    /// Компиляция не удалась — больше не пытаемся каждый кадр.
    Failed,
}

/// Кэш программы в user_data EGL-контекста: программа — объект контекста,
/// поэтому один кэш на контекст (и на рендерер).
struct MeshProgramCache(Cell<ProgramSlot>);

/// Компилирует программу сетки.
///
/// # Safety
/// Контекст `gl` должен быть текущим.
unsafe fn compile_mesh_program(gl: &ffi::Gles2) -> Result<MeshProgram, GlesError> {
    unsafe {
        let program = link_program(gl, MESH_VERT, MESH_FRAG)?;
        let attr = |name: &std::ffi::CStr| gl.GetAttribLocation(program, name.as_ptr());
        let uni = |name: &std::ffi::CStr| gl.GetUniformLocation(program, name.as_ptr());
        let attr_position = attr(c"position");
        let attr_texcoord = attr(c"texcoord");
        let p = MeshProgram {
            program,
            attr_position: attr_position.max(0) as u32,
            attr_texcoord: attr_texcoord.max(0) as u32,
            uni_matrix: uni(c"matrix"),
            uni_tex_xform: uni(c"tex_xform"),
            uni_tex: uni(c"tex"),
            uni_alpha: uni(c"alpha"),
            uni_opaque: uni(c"opaque"),
        };
        if attr_position < 0
            || attr_texcoord < 0
            || p.uni_matrix < 0
            || p.uni_tex_xform < 0
            || p.uni_tex < 0
            || p.uni_alpha < 0
            || p.uni_opaque < 0
        {
            gl.DeleteProgram(program);
            return Err(GlesError::ProgramLinkError);
        }
        Ok(p)
    }
}

/// Программа сетки для контекста кадра: компилируется лениво один раз.
/// `None` — компиляция не удалась (уже сообщили в лог), рисовать нечем.
fn mesh_program(frame: &mut GlesFrame<'_, '_>) -> Result<Option<MeshProgram>, GlesError> {
    let slot = frame
        .egl_context()
        .user_data()
        .get_or_insert(|| MeshProgramCache(Cell::new(ProgramSlot::Unset)))
        .0
        .get();
    match slot {
        ProgramSlot::Ready(p) => return Ok(Some(p)),
        ProgramSlot::Failed => return Ok(None),
        ProgramSlot::Unset => {}
    }
    let compiled = frame.with_context(|gl| unsafe { compile_mesh_program(gl) })?;
    let slot = match compiled {
        Ok(p) => ProgramSlot::Ready(p),
        Err(err) => {
            eprintln!("[mesh] не удалось собрать шейдер сетки: {err:?}");
            ProgramSlot::Failed
        }
    };
    if let Some(cache) = frame.egl_context().user_data().get::<MeshProgramCache>() {
        cache.0.set(slot);
    }
    Ok(match slot {
        ProgramSlot::Ready(p) => Some(p),
        _ => None,
    })
}

// --- MeshElement ---

/// Текстура, натянутая на треугольную сетку.
///
/// Текстура должна быть обычной `TEXTURE_2D` (снимок [`SnapshotCache`],
/// `import_memory`, SHM). External OES-текстуры dmabuf шейдер не сэмплит, а
/// публично smithay их не отличает — клиентские буферы пропускайте через снимок.
/// Элемент пересоздаётся каждый кадр; `Id` держите постоянным для куска окна
/// (так трекер видит перемещение одного элемента, а не исчезновение/появление).
/// Damage — всегда вся геометрия (как у `PhysicsElement`), так что `commit`
/// на перерисовку не влияет; можно передавать, например, `Snapshot::commit`.
pub struct MeshElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    /// Вершины в физических пикселях выхода.
    positions: Vec<[f32; 2]>,
    /// Texcoords 0..1 по текстуре, (0,0) — левый верхний угол как на экране.
    texcoords: Vec<[f32; 2]>,
    indices: Vec<u16>,
    alpha: f32,
    /// Bounding rect'ы прошлых кадров (buffer age), см. `PhysicsElement`.
    damage_history: Vec<Rectangle<i32, Physical>>,
    geometry: Rectangle<i32, Physical>,
}

impl MeshElement {
    /// Сетку проверяем сразу: лишние вершины без texcoord отбрасываются,
    /// треугольники с индексами за пределами массива — тоже (иначе GPU читал
    /// бы чужую память).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        texture: GlesTexture,
        mut positions: Vec<[f32; 2]>,
        mut texcoords: Vec<[f32; 2]>,
        mut indices: Vec<u16>,
        alpha: f32,
        damage_history: Vec<Rectangle<i32, Physical>>,
    ) -> Self {
        let n = positions.len().min(texcoords.len());
        positions.truncate(n);
        texcoords.truncate(n);
        indices.truncate(indices.len() / 3 * 3);
        if indices.iter().any(|&i| i as usize >= n) {
            indices = indices
                .chunks_exact(3)
                .filter(|t| t.iter().all(|&i| (i as usize) < n))
                .flatten()
                .copied()
                .collect();
        }
        let geometry = Self::bounding_box(&positions, &indices);
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            1.0
        };
        Self {
            id,
            commit,
            texture,
            positions,
            texcoords,
            indices,
            alpha,
            damage_history,
            geometry,
        }
    }

    /// Bounding box используемых вершин, округлённый наружу.
    fn bounding_box(positions: &[[f32; 2]], indices: &[u16]) -> Rectangle<i32, Physical> {
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for &i in indices {
            let p = positions[i as usize];
            if !(p[0].is_finite() && p[1].is_finite()) {
                continue;
            }
            for k in 0..2 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        if min[0] > max[0] || min[1] > max[1] {
            return Rectangle::default();
        }
        // `as i32` насыщается, так что гигантские координаты не переполнят.
        let x0 = min[0].floor() as i32;
        let y0 = min[1].floor() as i32;
        let x1 = max[0].ceil() as i32;
        let y1 = max[1].ceil() as i32;
        Rectangle::new(
            (x0, y0).into(),
            (x1.saturating_sub(x0), y1.saturating_sub(y0)).into(),
        )
    }

    pub fn texture(&self) -> &GlesTexture {
        &self.texture
    }

    pub fn positions(&self) -> &[[f32; 2]] {
        &self.positions
    }

    pub fn texcoords(&self) -> &[[f32; 2]] {
        &self.texcoords
    }

    pub fn indices(&self) -> &[u16] {
        &self.indices
    }

    /// Сам GL-проход. Всё, что меняем, сохраняем и восстанавливаем: smithay
    /// считает своё GL-состояние неизменным между вызовами.
    ///
    /// # Safety
    /// `gl` — текущий контекст кадра, `program` собрана в нём.
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_gl(
        &self,
        gl: &ffi::Gles2,
        program: &MeshProgram,
        projection: &[f32; 9],
        instancing: bool,
        damage: &[Rectangle<i32, Physical>],
    ) {
        use ffi::types::{GLboolean, GLenum, GLint, GLuint};

        let tex_id = self.texture.tex_id();
        let opaque = self.texture.format().is_some_and(|f| !has_alpha(f));
        // y-inverted текстура хранится вверх ногами: v' = 1 - v.
        let tex_xform: [f32; 4] = if self.texture.is_y_inverted() {
            [1.0, -1.0, 0.0, 1.0]
        } else {
            [1.0, 1.0, 0.0, 0.0]
        };
        let attribs = [program.attr_position, program.attr_texcoord];
        let has_divisor = instancing && gl.VertexAttribDivisor.is_loaded();

        unsafe {
            let get_int = |pname: GLenum| {
                let mut v: GLint = 0;
                gl.GetIntegerv(pname, &mut v);
                v
            };

            // --- Сохранение состояния ---
            let prev_program = get_int(ffi::CURRENT_PROGRAM);
            let prev_array_buffer = get_int(ffi::ARRAY_BUFFER_BINDING);
            let prev_element_buffer = get_int(ffi::ELEMENT_ARRAY_BUFFER_BINDING);
            let prev_active_texture = get_int(ffi::ACTIVE_TEXTURE);
            gl.ActiveTexture(ffi::TEXTURE0);
            let prev_texture = get_int(ffi::TEXTURE_BINDING_2D);
            let prev_blend: GLboolean = gl.IsEnabled(ffi::BLEND);
            let prev_blend_func = [
                get_int(ffi::BLEND_SRC_RGB),
                get_int(ffi::BLEND_DST_RGB),
                get_int(ffi::BLEND_SRC_ALPHA),
                get_int(ffi::BLEND_DST_ALPHA),
            ];
            let prev_blend_eq = [
                get_int(ffi::BLEND_EQUATION_RGB),
                get_int(ffi::BLEND_EQUATION_ALPHA),
            ];
            let prev_scissor_test: GLboolean = gl.IsEnabled(ffi::SCISSOR_TEST);
            let mut prev_scissor_box = [0 as GLint; 4];
            gl.GetIntegerv(ffi::SCISSOR_BOX, prev_scissor_box.as_mut_ptr());
            let mut viewport = [0 as GLint; 4];
            gl.GetIntegerv(ffi::VIEWPORT, viewport.as_mut_ptr());
            let prev_cull: GLboolean = gl.IsEnabled(ffi::CULL_FACE);

            #[derive(Clone, Copy)]
            struct AttribState {
                enabled: GLint,
                size: GLint,
                kind: GLint,
                normalized: GLint,
                stride: GLint,
                buffer: GLint,
                pointer: *mut std::ffi::c_void,
                divisor: GLint,
            }
            let mut prev_attribs = [AttribState {
                enabled: 0,
                size: 4,
                kind: ffi::FLOAT as GLint,
                normalized: 0,
                stride: 0,
                buffer: 0,
                pointer: std::ptr::null_mut(),
                divisor: 0,
            }; 2];
            for (state, &loc) in prev_attribs.iter_mut().zip(&attribs) {
                gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_ENABLED, &mut state.enabled);
                gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_SIZE, &mut state.size);
                gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_TYPE, &mut state.kind);
                gl.GetVertexAttribiv(
                    loc,
                    ffi::VERTEX_ATTRIB_ARRAY_NORMALIZED,
                    &mut state.normalized,
                );
                gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_STRIDE, &mut state.stride);
                gl.GetVertexAttribiv(
                    loc,
                    ffi::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING,
                    &mut state.buffer,
                );
                // Биндинг объявлен с `*const *mut`, но GL пишет в него.
                gl.GetVertexAttribPointerv(
                    loc,
                    ffi::VERTEX_ATTRIB_ARRAY_POINTER,
                    (&raw mut state.pointer).cast_const(),
                );
                if has_divisor {
                    gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_DIVISOR, &mut state.divisor);
                }
            }

            // Параметры сэмплинга — состояние объекта текстуры, тоже вернём.
            gl.BindTexture(ffi::TEXTURE_2D, tex_id);
            let tex_params = [
                ffi::TEXTURE_MIN_FILTER,
                ffi::TEXTURE_MAG_FILTER,
                ffi::TEXTURE_WRAP_S,
                ffi::TEXTURE_WRAP_T,
            ];
            let mut prev_tex_params = [0 as GLint; 4];
            for (v, &p) in prev_tex_params.iter_mut().zip(&tex_params) {
                gl.GetTexParameteriv(ffi::TEXTURE_2D, p, v);
            }

            // --- Рисование ---
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MIN_FILTER,
                ffi::LINEAR as GLint,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MAG_FILTER,
                ffi::LINEAR as GLint,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as GLint,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as GLint,
            );

            gl.UseProgram(program.program);
            gl.UniformMatrix3fv(program.uni_matrix, 1, ffi::FALSE, projection.as_ptr());
            gl.Uniform4f(
                program.uni_tex_xform,
                tex_xform[0],
                tex_xform[1],
                tex_xform[2],
                tex_xform[3],
            );
            gl.Uniform1i(program.uni_tex, 0);
            gl.Uniform1f(program.uni_alpha, self.alpha);
            gl.Uniform1f(program.uni_opaque, if opaque { 1.0 } else { 0.0 });

            // Клиентские массивы вершин: буферы должны быть отвязаны.
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.BindBuffer(ffi::ELEMENT_ARRAY_BUFFER, 0);
            for (&loc, data) in attribs.iter().zip([&self.positions, &self.texcoords]) {
                gl.EnableVertexAttribArray(loc);
                gl.VertexAttribPointer(loc, 2, ffi::FLOAT, ffi::FALSE, 0, data.as_ptr().cast());
                if has_divisor {
                    // smithay оставляет divisor = 1 на своих атрибутах инстансинга.
                    gl.VertexAttribDivisor(loc, 0);
                }
            }

            gl.Enable(ffi::BLEND);
            gl.BlendEquation(ffi::FUNC_ADD);
            gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
            gl.Disable(ffi::CULL_FACE);
            gl.Enable(ffi::SCISSOR_TEST);

            // Рисуем только внутри damage: вне его в буфере лежит прошлый
            // кадр (buffer age), и повторное смешивание полупрозрачных пикселей
            // или затирание вышележащих элементов дало бы артефакты.
            let prev_box = if prev_scissor_test != ffi::FALSE {
                Some(prev_scissor_box)
            } else {
                None
            };
            for rect in damage {
                let Some([x, y, w, h]) = scissor_for(projection, viewport, prev_box, *rect) else {
                    continue;
                };
                gl.Scissor(x, y, w, h);
                gl.DrawElements(
                    ffi::TRIANGLES,
                    self.indices.len() as i32,
                    ffi::UNSIGNED_SHORT,
                    self.indices.as_ptr().cast(),
                );
            }

            // --- Восстановление ---
            for (state, &loc) in prev_attribs.iter().zip(&attribs) {
                gl.BindBuffer(ffi::ARRAY_BUFFER, state.buffer as GLuint);
                gl.VertexAttribPointer(
                    loc,
                    state.size,
                    state.kind as GLenum,
                    if state.normalized != 0 {
                        ffi::TRUE
                    } else {
                        ffi::FALSE
                    },
                    state.stride,
                    state.pointer,
                );
                if has_divisor {
                    gl.VertexAttribDivisor(loc, state.divisor as GLuint);
                }
                if state.enabled != 0 {
                    gl.EnableVertexAttribArray(loc);
                } else {
                    gl.DisableVertexAttribArray(loc);
                }
            }
            gl.BindBuffer(ffi::ARRAY_BUFFER, prev_array_buffer as GLuint);
            gl.BindBuffer(ffi::ELEMENT_ARRAY_BUFFER, prev_element_buffer as GLuint);

            for (v, &p) in prev_tex_params.iter().zip(&tex_params) {
                gl.TexParameteri(ffi::TEXTURE_2D, p, *v);
            }
            gl.BindTexture(ffi::TEXTURE_2D, prev_texture as GLuint);
            gl.ActiveTexture(prev_active_texture as GLenum);
            gl.UseProgram(prev_program as GLuint);

            gl.BlendEquationSeparate(prev_blend_eq[0] as GLenum, prev_blend_eq[1] as GLenum);
            gl.BlendFuncSeparate(
                prev_blend_func[0] as GLenum,
                prev_blend_func[1] as GLenum,
                prev_blend_func[2] as GLenum,
                prev_blend_func[3] as GLenum,
            );
            set_cap(gl, ffi::BLEND, prev_blend);
            set_cap(gl, ffi::CULL_FACE, prev_cull);
            gl.Scissor(
                prev_scissor_box[0],
                prev_scissor_box[1],
                prev_scissor_box[2],
                prev_scissor_box[3],
            );
            set_cap(gl, ffi::SCISSOR_TEST, prev_scissor_test);
        }
    }
}

/// Включает/выключает capability по сохранённому значению.
unsafe fn set_cap(gl: &ffi::Gles2, cap: ffi::types::GLenum, enabled: ffi::types::GLboolean) {
    unsafe {
        if enabled != ffi::FALSE {
            gl.Enable(cap);
        } else {
            gl.Disable(cap);
        }
    }
}

/// Прямоугольник damage (физические px выхода) → scissor в пикселях
/// framebuffer'а. Через ту же проекцию кадра, что и вершины, поэтому
/// корректно при любом transform выхода (поворот, отражение, y-flip GL).
fn scissor_for(
    projection: &[f32; 9],
    viewport: [i32; 4],
    clip: Option<[i32; 4]>,
    rect: Rectangle<i32, Physical>,
) -> Option<[i32; 4]> {
    let m = projection;
    let x0 = rect.loc.x as f32;
    let y0 = rect.loc.y as f32;
    let x1 = (rect.loc.x + rect.size.w) as f32;
    let y1 = (rect.loc.y + rect.size.h) as f32;
    let mut min = [f32::MAX; 2];
    let mut max = [f32::MIN; 2];
    for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        // Матрица column-major: ndc = M · (x, y, 1).
        let nx = m[0] * x + m[3] * y + m[6];
        let ny = m[1] * x + m[4] * y + m[7];
        let fx = viewport[0] as f32 + (nx + 1.0) * 0.5 * viewport[2] as f32;
        let fy = viewport[1] as f32 + (ny + 1.0) * 0.5 * viewport[3] as f32;
        min = [min[0].min(fx), min[1].min(fy)];
        max = [max[0].max(fx), max[1].max(fy)];
    }
    let mut bx0 = min[0].round() as i32;
    let mut by0 = min[1].round() as i32;
    let mut bx1 = max[0].round() as i32;
    let mut by1 = max[1].round() as i32;
    if let Some([cx, cy, cw, ch]) = clip {
        bx0 = bx0.max(cx);
        by0 = by0.max(cy);
        bx1 = bx1.min(cx.saturating_add(cw));
        by1 = by1.min(cy.saturating_add(ch));
    }
    (bx1 > bx0 && by1 > by0).then(|| [bx0, by0, bx1 - bx0, by1 - by0])
}

impl Element for MeshElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size().to_f64())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        // Вершины уже в физических пикселях.
        self.geometry
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Как у PhysicsElement: всегда вся геометрия (желе может шевелиться
        // внутри того же bbox) плюс прошлые bbox'ы для buffer age. Rect'ы —
        // относительно geometry().loc.
        let geo = self.geometry(scale);
        let mut rects = Vec::with_capacity(1 + self.damage_history.len());
        rects.push(Rectangle::from_size(geo.size));
        for prev in &self.damage_history {
            let mut rel = *prev;
            rel.loc -= geo.loc;
            if rel != Rectangle::from_size(geo.size) {
                rects.push(rel);
            }
        }
        DamageSet::from_slice(&rects)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Деформированное окно не прямоугольно — непрозрачных областей не заявляем.
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for MeshElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        if self.indices.is_empty() || damage.is_empty() || self.alpha <= 0.0 {
            return Ok(());
        }
        let Some(program) = mesh_program(frame)? else {
            return Ok(());
        };
        let projection = *frame.projection();
        let instancing = frame.capabilities().contains(&Capability::Instancing);
        // damage — относительно dst (геометрии элемента).
        let damage: Vec<Rectangle<i32, Physical>> = damage
            .iter()
            .map(|r| Rectangle::new(r.loc + dst.loc, r.size))
            .collect();
        frame.with_context(|gl| unsafe {
            self.draw_gl(gl, &program, &projection, instancing, &damage)
        })
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Никогда не на scanout-плоскость: там окно оказалось бы плоским.
        None
    }
}

// --- Снимок окна в текстуру ---

/// Рисует элементы (front-to-back, как их отдаёт smithay) в RGBA-текстуру
/// размера `bounds.size`; точка `bounds.loc` элементов попадает в (0,0)
/// текстуры. Текстура переиспользуется, если размер совпадает.
fn render_to_texture<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    reuse: Option<GlesTexture>,
    bounds: Rectangle<i32, Physical>,
    elements: &[E],
) -> Result<GlesTexture, GlesError> {
    let size = bounds.size;
    let buffer_size: Size<i32, Buffer> = (size.w, size.h).into();
    let mut texture = match reuse {
        Some(t) if t.size() == buffer_size => t,
        _ => {
            let t: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size)?;
            init_sampling(renderer, &t)?;
            t
        }
    };
    let sync = {
        let mut target = renderer.bind(&mut texture)?;
        // Transform::Normal: физический y = 0 → ряд 0 текстуры = верх окна.
        let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
        frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])?;
        let scale = Scale::from(1.0);
        for element in elements.iter().rev() {
            let mut dst = element.geometry(scale);
            dst.loc -= bounds.loc;
            // damage — в локальных координатах элемента (относительно dst).
            let damage = [Rectangle::from_size(dst.size)];
            element.draw(&mut frame, element.src(), dst, &damage, &[])?;
        }
        frame.finish()?
    };
    // Ожидание на стороне GPU: последующее сэмплирование снимка (в том числе
    // сырым GL из MeshElement, который не видит внутренних fence'ов
    // текстуры) начнётся только после окончания рендера в неё.
    renderer.wait(&sync)?;
    Ok(texture)
}

/// Линейная фильтрация и CLAMP_TO_EDGE для новой offscreen-текстуры
/// (по умолчанию у неё REPEAT — края смешивались бы с противоположными).
fn init_sampling(renderer: &mut GlesRenderer, texture: &GlesTexture) -> Result<(), GlesError> {
    renderer.with_context(|gl| unsafe {
        let mut prev: ffi::types::GLint = 0;
        gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut prev);
        gl.BindTexture(ffi::TEXTURE_2D, texture.tex_id());
        let linear = ffi::LINEAR as ffi::types::GLint;
        let clamp = ffi::CLAMP_TO_EDGE as ffi::types::GLint;
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, linear);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, linear);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, clamp);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, clamp);
        gl.BindTexture(ffi::TEXTURE_2D, prev as ffi::types::GLuint);
    })
}

/// Снимок окна: прямая (row 0 = верх) RGBA-текстура со всем деревом
/// поверхностей окна, premultiplied alpha, масштаб 1 (1 texel = 1 логический px).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub texture: GlesTexture,
    /// Размер текстуры (bbox дерева поверхностей), логические px.
    pub size: Size<i32, Logical>,
    /// Где в текстуре начало геометрии окна (`Window::geometry().loc`):
    /// у CSD-клиентов вокруг геометрии бывают тени.
    pub geometry_offset: Point<i32, Logical>,
    /// Размер геометрии окна.
    pub geometry_size: Size<i32, Logical>,
    /// Где в текстуре начало toplevel wl_surface (subsurface'ы могут
    /// торчать влево/вверх, тогда это не (0,0)).
    pub surface_offset: Point<i32, Logical>,
    /// Растёт при каждой перерисовке снимка — удобно для commit'а MeshElement.
    pub commit: CommitCounter,
}

impl Snapshot {
    /// uv по геометрии окна (0..1, как у `SoftMesh::uvs`) → texcoord текстуры
    /// снимка для [`MeshElement`].
    pub fn texcoord(&self, geometry_uv: [f32; 2]) -> [f32; 2] {
        let w = self.size.w.max(1) as f32;
        let h = self.size.h.max(1) as f32;
        [
            (self.geometry_offset.x as f32 + geometry_uv[0] * self.geometry_size.w as f32) / w,
            (self.geometry_offset.y as f32 + geometry_uv[1] * self.geometry_size.h as f32) / h,
        ]
    }
}

/// Подпись одного элемента поверхности: если ни одна не изменилась,
/// содержимое снимка актуально.
#[derive(Debug, Clone, PartialEq)]
struct ElementSig {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    src: Rectangle<f64, Buffer>,
    transform: Transform,
}

struct SnapshotEntry {
    snapshot: Snapshot,
    signature: Vec<ElementSig>,
}

/// Кэш снимков окон; перерисовывает снимок, только когда изменились
/// коммиты/геометрия поверхностей окна.
#[derive(Default)]
pub struct SnapshotCache {
    entries: HashMap<Window, SnapshotEntry>,
}

impl SnapshotCache {
    /// Актуальный снимок окна. `None`, если у окна нет отображённых
    /// поверхностей (ещё не было буфера / unmap) или рендер не удался.
    /// Вызывать вне кадра (между render pass'ами), как screencopy.
    pub fn snapshot(&mut self, renderer: &mut GlesRenderer, window: &Window) -> Option<&Snapshot> {
        let Some(surface) = window.wl_surface() else {
            self.entries.remove(window);
            return None;
        };
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                &surface,
                (0, 0),
                1.0,
                1.0,
                Kind::Unspecified,
            );
        let scale = Scale::from(1.0);
        let bounds = elements
            .iter()
            .map(|e| e.geometry(scale))
            .reduce(|a, b| a.merge(b))
            .filter(|b| !b.is_empty());
        let Some(bounds) = bounds else {
            self.entries.remove(window);
            return None;
        };
        let signature: Vec<ElementSig> = elements
            .iter()
            .map(|e| ElementSig {
                id: e.id().clone(),
                commit: e.current_commit(),
                geometry: e.geometry(scale),
                src: e.src(),
                transform: e.transform(),
            })
            .collect();

        let up_to_date = self
            .entries
            .get(window)
            .is_some_and(|e| e.signature == signature);
        if !up_to_date {
            let (reuse, mut commit) = match self.entries.get(window) {
                Some(e) => (Some(e.snapshot.texture.clone()), e.snapshot.commit),
                None => (None, CommitCounter::default()),
            };
            let texture = match render_to_texture(renderer, reuse, bounds, &elements) {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("[mesh] снимок окна не удался: {err:?}");
                    self.entries.remove(window);
                    return None;
                }
            };
            commit.increment();
            self.entries.insert(
                window.clone(),
                SnapshotEntry {
                    snapshot: Snapshot {
                        texture,
                        size: (bounds.size.w, bounds.size.h).into(),
                        geometry_offset: Point::default(),
                        geometry_size: Size::default(),
                        surface_offset: Point::default(),
                        commit,
                    },
                    signature,
                },
            );
        }

        // Геометрия окна может смениться и без перерисовки снимка.
        let geo = window.geometry();
        let entry = self.entries.get_mut(window)?;
        entry.snapshot.geometry_offset =
            (geo.loc.x - bounds.loc.x, geo.loc.y - bounds.loc.y).into();
        entry.snapshot.geometry_size = geo.size;
        entry.snapshot.surface_offset = (-bounds.loc.x, -bounds.loc.y).into();
        Some(&entry.snapshot)
    }

    /// Уже снятый снимок без перерисовки.
    pub fn get(&self, window: &Window) -> Option<&Snapshot> {
        self.entries.get(window).map(|e| &e.snapshot)
    }

    pub fn remove(&mut self, window: &Window) {
        self.entries.remove(window);
    }

    pub fn retain(&mut self, alive: impl Fn(&Window) -> bool) {
        self.entries.retain(|w, _| alive(w));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Тесты на настоящем GL без оконной системы: EGL device platform +
/// программный Mesa llvmpipe (EGL_MESA_device_software). Если такого
/// устройства нет — тест печатает причину и пропускается.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::softbody::{SoftMesh, SoftParams};
    use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::element::render_elements;
    use smithay::backend::renderer::element::solid::SolidColorRenderElement;
    use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
    use smithay::backend::renderer::{ExportMem, ImportMem};
    use std::sync::{Mutex, MutexGuard};

    render_elements! {
        TestElements<=GlesRenderer>;
        Mesh=MeshElement,
        Tex=TextureRenderElement<GlesTexture>,
        Solid=SolidColorRenderElement,
    }

    /// EGLDisplay устройства одна на процесс, а smithay делает eglTerminate
    /// при drop — параллельные GL-тесты убили бы друг другу дисплей.
    static GL_LOCK: Mutex<()> = Mutex::new(());

    fn gl_lock() -> MutexGuard<'static, ()> {
        GL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn headless_renderer() -> Option<GlesRenderer> {
        let devices = match EGLDevice::enumerate() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("ПРОПУСК GL-теста: EGL-устройства недоступны: {err}");
                return None;
            }
        };
        let Some(device) = devices.into_iter().find(|d| d.is_software()) else {
            eprintln!("ПРОПУСК GL-теста: нет программного EGL-устройства (llvmpipe)");
            return None;
        };
        let display = match unsafe { EGLDisplay::new(device) } {
            Ok(d) => d,
            Err(err) => {
                eprintln!("ПРОПУСК GL-теста: EGLDisplay: {err}");
                return None;
            }
        };
        let context = match EGLContext::new(&display) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("ПРОПУСК GL-теста: EGLContext: {err}");
                return None;
            }
        };
        match unsafe { GlesRenderer::new(context) } {
            Ok(r) => Some(r),
            Err(err) => {
                eprintln!("ПРОПУСК GL-теста: GlesRenderer: {err}");
                None
            }
        }
    }

    // Узор 64×48: четыре квадранта разных цветов, по ним видно и переворот,
    // и зеркалирование.
    const PW: i32 = 64;
    const PH: i32 = 48;
    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];
    const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

    /// Цвет узора в точке uv (0,0 — левый верхний угол картинки).
    fn quadrant(uv: [f32; 2]) -> [u8; 4] {
        match (uv[0] < 0.5, uv[1] < 0.5) {
            (true, true) => RED,
            (false, true) => GREEN,
            (true, false) => BLUE,
            (false, false) => WHITE,
        }
    }

    /// RGBA-байты узора (Fourcc::Abgr8888 — это R,G,B,A в памяти).
    /// `upside_down` — ряды в обратном порядке (для y-inverted импорта).
    fn pattern_bytes(upside_down: bool) -> Vec<u8> {
        let mut data = Vec::with_capacity((PW * PH * 4) as usize);
        for row in 0..PH {
            let y = if upside_down { PH - 1 - row } else { row };
            for x in 0..PW {
                let uv = [(x as f32 + 0.5) / PW as f32, (y as f32 + 0.5) / PH as f32];
                data.extend_from_slice(&quadrant(uv));
            }
        }
        data
    }

    fn import_pattern(renderer: &mut GlesRenderer, y_inverted: bool) -> GlesTexture {
        renderer
            .import_memory(
                &pattern_bytes(y_inverted),
                Fourcc::Abgr8888,
                (PW, PH).into(),
                y_inverted,
            )
            .expect("import_memory")
    }

    struct Image {
        w: i32,
        h: i32,
        data: Vec<u8>,
    }

    impl Image {
        /// Пиксель (x, y) в порядке, в котором вернул ReadPixels.
        fn px(&self, x: i32, y: i32) -> [u8; 4] {
            let i = ((y * self.w + x) * 4) as usize;
            [
                self.data[i],
                self.data[i + 1],
                self.data[i + 2],
                self.data[i + 3],
            ]
        }

        fn max_diff(&self, other: &Image) -> u8 {
            assert_eq!((self.w, self.h), (other.w, other.h));
            self.data
                .iter()
                .zip(&other.data)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0)
        }
    }

    fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
        a.iter().zip(&b).all(|(x, y)| x.abs_diff(*y) <= tol)
    }

    /// Readback текстуры как есть (ряд 0 = ряд 0 GL-текстуры).
    fn read_texture(renderer: &mut GlesRenderer, tex: &GlesTexture) -> Image {
        let size = tex.size();
        let mapping = renderer
            .copy_texture(tex, Rectangle::from_size(size), Fourcc::Abgr8888)
            .expect("copy_texture");
        let data = renderer
            .map_texture(&mapping)
            .expect("map_texture")
            .to_vec();
        Image {
            w: size.w,
            h: size.h,
            data,
        }
    }

    /// Рендер элементов штатным OutputDamageTracker'ом (как DrmCompositor) в
    /// offscreen-текстуру и readback.
    fn render_frame(
        renderer: &mut GlesRenderer,
        size: (i32, i32),
        transform: Transform,
        elements: &[TestElements],
    ) -> Image {
        let buffer_size: Size<i32, Buffer> = size.into();
        let mut target_tex: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, buffer_size)
            .expect("create_buffer");
        let mut tracker = OutputDamageTracker::new(size, 1.0, transform);
        let mut target = renderer.bind(&mut target_tex).expect("bind");
        tracker
            .render_output(renderer, &mut target, 0, elements, CLEAR)
            .expect("render_output");
        let mapping = renderer
            .copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let data = renderer
            .map_texture(&mapping)
            .expect("map_texture")
            .to_vec();
        Image {
            w: size.0,
            h: size.1,
            data,
        }
    }

    /// Ручной проход с явным damage (относительно геометрии элемента).
    fn render_manual(
        renderer: &mut GlesRenderer,
        size: (i32, i32),
        transform: Transform,
        element: &TestElements,
        damage: &[Rectangle<i32, Physical>],
        before_draw: impl FnOnce(&mut GlesFrame<'_, '_>),
        after_draw: impl FnOnce(&mut GlesFrame<'_, '_>),
    ) -> Image {
        let buffer_size: Size<i32, Buffer> = size.into();
        let mut target_tex: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, buffer_size)
            .expect("create_buffer");
        let mut target = renderer.bind(&mut target_tex).expect("bind");
        {
            // Размер кадра — режим выхода; при 90° логическое пространство повёрнуто.
            let mut frame = renderer
                .render(&mut target, size.into(), transform)
                .expect("render");
            let full = Rectangle::from_size(transform.transform_size(Size::from(size)));
            frame.clear(CLEAR.into(), &[full]).expect("clear");
            before_draw(&mut frame);
            let geo = element.geometry(Scale::from(1.0));
            element
                .draw(&mut frame, element.src(), geo, damage, &[])
                .expect("draw");
            after_draw(&mut frame);
            let _sync = frame.finish().expect("finish");
        }
        let mapping = renderer
            .copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let data = renderer
            .map_texture(&mapping)
            .expect("map_texture")
            .to_vec();
        Image {
            w: size.0,
            h: size.1,
            data,
        }
    }

    /// Плоский прямоугольник (x, y, w, h) с texcoords на всю текстуру.
    fn quad(tex: &GlesTexture, x: f32, y: f32, w: f32, h: f32, alpha: f32) -> MeshElement {
        MeshElement::new(
            Id::new(),
            CommitCounter::default(),
            tex.clone(),
            vec![[x, y], [x + w, y], [x, y + h], [x + w, y + h]],
            vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            vec![0, 1, 3, 0, 3, 2],
            alpha,
            Vec::new(),
        )
    }

    fn reference(renderer: &GlesRenderer, tex: &GlesTexture, x: f64, y: f64) -> TestElements {
        let buffer = TextureBuffer::from_texture(renderer, tex.clone(), 1, Transform::Normal, None);
        TestElements::Tex(TextureRenderElement::from_texture_buffer(
            (x, y),
            &buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ))
    }

    #[test]
    fn identity_mesh_is_upright_and_matches_smithay() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);

        // Эталон: штатный TextureRenderElement smithay. Он и задаёт, как
        // выглядит «прямое» окно в readback'е.
        let reference_el = reference(&renderer, &tex, 32.0, 24.0);
        let reference_img =
            render_frame(&mut renderer, (128, 96), Transform::Normal, &[reference_el]);
        assert_eq!(reference_img.px(40, 30), RED, "эталон: левый верх");
        assert_eq!(reference_img.px(90, 30), GREEN, "эталон: правый верх");
        assert_eq!(reference_img.px(40, 66), BLUE, "эталон: левый низ");
        assert_eq!(reference_img.px(90, 66), WHITE, "эталон: правый низ");
        assert_eq!(reference_img.px(5, 5), BLACK);

        let mesh_img = render_frame(
            &mut renderer,
            (128, 96),
            Transform::Normal,
            &[TestElements::Mesh(quad(&tex, 32.0, 24.0, 64.0, 48.0, 1.0))],
        );
        assert_eq!(mesh_img.px(40, 30), RED, "сетка: левый верх");
        assert_eq!(mesh_img.px(90, 30), GREEN, "сетка: правый верх");
        assert_eq!(mesh_img.px(40, 66), BLUE, "сетка: левый низ");
        assert_eq!(mesh_img.px(90, 66), WHITE, "сетка: правый низ");
        assert_eq!(mesh_img.px(5, 5), BLACK);
        assert_eq!(mesh_img.px(31, 30), BLACK, "ровно по границе квада");
        assert_eq!(mesh_img.px(32, 30), RED);
        assert!(
            mesh_img.max_diff(&reference_img) <= 1,
            "попиксельно как smithay"
        );

        // Программа собрана и закэширована в user_data контекста.
        let slot = renderer
            .egl_context()
            .user_data()
            .get::<MeshProgramCache>()
            .map(|c| c.0.get());
        assert!(matches!(slot, Some(ProgramSlot::Ready(_))), "{slot:?}");
    }

    #[test]
    fn output_transforms_match_smithay() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let reference_el = reference(&renderer, &tex, 20.0, 30.0);
            let reference_img = render_frame(&mut renderer, (128, 112), transform, &[reference_el]);
            let mesh_img = render_frame(
                &mut renderer,
                (128, 112),
                transform,
                &[TestElements::Mesh(quad(&tex, 20.0, 30.0, 64.0, 48.0, 1.0))],
            );
            assert!(
                mesh_img.max_diff(&reference_img) <= 1,
                "transform {transform:?}: сетка отличается от smithay"
            );
        }
    }

    #[test]
    fn y_inverted_texture_is_drawn_upright() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let flipped = import_pattern(&mut renderer, true);
        assert!(flipped.is_y_inverted());
        let img = render_frame(
            &mut renderer,
            (128, 96),
            Transform::Normal,
            &[TestElements::Mesh(quad(
                &flipped, 32.0, 24.0, 64.0, 48.0, 1.0,
            ))],
        );
        assert_eq!(img.px(40, 30), RED);
        assert_eq!(img.px(90, 30), GREEN);
        assert_eq!(img.px(40, 66), BLUE);
        assert_eq!(img.px(90, 66), WHITE);
        // Эталон здесь — та же сетка с обычной текстурой. (Штатный путь smithay
        // 0.7 для y_inverted делает v' = −v и полагается на REPEAT; у текстур с
        // CLAMP_TO_EDGE, как после import_memory, он размазывает крайний ряд.
        // Наш шейдер делает v' = 1 − v и от режима wrap не зависит.)
        let upright = import_pattern(&mut renderer, false);
        let reference_img = render_frame(
            &mut renderer,
            (128, 96),
            Transform::Normal,
            &[TestElements::Mesh(quad(
                &upright, 32.0, 24.0, 64.0, 48.0, 1.0,
            ))],
        );
        assert!(img.max_diff(&reference_img) <= 1);
    }

    #[test]
    fn deformed_mesh_matches_inverse_projection() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);

        // Желе 160×120, деформированное захватом угла и ударом.
        let mut mesh = SoftMesh::new(160.0, 120.0, 8);
        let params = SoftParams {
            softness: 0.8,
            impact_squash: 1.0,
            plasticity: 0.3,
        };
        let grab = mesh.grab([80.0, 60.0], 3.0).expect("угол");
        for i in 0..12 {
            let t = i as f32 / 12.0;
            mesh.drag(&grab, [80.0 + 30.0 * t, 60.0 + 20.0 * t], &params);
            mesh.step(
                1.0 / 60.0,
                [0.0, if i == 3 { -5.0e4 } else { 0.0 }],
                1.0,
                &params,
            );
        }
        let center = [128.0f32, 96.0];
        let positions: Vec<[f32; 2]> = mesh
            .positions()
            .iter()
            .map(|p| [p[0] + center[0], p[1] + center[1]])
            .collect();
        let element = MeshElement::new(
            Id::new(),
            CommitCounter::default(),
            tex.clone(),
            positions,
            mesh.uvs().to_vec(),
            mesh.indices().to_vec(),
            1.0,
            Vec::new(),
        );
        let geo = element.geometry(Scale::from(1.0));
        let img = render_frame(
            &mut renderer,
            (256, 192),
            Transform::Normal,
            &[TestElements::Mesh(element)],
        );

        // Под каждым пикселем должен быть ровно тот цвет буфера, который
        // даёт обратная проекция (§6) — «кликаем туда, что видим».
        let mut checked = 0;
        let mut outside = 0;
        for py in (0..192).step_by(2) {
            for px in (0..256).step_by(2) {
                let local = [px as f32 + 0.5 - center[0], py as f32 + 0.5 - center[1]];
                match mesh.local_to_uv(local, false) {
                    Some(uv) => {
                        let margin = 0.06;
                        let interior = uv[0] > margin
                            && uv[0] < 1.0 - margin
                            && uv[1] > margin
                            && uv[1] < 1.0 - margin;
                        let off_seam = (uv[0] - 0.5).abs() > margin && (uv[1] - 0.5).abs() > margin;
                        if interior && off_seam {
                            let got = img.px(px, py);
                            let want = quadrant(uv);
                            assert!(
                                close(got, want, 8),
                                "пиксель ({px},{py}) uv {uv:?}: {got:?} вместо {want:?}"
                            );
                            checked += 1;
                        }
                    }
                    None => {
                        // Заведомо вне bbox сетки — фон.
                        let outside_bbox = px < geo.loc.x - 2
                            || py < geo.loc.y - 2
                            || px > geo.loc.x + geo.size.w + 2
                            || py > geo.loc.y + geo.size.h + 2;
                        if outside_bbox {
                            assert_eq!(img.px(px, py), BLACK, "фон в ({px},{py})");
                            outside += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 2000, "проверено пикселей сетки: {checked}");
        assert!(outside > 1000, "проверено пикселей фона: {outside}");
    }

    #[test]
    fn draw_is_clipped_to_damage() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        for transform in [Transform::Normal, Transform::_90, Transform::Flipped180] {
            // damage относительно геометрии элемента: кусок у правого верхнего
            // угла и полоса по низу.
            let damage = [
                Rectangle::new((40, 4).into(), (16, 12).into()),
                Rectangle::new((0, 40).into(), (64, 5).into()),
            ];
            let mesh = TestElements::Mesh(quad(&tex, 20.0, 30.0, 64.0, 48.0, 1.0));
            let mesh_img = render_manual(
                &mut renderer,
                (128, 112),
                transform,
                &mesh,
                &damage,
                |_| {},
                |_| {},
            );
            // Эталон: smithay сам режет отрисовку текстуры по damage.
            let reference_el = reference(&renderer, &tex, 20.0, 30.0);
            let reference_img = render_manual(
                &mut renderer,
                (128, 112),
                transform,
                &reference_el,
                &damage,
                |_| {},
                |_| {},
            );
            assert!(
                mesh_img.max_diff(&reference_img) <= 1,
                "transform {transform:?}: обрезка по damage отличается от smithay"
            );
            let colored = (0..112)
                .flat_map(|y| (0..128).map(move |x| (x, y)))
                .filter(|&(x, y)| mesh_img.px(x, y) != BLACK)
                .count();
            assert_eq!(colored, 16 * 12 + 64 * 5, "transform {transform:?}");
        }
    }

    #[test]
    fn gl_state_is_restored() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        let mesh = TestElements::Mesh(quad(&tex, 10.0, 10.0, 64.0, 48.0, 0.5));
        type Snap = (Vec<i32>, [u8; 3]);
        let snap = |gl: &ffi::Gles2| -> Snap {
            let mut v = Vec::new();
            let get = |p: ffi::types::GLenum| {
                let mut x = 0;
                unsafe { gl.GetIntegerv(p, &mut x) };
                x
            };
            for p in [
                ffi::CURRENT_PROGRAM,
                ffi::ARRAY_BUFFER_BINDING,
                ffi::ELEMENT_ARRAY_BUFFER_BINDING,
                ffi::ACTIVE_TEXTURE,
                ffi::BLEND_SRC_RGB,
                ffi::BLEND_DST_RGB,
                ffi::BLEND_SRC_ALPHA,
                ffi::BLEND_DST_ALPHA,
                ffi::BLEND_EQUATION_RGB,
                ffi::BLEND_EQUATION_ALPHA,
            ] {
                v.push(get(p));
            }
            let mut sb = [0; 4];
            unsafe { gl.GetIntegerv(ffi::SCISSOR_BOX, sb.as_mut_ptr()) };
            v.extend_from_slice(&sb);
            for loc in 0..4 {
                let mut e = 0;
                unsafe { gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_ENABLED, &mut e) };
                v.push(e);
                let mut d = 0;
                unsafe { gl.GetVertexAttribiv(loc, ffi::VERTEX_ATTRIB_ARRAY_DIVISOR, &mut d) };
                v.push(d);
            }
            let caps = unsafe {
                [
                    gl.IsEnabled(ffi::BLEND),
                    gl.IsEnabled(ffi::SCISSOR_TEST),
                    gl.IsEnabled(ffi::CULL_FACE),
                ]
            };
            (v, caps)
        };
        let before = std::cell::RefCell::new(None);
        let after = std::cell::RefCell::new(None);
        let _ = render_manual(
            &mut renderer,
            (128, 96),
            Transform::Normal,
            &mesh,
            &[Rectangle::new((5, 5).into(), (30, 20).into())],
            |frame| {
                // Нестандартное состояние, чтобы проверить именно восстановление.
                let s = frame
                    .with_context(|gl| unsafe {
                        gl.ActiveTexture(ffi::TEXTURE3);
                        gl.BlendFuncSeparate(ffi::SRC_ALPHA, ffi::ZERO, ffi::ONE, ffi::ZERO);
                        gl.Enable(ffi::CULL_FACE);
                        gl.Scissor(1, 2, 100, 90);
                        gl.VertexAttribDivisor(1, 1);
                        gl.EnableVertexAttribArray(1);
                        snap(gl)
                    })
                    .expect("with_context");
                *before.borrow_mut() = Some(s);
            },
            |frame| {
                let s = frame.with_context(|gl| snap(gl)).expect("with_context");
                // Возвращаем как было у smithay.
                frame
                    .with_context(|gl| unsafe {
                        gl.ActiveTexture(ffi::TEXTURE0);
                        gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
                        gl.Disable(ffi::CULL_FACE);
                        gl.DisableVertexAttribArray(1);
                        gl.VertexAttribDivisor(1, 0);
                    })
                    .expect("with_context");
                *after.borrow_mut() = Some(s);
            },
        );
        let before = before.into_inner().expect("снимок до");
        let after = after.into_inner().expect("снимок после");
        assert_eq!(before, after, "GL-состояние не восстановлено");
        let err = renderer
            .with_context(|gl| unsafe { gl.GetError() })
            .expect("with_context");
        assert_eq!(err, ffi::NO_ERROR, "GL-ошибка после отрисовки");
    }

    #[test]
    fn offscreen_snapshot_is_upright() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        // Тот же путь, что у SnapshotCache: узор + «subsurface» (жёлтый
        // квадрат поверх, торчит влево-вверх → bbox с отрицательным началом).
        let pattern = reference(&renderer, &tex, 0.0, 0.0);
        let solid = TestElements::Solid(SolidColorRenderElement::new(
            Id::new(),
            Rectangle::new((-4, -4).into(), (8, 8).into()),
            CommitCounter::default(),
            [1.0, 1.0, 0.0, 1.0],
            Kind::Unspecified,
        ));
        let elements = [solid, pattern]; // front-to-back, как у smithay
        let bounds = elements
            .iter()
            .map(|e| e.geometry(Scale::from(1.0)))
            .reduce(|a, b| a.merge(b))
            .expect("bounds");
        assert_eq!(bounds, Rectangle::new((-4, -4).into(), (68, 52).into()));
        let snapshot = render_to_texture(&mut renderer, None, bounds, &elements).expect("snapshot");
        assert_eq!(snapshot.size(), (68, 52).into());
        assert!(!snapshot.is_y_inverted());

        // Сырые ряды текстуры: ряд 0 — верх окна.
        let raw = read_texture(&mut renderer, &snapshot);
        assert_eq!(raw.px(2, 2), YELLOW, "subsurface слева сверху");
        assert_eq!(raw.px(6, 6), YELLOW, "subsurface поверх узора");
        assert_eq!(raw.px(14, 10), RED);
        assert_eq!(raw.px(60, 10), GREEN);
        assert_eq!(raw.px(14, 45), BLUE);
        assert_eq!(raw.px(60, 45), WHITE);
        assert_eq!(raw.px(66, 1), [0, 0, 0, 0], "пустой угол bbox прозрачен");

        // Главное: сетка, сэмплящая снимок texcoord'ами (0,0)=верх-лево,
        // показывает окно прямо. Квад 68×52 в (30, 20).
        let img = render_frame(
            &mut renderer,
            (128, 96),
            Transform::Normal,
            &[TestElements::Mesh(quad(
                &snapshot, 30.0, 20.0, 68.0, 52.0, 1.0,
            ))],
        );
        assert_eq!(img.px(32, 22), YELLOW);
        assert_eq!(img.px(44, 30), RED);
        assert_eq!(img.px(90, 30), GREEN);
        assert_eq!(img.px(44, 65), BLUE);
        assert_eq!(img.px(90, 65), WHITE);
        assert_eq!(img.px(96, 21), BLACK, "прозрачный угол снимка → фон");

        // Переиспользование текстуры того же размера.
        let id = snapshot.tex_id();
        let again =
            render_to_texture(&mut renderer, Some(snapshot), bounds, &elements).expect("снова");
        assert_eq!(again.tex_id(), id);
        assert_eq!(read_texture(&mut renderer, &again).max_diff(&raw), 0);
    }

    #[test]
    fn snapshot_texcoord_maps_geometry() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        let snap = Snapshot {
            texture: tex,
            size: (100, 80).into(),
            geometry_offset: (10, 20).into(),
            geometry_size: (80, 40).into(),
            surface_offset: (0, 0).into(),
            commit: CommitCounter::default(),
        };
        assert_eq!(snap.texcoord([0.0, 0.0]), [0.1, 0.25]);
        assert_eq!(snap.texcoord([1.0, 1.0]), [0.9, 0.75]);
    }

    #[test]
    fn invalid_mesh_input_is_sanitized() {
        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let tex = import_pattern(&mut renderer, false);
        let el = MeshElement::new(
            Id::new(),
            CommitCounter::default(),
            tex,
            vec![[0.0, 0.0], [10.0, 0.0], [0.0, 10.0], [f32::NAN, 5.0]],
            vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            vec![0, 1, 2, 0, 2, 3, 1, 2, 99, 0, 1],
            f32::NAN,
            Vec::new(),
        );
        assert_eq!(el.positions().len(), 3);
        assert_eq!(el.indices(), &[0, 1, 2]);
        assert_eq!(el.alpha(), 1.0);
        assert_eq!(
            el.geometry(Scale::from(1.0)),
            Rectangle::new((0, 0).into(), (10, 10).into())
        );
        // Пустая сетка рисуется как ничего.
        let empty = MeshElement::new(
            Id::new(),
            CommitCounter::default(),
            el.texture().clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1.0,
            Vec::new(),
        );
        let img = render_frame(
            &mut renderer,
            (32, 32),
            Transform::Normal,
            &[TestElements::Mesh(empty)],
        );
        assert_eq!(img.px(16, 16), BLACK);
    }

    // --- Настоящее дерево поверхностей: мини-композитор + клиент «на проводе» ---

    mod wl {
        use smithay::backend::renderer::utils::on_commit_buffer_handler;
        use smithay::desktop::Window;
        use smithay::input::{SeatHandler, SeatState};
        use smithay::reexports::rustix::net::{
            SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg,
        };
        use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
        use smithay::reexports::wayland_server::protocol::{
            wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface,
        };
        use smithay::reexports::wayland_server::{Client, Display};
        use smithay::utils::Serial;
        use smithay::wayland::buffer::BufferHandler;
        use smithay::wayland::compositor::{
            CompositorClientState, CompositorHandler, CompositorState,
        };
        use smithay::wayland::shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        };
        use smithay::wayland::shm::{ShmHandler, ShmState};
        use std::io::{IoSlice, Read, Write};
        use std::mem::MaybeUninit;
        use std::os::fd::BorrowedFd;
        use std::os::unix::net::UnixStream;

        pub struct State {
            compositor: CompositorState,
            xdg: XdgShellState,
            shm: ShmState,
            seat: SeatState<State>,
            pub toplevels: Vec<ToplevelSurface>,
            pub windows: Vec<Window>,
        }

        #[derive(Default)]
        pub struct ClientState {
            compositor: CompositorClientState,
        }

        impl ClientData for ClientState {
            fn initialized(&self, _client_id: ClientId) {}
            fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
        }

        impl CompositorHandler for State {
            fn compositor_state(&mut self) -> &mut CompositorState {
                &mut self.compositor
            }
            fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
                &client
                    .get_data::<ClientState>()
                    .expect("данные клиента")
                    .compositor
            }
            fn commit(&mut self, surface: &WlSurface) {
                on_commit_buffer_handler::<Self>(surface);
                for w in &self.windows {
                    w.on_commit();
                }
                if let Some(t) = self.toplevels.iter().find(|t| t.wl_surface() == surface)
                    && !t.is_initial_configure_sent()
                {
                    t.send_configure();
                }
            }
        }

        impl BufferHandler for State {
            fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
        }

        impl ShmHandler for State {
            fn shm_state(&self) -> &ShmState {
                &self.shm
            }
        }

        impl XdgShellHandler for State {
            fn xdg_shell_state(&mut self) -> &mut XdgShellState {
                &mut self.xdg
            }
            fn new_toplevel(&mut self, surface: ToplevelSurface) {
                self.toplevels.push(surface);
            }
            fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
            fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
            fn reposition_request(
                &mut self,
                _surface: PopupSurface,
                _positioner: PositionerState,
                _token: u32,
            ) {
            }
        }

        // xdg_popup grab требует сиденье, хотя тесту оно не нужно.
        impl SeatHandler for State {
            type KeyboardFocus = WlSurface;
            type PointerFocus = WlSurface;
            type TouchFocus = WlSurface;
            fn seat_state(&mut self) -> &mut SeatState<Self> {
                &mut self.seat
            }
        }

        smithay::delegate_compositor!(State);
        smithay::delegate_shm!(State);
        smithay::delegate_xdg_shell!(State);
        smithay::delegate_seat!(State);

        pub fn server() -> (Display<State>, State) {
            let display = Display::<State>::new().expect("wl_display");
            let dh = display.handle();
            let state = State {
                compositor: CompositorState::new::<State>(&dh),
                xdg: XdgShellState::new::<State>(&dh),
                shm: ShmState::new::<State>(&dh, Vec::new()),
                seat: SeatState::new(),
                toplevels: Vec::new(),
                windows: Vec::new(),
            };
            (display, state)
        }

        /// Несколько раундов dispatch: recvmsg на stream-сокете останавливается
        /// на границе сообщения с SCM_RIGHTS, а libwayland читает один раз за
        /// dispatch — запросы после create_pool иначе ждали бы следующего раунда.
        pub fn pump(display: &mut Display<State>, state: &mut State) {
            for _ in 0..4 {
                display.dispatch_clients(state).expect("dispatch_clients");
                display.flush_clients().expect("flush_clients");
            }
        }

        /// Аргумент запроса.
        pub enum A<'a> {
            U(u32),
            I(i32),
            S(&'a str),
        }

        /// Сырой Wayland-клиент: ровно столько протокола, сколько нужно тесту.
        pub struct Wire {
            sock: UnixStream,
            next_id: u32,
            inbox: Vec<u8>,
        }

        pub struct Event {
            pub obj: u32,
            pub opcode: u16,
            pub args: Vec<u8>,
        }

        impl Event {
            pub fn u32(&self, word: usize) -> u32 {
                let b = &self.args[word * 4..word * 4 + 4];
                u32::from_ne_bytes([b[0], b[1], b[2], b[3]])
            }
            /// Строка, начинающаяся со слова `word`, и слово после неё.
            pub fn str(&self, word: usize) -> (String, usize) {
                let len = self.u32(word) as usize;
                let start = (word + 1) * 4;
                let s = &self.args[start..start + len.saturating_sub(1)];
                (
                    String::from_utf8_lossy(s).into_owned(),
                    word + 1 + len.div_ceil(4),
                )
            }
        }

        impl Wire {
            pub fn connect(display: &mut Display<State>) -> Self {
                let (server, client) = UnixStream::pair().expect("socketpair");
                display
                    .handle()
                    .insert_client(server, std::sync::Arc::new(ClientState::default()))
                    .expect("insert_client");
                client.set_nonblocking(true).expect("nonblocking");
                Self {
                    sock: client,
                    next_id: 2,
                    inbox: Vec::new(),
                }
            }

            pub fn new_id(&mut self) -> u32 {
                let id = self.next_id;
                self.next_id += 1;
                id
            }

            pub fn send(
                &mut self,
                obj: u32,
                opcode: u16,
                args: &[A<'_>],
                fd: Option<BorrowedFd<'_>>,
            ) {
                let mut body = Vec::new();
                for a in args {
                    match a {
                        A::U(v) => body.extend_from_slice(&v.to_ne_bytes()),
                        A::I(v) => body.extend_from_slice(&v.to_ne_bytes()),
                        A::S(s) => {
                            body.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
                            body.extend_from_slice(s.as_bytes());
                            body.push(0);
                            while !body.len().is_multiple_of(4) {
                                body.push(0);
                            }
                        }
                    }
                }
                let size = 8 + body.len() as u32;
                let mut msg = Vec::with_capacity(size as usize);
                msg.extend_from_slice(&obj.to_ne_bytes());
                msg.extend_from_slice(&((size << 16) | opcode as u32).to_ne_bytes());
                msg.extend_from_slice(&body);
                match fd {
                    Some(fd) => {
                        let mut space = [MaybeUninit::<u8>::uninit(); 64];
                        let mut cmsg = SendAncillaryBuffer::new(&mut space);
                        let fds = [fd];
                        assert!(cmsg.push(SendAncillaryMessage::ScmRights(&fds)));
                        let n = sendmsg(
                            &self.sock,
                            &[IoSlice::new(&msg)],
                            &mut cmsg,
                            SendFlags::empty(),
                        )
                        .expect("sendmsg");
                        assert_eq!(n, msg.len());
                    }
                    None => self.sock.write_all(&msg).expect("write"),
                }
            }

            /// Всё, что сервер уже прислал. wl_display.error — сразу паника.
            pub fn events(&mut self) -> Vec<Event> {
                let mut buf = [0u8; 4096];
                loop {
                    match self.sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => self.inbox.extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("read: {e}"),
                    }
                }
                let mut out = Vec::new();
                while self.inbox.len() >= 8 {
                    let word = |i: usize| {
                        u32::from_ne_bytes([
                            self.inbox[i],
                            self.inbox[i + 1],
                            self.inbox[i + 2],
                            self.inbox[i + 3],
                        ])
                    };
                    let obj = word(0);
                    let size = (word(4) >> 16) as usize;
                    let opcode = (word(4) & 0xffff) as u16;
                    if size < 8 || self.inbox.len() < size {
                        break;
                    }
                    let ev = Event {
                        obj,
                        opcode,
                        args: self.inbox[8..size].to_vec(),
                    };
                    self.inbox.drain(..size);
                    if ev.obj == 1 && ev.opcode == 0 {
                        let (msg, _) = ev.str(2);
                        panic!(
                            "ошибка протокола: объект {} код {}: {msg}",
                            ev.u32(0),
                            ev.u32(1)
                        );
                    }
                    out.push(ev);
                }
                out
            }
        }
    }

    /// Пиксель ARGB8888 для wl_shm (в памяти little-endian: B, G, R, A).
    fn shm_px(c: [u8; 4]) -> [u8; 4] {
        [c[2], c[1], c[0], c[3]]
    }

    #[test]
    fn snapshot_cache_real_surface_tree() {
        use smithay::reexports::rustix::fs::{MemfdFlags, memfd_create};
        use std::io::Write as _;
        use std::os::fd::AsFd;
        use wl::A;

        let _lock = gl_lock();
        let Some(mut renderer) = headless_renderer() else {
            return;
        };
        let (mut display, mut state) = wl::server();
        let mut c = wl::Wire::connect(&mut display);

        // Глобалы.
        let registry = c.new_id();
        c.send(1, 1, &[A::U(registry)], None); // wl_display.get_registry
        wl::pump(&mut display, &mut state);
        let mut globals = std::collections::HashMap::new();
        for ev in c.events() {
            if ev.obj == registry && ev.opcode == 0 {
                let (iface, next) = ev.str(1);
                globals.insert(iface, (ev.u32(0), ev.u32(next)));
            }
        }
        let bind = |c: &mut wl::Wire, iface: &str, version: u32| {
            let (name, max) = *globals.get(iface).unwrap_or_else(|| panic!("нет {iface}"));
            let id = c.new_id();
            let v = version.min(max);
            c.send(
                registry,
                0,
                &[A::U(name), A::S(iface), A::U(v), A::U(id)],
                None,
            );
            id
        };
        let compositor = bind(&mut c, "wl_compositor", 4);
        let subcompositor = bind(&mut c, "wl_subcompositor", 1);
        let shm = bind(&mut c, "wl_shm", 1);
        let wm_base = bind(&mut c, "xdg_wm_base", 1);

        // Окно 64×48 + subsurface 16×12, торчащая на (−6, −4) влево-вверх.
        let main = c.new_id();
        c.send(compositor, 0, &[A::U(main)], None);
        let child = c.new_id();
        c.send(compositor, 0, &[A::U(child)], None);
        let sub = c.new_id();
        c.send(
            subcompositor,
            1,
            &[A::U(sub), A::U(child), A::U(main)],
            None,
        );
        c.send(sub, 1, &[A::I(-6), A::I(-4)], None); // set_position
        c.send(sub, 5, &[], None); // set_desync
        let xdg_surface = c.new_id();
        c.send(wm_base, 2, &[A::U(xdg_surface), A::U(main)], None);
        let toplevel = c.new_id();
        c.send(xdg_surface, 1, &[A::U(toplevel)], None);
        c.send(
            xdg_surface,
            3,
            &[A::I(0), A::I(0), A::I(64), A::I(48)],
            None,
        );
        c.send(main, 6, &[], None); // начальный commit без буфера
        wl::pump(&mut display, &mut state);
        let serial = c
            .events()
            .into_iter()
            .find(|e| e.obj == xdg_surface && e.opcode == 0)
            .map(|e| e.u32(0))
            .expect("xdg_surface.configure");
        c.send(xdg_surface, 4, &[A::U(serial)], None); // ack_configure

        // SHM-пул: узор окна, жёлтая subsurface, пурпурный второй кадр.
        let mut pool_bytes = Vec::new();
        for y in 0..PH {
            for x in 0..PW {
                let uv = [(x as f32 + 0.5) / PW as f32, (y as f32 + 0.5) / PH as f32];
                pool_bytes.extend_from_slice(&shm_px(quadrant(uv)));
            }
        }
        let child_off = pool_bytes.len() as i32;
        for _ in 0..16 * 12 {
            pool_bytes.extend_from_slice(&shm_px(YELLOW));
        }
        let magenta_off = pool_bytes.len() as i32;
        for _ in 0..PW * PH {
            pool_bytes.extend_from_slice(&shm_px([255, 0, 255, 255]));
        }
        let mut memfd = std::fs::File::from(
            memfd_create("sandboxwm-test", MemfdFlags::CLOEXEC).expect("memfd"),
        );
        memfd.write_all(&pool_bytes).expect("memfd write");
        let pool = c.new_id();
        c.send(
            shm,
            0,
            &[A::U(pool), A::I(pool_bytes.len() as i32)],
            Some(memfd.as_fd()),
        );
        let buffer = |c: &mut wl::Wire, off: i32, w: i32, h: i32| {
            let id = c.new_id();
            // wl_shm_pool.create_buffer, формат 0 = ARGB8888
            c.send(
                pool,
                0,
                &[A::U(id), A::I(off), A::I(w), A::I(h), A::I(w * 4), A::U(0)],
                None,
            );
            id
        };
        let buf_main = buffer(&mut c, 0, PW, PH);
        let buf_child = buffer(&mut c, child_off, 16, 12);
        let buf_magenta = buffer(&mut c, magenta_off, PW, PH);
        let attach = |c: &mut wl::Wire, surface: u32, buf: u32, w: i32, h: i32| {
            c.send(surface, 1, &[A::U(buf), A::I(0), A::I(0)], None);
            c.send(surface, 2, &[A::I(0), A::I(0), A::I(w), A::I(h)], None);
            c.send(surface, 6, &[], None);
        };
        attach(&mut c, child, buf_child, 16, 12);
        attach(&mut c, main, buf_main, PW, PH);
        wl::pump(&mut display, &mut state);
        let _ = c.events();

        let window = Window::new_wayland_window(state.toplevels[0].clone());
        window.on_commit();
        state.windows.push(window.clone());
        assert_eq!(
            window.geometry(),
            Rectangle::new((0, 0).into(), (64, 48).into())
        );
        assert_eq!(
            window.bbox(),
            Rectangle::new((-6, -4).into(), (70, 52).into())
        );

        let mut cache = SnapshotCache::default();
        let snap = cache
            .snapshot(&mut renderer, &window)
            .expect("снимок")
            .clone();
        assert_eq!(snap.size, (70, 52).into());
        assert_eq!(snap.geometry_offset, (6, 4).into());
        assert_eq!(snap.surface_offset, (6, 4).into());
        assert_eq!(snap.geometry_size, (64, 48).into());
        assert_eq!(snap.texture.size(), (70, 52).into());
        assert!(!snap.texture.is_y_inverted());
        let raw = read_texture(&mut renderer, &snap.texture);
        assert_eq!(raw.px(2, 2), YELLOW, "subsurface");
        assert_eq!(raw.px(14, 10), YELLOW, "subsurface поверх окна");
        assert_eq!(raw.px(26, 19), RED, "окно: левый верх");
        assert_eq!(raw.px(56, 19), GREEN, "окно: правый верх");
        assert_eq!(raw.px(26, 39), BLUE, "окно: левый низ");
        assert_eq!(raw.px(56, 39), WHITE, "окно: правый низ");
        assert_eq!(raw.px(68, 1), [0, 0, 0, 0], "вне поверхностей — прозрачно");
        // texcoord для uv геометрии окна.
        let tc = snap.texcoord([0.0, 0.0]);
        assert!((tc[0] - 6.0 / 70.0).abs() < 1e-6 && (tc[1] - 4.0 / 52.0).abs() < 1e-6);

        // Без новых коммитов снимок не перерисовывается.
        let again = cache
            .snapshot(&mut renderer, &window)
            .expect("снимок")
            .clone();
        assert_eq!(again.commit, snap.commit);
        assert_eq!(again.texture.tex_id(), snap.texture.tex_id());

        // Новый буфер у окна → перерисовка в ту же текстуру.
        attach(&mut c, main, buf_magenta, PW, PH);
        wl::pump(&mut display, &mut state);
        let _ = c.events();
        let fresh = cache
            .snapshot(&mut renderer, &window)
            .expect("снимок")
            .clone();
        assert_ne!(fresh.commit, snap.commit);
        assert_eq!(fresh.texture.tex_id(), snap.texture.tex_id());
        let raw = read_texture(&mut renderer, &fresh.texture);
        assert_eq!(raw.px(26, 19), [255, 0, 255, 255]);
        assert_eq!(raw.px(2, 2), YELLOW);

        // Окно без поверхностей/исчезло — кэш чистится.
        cache.retain(|_| false);
        assert!(cache.is_empty());
    }
}
