//! Render-элементы и сборка кадра.
//!
//! В тайлинге окна рисуются как есть, обрезанные по своему тайлу (клиент,
//! не умеющий сжаться до тайла, не налезает на соседей). В физическом режиме каждое окно
//! состоит из одного или нескольких кусков (разрезание ножом) и рисуется
//! [`PhysicsElement`]-ами: текстура поверхности, повёрнутая/масштабированная
//! аффинным преобразованием «координаты окна → экран» и обрезанная по
//! прямоугольнику куска. Поверх — оверлей (линии, верёвки, связи, пол).
//!
//! Damage: у каждого элемента свой commit, который растёт только когда
//! меняется буфер поверхности или трансформа куска. Раньше все физические
//! элементы делили общий счётчик и отдавали полный damage каждый кадр — любое
//! движение мыши в DRM-режиме перекомпоновывало все окна целиком.
//!
//! Слои layer-shell (панели, фоны) и полноэкранное окно рисуются в экранных
//! координатах в обоих режимах.

use std::collections::HashMap;

use cgmath::Matrix3;
use smithay::backend::renderer::{
    element::{
        Element, Id, Kind, RenderElement, UnderlyingStorage, render_elements,
        solid::SolidColorRenderElement,
        surface::{
            WaylandSurfaceRenderElement, WaylandSurfaceTexture, render_elements_from_surface_tree,
        },
        utils::CropRenderElement,
    },
    gles::{GlesError, GlesFrame, GlesRenderer},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::desktop::{PopupManager, Window};
use smithay::output::Output;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::seat::WaylandFocus;

use crate::overlay::OverlayGeometry;
use crate::state::AppState;

render_elements! {
    pub CustomRenderElements<=GlesRenderer>;
    /// Поверхность как есть: окна тайлинга/полноэкранное, слои, курсор.
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    /// Поверхность окна, обрезанная по тайлу.
    Cropped=CropRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor=SolidColorRenderElement,
    CursorImage=smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<GlesRenderer>,
    Physics=PhysicsElement,
    Overlay=crate::overlay::OverlayElement,
    Mesh=crate::mesh_render::MeshElement,
}

/// Аффинное преобразование 2D: `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Кусок окна: точка `rect_center` (координаты окна) переходит в
    /// `center` (экран), с поворотом `angle` и масштабом `scale`.
    pub fn piece(center: (f64, f64), angle: f64, scale: f64, rect_center: (f64, f64)) -> Self {
        let (s, c) = angle.sin_cos();
        let a = scale * c;
        let b = scale * s;
        Affine {
            a,
            b,
            c: -b,
            d: a,
            e: center.0 - (a * rect_center.0 - b * rect_center.1),
            f: center.1 - (b * rect_center.0 + a * rect_center.1),
        }
    }

    pub fn translation(x: f64, y: f64) -> Self {
        Affine {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `self ∘ other` (сначала `other`).
    pub fn then_after(&self, other: &Affine) -> Affine {
        Affine {
            a: self.a * other.a + self.c * other.b,
            b: self.b * other.a + self.d * other.b,
            c: self.a * other.c + self.c * other.d,
            d: self.b * other.c + self.d * other.d,
            e: self.a * other.e + self.c * other.f + self.e,
            f: self.b * other.e + self.d * other.f + self.f,
        }
    }

    pub fn inverse(&self) -> Option<Affine> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-12 {
            return None;
        }
        let ia = self.d / det;
        let ib = -self.b / det;
        let ic = -self.c / det;
        let id = self.a / det;
        Some(Affine {
            a: ia,
            b: ib,
            c: ic,
            d: id,
            e: -(ia * self.e + ic * self.f),
            f: -(ib * self.e + id * self.f),
        })
    }

    fn approx_eq(&self, o: &Affine) -> bool {
        (self.a - o.a).abs() < 1e-6
            && (self.b - o.b).abs() < 1e-6
            && (self.c - o.c).abs() < 1e-6
            && (self.d - o.d).abs() < 1e-6
            && (self.e - o.e).abs() < 0.01
            && (self.f - o.f).abs() < 0.01
    }

    /// Bounding box образа прямоугольника.
    pub fn bounds(&self, r: Rectangle<f64, Logical>) -> Rectangle<i32, Physical> {
        let corners = [
            self.apply(r.loc.x, r.loc.y),
            self.apply(r.loc.x + r.size.w, r.loc.y),
            self.apply(r.loc.x, r.loc.y + r.size.h),
            self.apply(r.loc.x + r.size.w, r.loc.y + r.size.h),
        ];
        let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        let x0 = min_x.floor() as i32;
        let y0 = min_y.floor() as i32;
        Rectangle::new(
            (x0, y0).into(),
            (max_x.ceil() as i32 - x0, max_y.ceil() as i32 - y0).into(),
        )
    }

    fn to_matrix(self) -> Matrix3<f32> {
        // cgmath column-major: столбцы (a,b,0), (c,d,0), (e,f,1).
        Matrix3::new(
            self.a as f32,
            self.b as f32,
            0.0,
            self.c as f32,
            self.d as f32,
            0.0,
            self.e as f32,
            self.f as f32,
            1.0,
        )
    }
}

/// Что и как рисовать для одного куска окна (готовит [`AppState`]).
#[derive(Debug, Clone)]
pub struct PieceDraw {
    pub window: Window,
    /// Id куска в физике (0 — X11 override-redirect окно без тела).
    pub key: u32,
    /// Обрезка в координатах окна (начало = верхний левый угол геометрии).
    pub clip: Option<Rectangle<f64, Logical>>,
    /// Координаты окна → экранные физические пиксели.
    pub xf: Affine,
    /// Рисовать xdg-попапы окна с этой трансформой (основной кусок).
    pub popups: bool,
    /// Деформированный кусок (мягкое тело) — рисуется сеткой по снимку окна.
    pub mesh: Option<MeshDraw>,
}

/// Деформированная сетка куска для [`crate::mesh_render::MeshElement`].
#[derive(Debug, Clone)]
pub struct MeshDraw {
    /// Вершины на экране (физические px).
    pub positions: Vec<[f32; 2]>,
    /// uv вершин по геометрии окна (0..1).
    pub geometry_uv: Vec<[f32; 2]>,
    pub indices: Vec<u16>,
    /// Отпечаток вершин: меняется — меняется commit элемента.
    pub fingerprint: u64,
}

/// Повёрнутая/обрезанная поверхность окна в физическом режиме.
pub struct PhysicsElement {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    id: Id,
    commit: CommitCounter,
    xf: Affine,
    /// Прямоугольник поверхности в координатах окна.
    local: Rectangle<f64, Logical>,
    /// Видимая часть поверхности (локально относительно `local.loc`).
    visible: Rectangle<f64, Logical>,
    geometry: Rectangle<i32, Physical>,
}

impl Element for PhysicsElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Смену геометрии (старый+новый bbox) damage-tracker учитывает сам;
        // здесь — только «содержимое/трансформа изменились».
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)])
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Повёрнутые окна не выровнены по осям — непрозрачных регионов нет.
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for PhysicsElement {
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Никогда не отдаём сырой буфер на scanout-плоскость: иначе DRM
        // показал бы неповёрнутую копию окна рядом с GLES-отрисовкой.
        None
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        use smithay::backend::renderer::Texture;

        let tex = match self.inner.texture() {
            WaylandSurfaceTexture::Texture(t) => t,
            // Однотонные поверхности (single-pixel buffer) не поворачиваем.
            WaylandSurfaceTexture::SolidColor(_) => return Ok(()),
        };
        let tex_size = tex.size();
        if src.size.is_empty() || tex_size.is_empty() || self.local.size.is_empty() {
            return Ok(());
        }

        // Квад в локальных px поверхности → экран.
        let matrix = self
            .xf
            .then_after(&Affine::translation(self.local.loc.x, self.local.loc.y))
            .to_matrix();
        // Локальные px → нормированные координаты текстуры (с учётом
        // wl_surface.set_buffer_transform и y-inverted dmabuf).
        let tex_matrix = texture_matrix(
            src,
            (self.local.size.w, self.local.size.h),
            (tex_size.w, tex_size.h),
            self.inner.transform(),
            tex.is_y_inverted(),
        );
        let v = self.visible;
        let instances = [
            v.loc.x as f32,
            v.loc.y as f32,
            v.size.w as f32,
            v.size.h as f32,
        ];
        let alpha = self.inner.alpha();
        crate::overlay::scissored(frame, dst, damage, |frame| {
            frame.render_texture(tex, tex_matrix, matrix, Some(instances), alpha, None, &[])
        })
    }
}

/// Матрица «локальные px квада (0..dest) → UV текстуры», как
/// `build_texture_mat` в Smithay: масштаб в src, компенсация buffer
/// transform, сдвиг на кроп src, нормировка. Для y-inverted текстуры
/// `v → 1 − v` (у Smithay `−v`, что работает только с REPEAT-обёрткой).
fn texture_matrix(
    src: Rectangle<f64, Buffer>,
    dest: (f64, f64),
    texture: (i32, i32),
    transform: Transform,
    y_inverted: bool,
) -> Matrix3<f32> {
    use cgmath::{SquareMatrix, Vector2};
    let dst_src = transform.transform_size(src.size);
    let mut m = Matrix3::<f32>::identity();
    m = Matrix3::from_nonuniform_scale(
        (dst_src.w / dest.0.max(1e-6)) as f32,
        (dst_src.h / dest.1.max(1e-6)) as f32,
    ) * m;
    let (w, h) = (dst_src.w as f32, dst_src.h as f32);
    let translation = match transform {
        Transform::Normal | Transform::Flipped90 => Vector2::new(0.0, 0.0),
        Transform::_90 => Vector2::new(0.0, w),
        Transform::_180 => Vector2::new(w, h),
        Transform::_270 => Vector2::new(h, 0.0),
        Transform::Flipped => Vector2::new(w, 0.0),
        Transform::Flipped180 => Vector2::new(0.0, h),
        Transform::Flipped270 => Vector2::new(h, w),
    };
    m = transform.matrix() * m;
    m = Matrix3::from_translation(translation) * m;
    m = Matrix3::from_translation(Vector2::new(src.loc.x as f32, src.loc.y as f32)) * m;
    m = Matrix3::from_nonuniform_scale(
        1.0 / texture.0.max(1) as f32,
        1.0 / texture.1.max(1) as f32,
    ) * m;
    if y_inverted {
        m = Matrix3::new(1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 1.0, 1.0) * m;
    }
    m
}

/// Кэш id/commit физических элементов между кадрами.
pub struct RenderCache {
    elements: HashMap<(Id, u32), CachedElement>,
    meshes: HashMap<u32, CachedMesh>,
    frame: u64,
    /// Слои оверлея: (id, отпечаток, commit).
    overlays: [(Id, u64, CommitCounter); 3],
    /// Обводки окон/кусков: (id, отпечаток, commit, кадр последнего показа).
    borders: HashMap<BorderKey, (Id, u64, CommitCounter, u64)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum BorderKey {
    Piece(u32),
    Window(Window),
}

struct CachedMesh {
    id: Id,
    fingerprint: u64,
    snapshot: CommitCounter,
    commit: CommitCounter,
    seen: u64,
}

struct CachedElement {
    id: Id,
    inner_commit: CommitCounter,
    xf: Affine,
    visible: Rectangle<f64, Logical>,
    commit: CommitCounter,
    seen: u64,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self {
            elements: HashMap::new(),
            meshes: HashMap::new(),
            frame: 0,
            overlays: std::array::from_fn(|_| (Id::new(), 0, CommitCounter::default())),
            borders: HashMap::new(),
        }
    }
}

impl RenderCache {
    /// Стабильный id и commit элемента; commit растёт при смене буфера или
    /// трансформы.
    fn track(
        &mut self,
        inner: &WaylandSurfaceRenderElement<GlesRenderer>,
        key: u32,
        xf: Affine,
        visible: Rectangle<f64, Logical>,
    ) -> (Id, CommitCounter) {
        let frame = self.frame;
        let inner_commit = inner.current_commit();
        let entry = self
            .elements
            .entry((inner.id().clone(), key))
            .or_insert_with(|| CachedElement {
                id: Id::new(),
                inner_commit,
                xf,
                visible,
                commit: CommitCounter::default(),
                seen: frame,
            });
        if entry.inner_commit != inner_commit
            || !entry.xf.approx_eq(&xf)
            || entry.visible != visible
        {
            entry.inner_commit = inner_commit;
            entry.xf = xf;
            entry.visible = visible;
            entry.commit.increment();
        }
        entry.seen = frame;
        (entry.id.clone(), entry.commit)
    }

    fn track_mesh(
        &mut self,
        key: u32,
        fingerprint: u64,
        snapshot: CommitCounter,
    ) -> (Id, CommitCounter) {
        let frame = self.frame;
        let entry = self.meshes.entry(key).or_insert_with(|| CachedMesh {
            id: Id::new(),
            fingerprint,
            snapshot,
            commit: CommitCounter::default(),
            seen: frame,
        });
        if entry.fingerprint != fingerprint || entry.snapshot != snapshot {
            entry.fingerprint = fingerprint;
            entry.snapshot = snapshot;
            entry.commit.increment();
        }
        entry.seen = frame;
        (entry.id.clone(), entry.commit)
    }

    fn begin_frame(&mut self) {
        self.frame += 1;
    }

    fn end_frame(&mut self) {
        let frame = self.frame;
        self.elements.retain(|_, e| e.seen + 120 > frame);
        self.meshes.retain(|_, e| e.seen + 120 > frame);
        self.borders.retain(|_, e| e.3 + 120 > frame);
    }

    /// Render-элемент обводки со стабильным id; commit растёт при изменении.
    fn border(
        &mut self,
        key: BorderKey,
        geometry: OverlayGeometry,
    ) -> Option<CustomRenderElements> {
        if geometry.is_empty() {
            return None;
        }
        let frame = self.frame;
        let print = geometry.fingerprint();
        let entry = self
            .borders
            .entry(key)
            .or_insert_with(|| (Id::new(), print, CommitCounter::default(), frame));
        if entry.1 != print {
            entry.1 = print;
            entry.2.increment();
        }
        entry.3 = frame;
        crate::overlay::OverlayElement::new(entry.0.clone(), entry.2, geometry)
            .map(CustomRenderElements::Overlay)
    }

    fn overlay_commit(&mut self, layer: usize, print: u64) -> (Id, CommitCounter) {
        let (id, last, commit) = &mut self.overlays[layer];
        if print != *last {
            *last = print;
            commit.increment();
        }
        (id.clone(), *commit)
    }
}

fn push_surface_elements(
    out: &mut Vec<CustomRenderElements>,
    renderer: &mut GlesRenderer,
    cache: &mut RenderCache,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    origin: Point<i32, Logical>,
    key: u32,
    xf: Affine,
    clip: Option<Rectangle<f64, Logical>>,
) {
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
            renderer,
            surface,
            (origin.x, origin.y),
            1.0,
            1.0,
            Kind::Unspecified,
        );
    // render_elements_from_surface_tree уже отдаёт front-to-back.
    for inner in elements {
        let g = inner.geometry(Scale::from(1.0));
        let local: Rectangle<f64, Logical> = Rectangle::new(
            (g.loc.x as f64, g.loc.y as f64).into(),
            (g.size.w as f64, g.size.h as f64).into(),
        );
        let visible_abs = match clip {
            Some(c) => match c.intersection(local) {
                Some(v) => v,
                None => continue,
            },
            None => local,
        };
        if visible_abs.size.w <= 0.0 || visible_abs.size.h <= 0.0 {
            continue;
        }
        let visible = Rectangle::new(
            (
                visible_abs.loc.x - local.loc.x,
                visible_abs.loc.y - local.loc.y,
            )
                .into(),
            visible_abs.size,
        );
        let geometry = xf.bounds(visible_abs);
        let (id, commit) = cache.track(&inner, key, xf, visible);
        out.push(CustomRenderElements::Physics(PhysicsElement {
            inner,
            id,
            commit,
            xf,
            local,
            visible,
            geometry,
        }));
    }
}

/// Параметры обводки: толщина и цвет (активное окно — свой цвет).
fn border_style(state: &AppState, window: &Window, focused: Option<&Window>) -> (f32, [f32; 4]) {
    let cfg = &state.config.rendering;
    let color = if focused == Some(window) {
        &cfg.border_active_color
    } else {
        &cfg.border_color
    };
    (cfg.border_width.max(0.0), crate::config::parse_color(color))
}

/// Обводка куска: прямоугольник снаружи кадра (повёрнутый вместе с окном)
/// или контур деформированной сетки мягкого окна.
fn piece_border(draw: &PieceDraw, width: f32, color: [f32; 4]) -> OverlayGeometry {
    let mut g = OverlayGeometry::new();
    let Some(rect) = draw.clip else {
        return g;
    };
    if width <= 0.0 {
        return g;
    }
    // Масштаб камеры: толщина рамки растёт и уменьшается вместе с окном.
    let scale = (draw.xf.a * draw.xf.a + draw.xf.b * draw.xf.b).sqrt() as f32;
    let w = (width * scale).max(1.0);
    let outline: Vec<[f32; 2]> = match &draw.mesh {
        Some(mesh) => {
            // Граница сетки (res+1)×(res+1), вершины построчно.
            let n = mesh.positions.len();
            let side = (n as f64).sqrt().round() as usize;
            if side < 2 || side * side != n {
                return g;
            }
            let idx = |x: usize, y: usize| y * side + x;
            let mut ring = Vec::with_capacity(4 * side);
            ring.extend((0..side).map(|x| idx(x, 0)));
            ring.extend((1..side).map(|y| idx(side - 1, y)));
            ring.extend((0..side - 1).rev().map(|x| idx(x, side - 1)));
            ring.extend((1..side - 1).rev().map(|y| idx(0, y)));
            ring.into_iter().map(|i| mesh.positions[i]).collect()
        }
        None => {
            // Рамка снаружи окна: центр линии на полтолщины за краем.
            let e = (width * 0.5) as f64;
            let (x0, y0) = (rect.loc.x - e, rect.loc.y - e);
            let (x1, y1) = (rect.loc.x + rect.size.w + e, rect.loc.y + rect.size.h + e);
            [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
                .into_iter()
                .map(|(x, y)| {
                    let (sx, sy) = draw.xf.apply(x, y);
                    [sx as f32, sy as f32]
                })
                .collect()
        }
    };
    g.outline(&outline, w, color);
    g
}

/// Элементы окон физического режима, front-to-back.
pub fn collect_physics_elements(
    renderer: &mut GlesRenderer,
    state: &mut AppState,
) -> Vec<CustomRenderElements> {
    let draws = state.piece_draws();
    let mut cache = std::mem::take(&mut state.render_cache);
    let mut snapshots = std::mem::take(&mut state.snapshots);
    {
        let alive: Vec<Window> = state.space.elements().cloned().collect();
        snapshots.retain(|w| alive.contains(w));
    }
    cache.begin_frame();
    let focused = state.get_focused_window();
    let mut out = Vec::new();
    for draw in draws {
        let Some(surface) = draw.window.wl_surface().map(|s| s.into_owned()) else {
            continue;
        };
        // Обводка — сразу за своим куском: позади окна, но поверх окон ниже.
        let (border_width, border_color) = border_style(state, &draw.window, focused.as_ref());
        let border = cache.border(
            BorderKey::Piece(draw.key),
            piece_border(&draw, border_width, border_color),
        );
        let geo_loc = draw.window.geometry().loc;
        let mesh_element = draw.mesh.as_ref().and_then(|mesh| {
            let snap = snapshots.snapshot(renderer, &draw.window)?;
            let texcoords = mesh
                .geometry_uv
                .iter()
                .map(|uv| snap.texcoord(*uv))
                .collect();
            let (id, commit) = cache.track_mesh(draw.key, mesh.fingerprint, snap.commit);
            Some(crate::mesh_render::MeshElement::new(
                id,
                commit,
                snap.texture.clone(),
                mesh.positions.clone(),
                texcoords,
                mesh.indices.clone(),
                1.0,
                Vec::new(),
            ))
        });
        if draw.popups {
            // xdg-попапы: поверх окна, с его трансформой и без обрезки.
            for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                let offset = popup_offset - popup.geometry().loc;
                push_surface_elements(
                    &mut out,
                    renderer,
                    &mut cache,
                    popup.wl_surface(),
                    offset,
                    draw.key,
                    draw.xf,
                    None,
                );
            }
        }
        if let Some(mesh) = mesh_element {
            out.push(CustomRenderElements::Mesh(mesh));
            out.extend(border);
            continue;
        }
        // Начало дерева поверхностей — так, чтобы геометрия окна была в (0,0).
        push_surface_elements(
            &mut out,
            renderer,
            &mut cache,
            &surface,
            Point::from((-geo_loc.x, -geo_loc.y)),
            draw.key,
            draw.xf,
            draw.clip,
        );
        out.extend(border);
    }
    cache.end_frame();
    state.render_cache = cache;
    state.snapshots = snapshots;
    out
}

/// Обводки окон в тайлинге: окна не пересекаются, рамки живут в зазорах
/// между ними — рисуем их позади всех окон.
fn tiling_borders(state: &mut AppState) -> Vec<CustomRenderElements> {
    let focused = state.get_focused_window();
    let windows: Vec<(Window, Rectangle<i32, Logical>)> = state
        .space
        .elements()
        .filter(|w| !w.x11_surface().is_some_and(|x| x.is_override_redirect()))
        .filter_map(|w| Some((w.clone(), state.space.element_geometry(w)?)))
        .collect();
    let mut cache = std::mem::take(&mut state.render_cache);
    cache.begin_frame();
    let mut out = Vec::new();
    for (window, geo) in windows {
        let (width, color) = border_style(state, &window, focused.as_ref());
        if width <= 0.0 {
            continue;
        }
        // Окно больше тайла обрезано по нему — рамка тоже.
        let geo = state
            .tile_rect(&window)
            .and_then(|tile| tile.intersection(geo))
            .unwrap_or(geo);
        let e = width * 0.5;
        let (x0, y0) = (geo.loc.x as f32 - e, geo.loc.y as f32 - e);
        let (x1, y1) = (
            (geo.loc.x + geo.size.w) as f32 + e,
            (geo.loc.y + geo.size.h) as f32 + e,
        );
        let mut g = OverlayGeometry::new();
        g.outline(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1]], width, color);
        out.extend(cache.border(BorderKey::Window(window), g));
    }
    cache.end_frame();
    state.render_cache = cache;
    out
}

/// Оверлей физического режима: интерфейс инструментов, мир, пол.
fn overlay_elements(state: &mut AppState) -> Vec<CustomRenderElements> {
    let layers = state.overlay_geometry();
    let mut out = Vec::new();
    for (layer, geometry) in layers.into_iter().enumerate() {
        if geometry.is_empty() {
            continue;
        }
        let (id, commit) = state
            .render_cache
            .overlay_commit(layer, geometry.fingerprint());
        if let Some(e) = crate::overlay::OverlayElement::new(id, commit, geometry) {
            out.push(CustomRenderElements::Overlay(e));
        }
    }
    out
}

/// Окно в экранных координатах: начало его геометрии — в `screen_loc`.
/// Попапы — поверх и без обрезки, само окно — обрезанное по `crop`.
fn window_elements(
    renderer: &mut GlesRenderer,
    window: &Window,
    screen_loc: Point<i32, Logical>,
    crop: Option<Rectangle<i32, Logical>>,
) -> Vec<CustomRenderElements> {
    let mut out = Vec::new();
    let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
        return out;
    };
    for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
        let loc = screen_loc + popup_offset - popup.geometry().loc;
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                loc.to_physical(1),
                1.0,
                1.0,
                Kind::Unspecified,
            );
        out.extend(elements.into_iter().map(CustomRenderElements::Surface));
    }
    let loc = screen_loc - window.geometry().loc;
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(
            renderer,
            &surface,
            loc.to_physical(1),
            1.0,
            1.0,
            Kind::Unspecified,
        );
    match crop {
        Some(crop) => {
            let crop = Rectangle::new(crop.loc.to_physical(1), crop.size.to_physical(1));
            out.extend(
                elements
                    .into_iter()
                    .filter_map(|e| CropRenderElement::from_element(e, 1.0, crop))
                    .map(CustomRenderElements::Cropped),
            );
        }
        None => out.extend(elements.into_iter().map(CustomRenderElements::Surface)),
    }
    out
}

fn is_override_redirect(window: &Window) -> bool {
    window
        .x11_surface()
        .is_some_and(|x| x.is_override_redirect())
}

/// Окна тайлинга front-to-back, каждое обрезано по своему тайлу.
fn tiling_window_elements(
    renderer: &mut GlesRenderer,
    state: &AppState,
) -> Vec<CustomRenderElements> {
    let mut out = Vec::new();
    for window in state.space.elements().rev() {
        let Some(geo) = state.space.element_geometry(window) else {
            continue;
        };
        let crop = if is_override_redirect(window) {
            None
        } else {
            state.tile_rect(window)
        };
        out.extend(window_elements(renderer, window, geo.loc, crop));
    }
    out
}

/// X11-меню/тултипы (override-redirect): их координаты всегда экранные.
fn override_redirect_elements(
    renderer: &mut GlesRenderer,
    state: &AppState,
) -> Vec<CustomRenderElements> {
    let mut out = Vec::new();
    for window in state
        .space
        .elements()
        .rev()
        .filter(|w| is_override_redirect(w))
    {
        if let Some(geo) = state.space.element_geometry(window) {
            out.extend(window_elements(renderer, window, geo.loc, None));
        }
    }
    out
}

/// Все элементы кадра (без курсора), front-to-back. Общая точка для winit
/// и DRM, чтобы оба бэкенда рисовали одно и то же.
pub fn frame_elements(
    renderer: &mut GlesRenderer,
    state: &mut AppState,
    output: &Output,
) -> Vec<CustomRenderElements> {
    use crate::layer_shell::layer_elements;
    use smithay::wayland::shell::wlr_layer::Layer;
    // Полноэкранное окно: только оно, его X11-меню и слой overlay.
    if let Some(window) = state.fullscreen_window() {
        let mut elements = layer_elements(renderer, output, &[Layer::Overlay]);
        elements.extend(override_redirect_elements(renderer, state));
        elements.extend(window_elements(renderer, &window, (0, 0).into(), None));
        return elements;
    }
    let mut elements = layer_elements(renderer, output, &[Layer::Overlay, Layer::Top]);
    if state.layout_mode == crate::state::LayoutMode::Tiling {
        elements.extend(tiling_window_elements(renderer, state));
        elements.extend(tiling_borders(state));
    } else {
        elements.extend(overlay_elements(state));
        elements.extend(collect_physics_elements(renderer, state));
    }
    elements.extend(layer_elements(
        renderer,
        output,
        &[Layer::Bottom, Layer::Background],
    ));
    elements
}

/// Кадровые колбэки после отрисовки: окнам активного стола и слоям.
/// Окна скрытых столов колбэков не получают (как свёрнутые).
pub fn send_frames<T: Into<std::time::Duration> + Copy>(state: &AppState, output: &Output, now: T) {
    for window in state.space.elements() {
        window.send_frame(output, now, Some(std::time::Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    state.send_layer_frames(output, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uv(m: &Matrix3<f32>, x: f32, y: f32) -> (f32, f32) {
        let v = m * cgmath::Vector3::new(x, y, 1.0);
        (v.x, v.y)
    }

    #[test]
    fn texture_matrix_crops_flips_and_rotates() {
        let src = Rectangle::new((0.0, 25.0).into(), (100.0, 50.0).into());
        // Обычный буфер 100×100, кроп по Y 25..75, квад 200×100.
        let m = texture_matrix(src, (200.0, 100.0), (100, 100), Transform::Normal, false);
        assert_eq!(uv(&m, 0.0, 0.0), (0.0, 0.25));
        assert_eq!(uv(&m, 200.0, 100.0), (1.0, 0.75));
        // y-inverted: верх кропа — строка 75 снизу → v = 1 − 0.25.
        let m = texture_matrix(src, (200.0, 100.0), (100, 100), Transform::Normal, true);
        assert_eq!(uv(&m, 0.0, 0.0), (0.0, 0.75));
        assert_eq!(uv(&m, 0.0, 100.0), (0.0, 0.25));
        // Буфер 100×50 с transform 90: на экране 50×100.
        let full = Rectangle::from_size((100.0, 50.0).into());
        let m = texture_matrix(full, (50.0, 100.0), (100, 50), Transform::_90, false);
        let corners = [
            uv(&m, 0.0, 0.0),
            uv(&m, 50.0, 0.0),
            uv(&m, 0.0, 100.0),
            uv(&m, 50.0, 100.0),
        ];
        for (u, v) in corners {
            assert!((u - 0.0).abs() < 1e-5 || (u - 1.0).abs() < 1e-5, "u={u}");
            assert!((v - 0.0).abs() < 1e-5 || (v - 1.0).abs() < 1e-5, "v={v}");
        }
        // Углы квада попадают в четыре разных угла текстуры.
        let mut uniq: Vec<(i32, i32)> = corners
            .iter()
            .map(|(u, v)| (*u as i32, *v as i32))
            .collect();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 4);
    }

    #[test]
    fn affine_piece_maps_center_and_inverts() {
        let xf = Affine::piece((500.0, 300.0), 0.7, 0.5, (100.0, 50.0));
        let (x, y) = xf.apply(100.0, 50.0);
        assert!((x - 500.0).abs() < 1e-9 && (y - 300.0).abs() < 1e-9);
        let inv = xf.inverse().unwrap();
        let (px, py) = xf.apply(13.0, -7.0);
        let (bx, by) = inv.apply(px, py);
        assert!((bx - 13.0).abs() < 1e-9 && (by + 7.0).abs() < 1e-9);
        let t = Affine::translation(5.0, 6.0);
        let comp = xf.then_after(&t);
        let (cx, cy) = comp.apply(1.0, 2.0);
        let (ex, ey) = xf.apply(6.0, 8.0);
        assert!((cx - ex).abs() < 1e-9 && (cy - ey).abs() < 1e-9);
    }
}
