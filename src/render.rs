//! Render-элементы и сборка кадра.
//!
//! В тайлинге кадр строит обычный [`Space`]. В физическом режиме каждое окно
//! состоит из одного или нескольких кусков (разрезание ножом) и рисуется
//! [`PhysicsElement`]-ами: текстура поверхности, повёрнутая/масштабированная
//! аффинным преобразованием «координаты окна → экран» и обрезанная по
//! прямоугольнику куска. Поверх — оверлей (линии, верёвки, связи, пол).
//!
//! Damage: у каждого элемента свой commit, который растёт только когда
//! меняется буфер поверхности или трансформа куска. Раньше все физические
//! элементы делили общий счётчик и отдавали полный damage каждый кадр — любое
//! движение мыши в DRM-режиме перекомпоновывало все окна целиком.

use std::collections::HashMap;

use cgmath::Matrix3;
use smithay::backend::renderer::{
    element::{
        Element, Id, Kind, RenderElement, UnderlyingStorage, render_elements,
        solid::SolidColorRenderElement,
        surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
    },
    gles::{GlesError, GlesFrame, GlesRenderer},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::desktop::{PopupManager, Window, space::SpaceRenderElements};
use smithay::output::Output;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::seat::WaylandFocus;

use crate::state::AppState;

render_elements! {
    pub CustomRenderElements<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor=SolidColorRenderElement,
    CursorSurface=WaylandSurfaceRenderElement<GlesRenderer>,
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
        // Локальные px → нормированные координаты текстуры.
        let sx = src.size.w / self.local.size.w;
        let sy = src.size.h / self.local.size.h;
        let (ty, sy) = if tex.is_y_inverted() {
            (src.loc.y + src.size.h, -sy)
        } else {
            (src.loc.y, sy)
        };
        let tex_matrix = Matrix3::new(
            (sx / tex_size.w as f64) as f32,
            0.0,
            0.0,
            0.0,
            (sy / tex_size.h as f64) as f32,
            0.0,
            (src.loc.x / tex_size.w as f64) as f32,
            (ty / tex_size.h as f64) as f32,
            1.0,
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

/// Кэш id/commit физических элементов между кадрами.
pub struct RenderCache {
    elements: HashMap<(Id, u32), CachedElement>,
    meshes: HashMap<u32, CachedMesh>,
    frame: u64,
    overlay_id: Id,
    overlay_print: u64,
    overlay_commit: CommitCounter,
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
            overlay_id: Id::new(),
            overlay_print: 0,
            overlay_commit: CommitCounter::default(),
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
    }

    fn overlay_commit(&mut self, print: u64) -> (Id, CommitCounter) {
        if print != self.overlay_print {
            self.overlay_print = print;
            self.overlay_commit.increment();
        }
        (self.overlay_id.clone(), self.overlay_commit)
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
    let mut out = Vec::new();
    for draw in draws {
        let Some(surface) = draw.window.wl_surface().map(|s| s.into_owned()) else {
            continue;
        };
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
    }
    cache.end_frame();
    state.render_cache = cache;
    state.snapshots = snapshots;
    out
}

/// Оверлей физического режима (линии, верёвки, связи, превью, пол).
fn overlay_element(state: &mut AppState) -> Option<CustomRenderElements> {
    let geometry = state.overlay_geometry();
    if geometry.is_empty() {
        return None;
    }
    let (id, commit) = state.render_cache.overlay_commit(geometry.fingerprint());
    crate::overlay::OverlayElement::new(id, commit, geometry).map(CustomRenderElements::Overlay)
}

/// Все элементы кадра (без курсора), front-to-back. Общая точка для winit
/// и DRM, чтобы оба бэкенда рисовали одно и то же.
pub fn frame_elements(
    renderer: &mut GlesRenderer,
    state: &mut AppState,
    output: &Output,
) -> Vec<CustomRenderElements> {
    if state.layout_mode == crate::state::LayoutMode::Tiling {
        return state
            .space
            .render_elements_for_output(renderer, output, 1.0)
            .unwrap_or_default()
            .into_iter()
            .map(CustomRenderElements::Space)
            .collect();
    }
    let mut elements = Vec::new();
    if let Some(overlay) = overlay_element(state) {
        elements.push(overlay);
    }
    elements.extend(collect_physics_elements(renderer, state));
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

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
