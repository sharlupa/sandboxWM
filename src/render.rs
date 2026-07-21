//! Shared render-element enum.
//!
//! Combines the normal surface elements produced by [`Space`] with a solid-color
//! element used to draw the software cursor in the DRM/KMS backend (which has no
//! hardware cursor plane wired up in Smithay 0.7).
//!
//! Specialised to `GlesRenderer`, the only renderer used by both backends.

use cgmath::{Matrix3, Vector2, prelude::*};
use smithay::backend::renderer::{
    element::{
        Element, Id, Kind, RenderElement, UnderlyingStorage, render_elements,
        solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement,
    },
    gles::GlesRenderer,
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::desktop::{Window, space::SpaceRenderElements};
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::seat::WaylandFocus;

use crate::state::AppState;

render_elements! {
    pub CustomRenderElements<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor=SolidColorRenderElement,
    Physics=PhysicsElement,
}

pub struct PhysicsElement {
    pub inner: WaylandSurfaceRenderElement<GlesRenderer>,
    pub angle: f64,
    pub zoom: f64,
    pub center: Point<f64, Physical>,
    /// Bounding rects from recent frames (buffer-age history). Unioned into
    /// damage so old silhouettes are cleared on every back-buffer.
    pub damage_history: Vec<Rectangle<i32, Physical>>,
    /// Bumped whenever angle/center change, even if the Wayland buffer did not.
    pub visual_commit: CommitCounter,
}

impl PhysicsElement {
    fn bounding_geometry(
        center: Point<f64, Physical>,
        inner_geo: Rectangle<i32, Physical>,
        zoom: f64,
    ) -> Rectangle<i32, Physical> {
        let corners: [Point<f64, Physical>; 4] = [
            Point::from((inner_geo.loc.x as f64, inner_geo.loc.y as f64)),
            Point::from((
                (inner_geo.loc.x + inner_geo.size.w) as f64,
                inner_geo.loc.y as f64,
            )),
            Point::from((
                inner_geo.loc.x as f64,
                (inner_geo.loc.y + inner_geo.size.h) as f64,
            )),
            Point::from((
                (inner_geo.loc.x + inner_geo.size.w) as f64,
                (inner_geo.loc.y + inner_geo.size.h) as f64,
            )),
        ];

        let max_sq_dist = corners
            .iter()
            .map(|p| {
                let dx = p.x - center.x;
                let dy = p.y - center.y;
                dx * dx + dy * dy
            })
            .fold(0.0f64, f64::max);

        let bounding_radius = (max_sq_dist.sqrt() * zoom).ceil() as i32;
        let bounding_diameter = bounding_radius * 2;
        let loc = Point::from((
            center.x as i32 - bounding_radius,
            center.y as i32 - bounding_radius,
        ));
        Rectangle::new(loc, (bounding_diameter, bounding_diameter).into())
    }
}

impl Element for PhysicsElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.visual_commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Self::bounding_geometry(self.center, self.inner.geometry(scale), self.zoom)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // ВАЖНО: damage_since — в координатах относительно geometry().loc
        // (трекер сам делает `d.loc += element_loc`). Абсолютные rect'ы
        // сдвигались второй раз → при вращении на месте зачищалась чужая
        // область, а старый силуэт оставался, пока окно не сдвинешь.
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
        // Rotated windows are not axis-aligned; never claim opaque regions.
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
        // Never expose the raw Wayland buffer for plane scanout: that would
        // show an unrotated copy of the window next to the GLES-rotated draw,
        // which looks exactly like a brief "ghost trail".
        None
    }

    fn draw(
        &self,
        frame: &mut <GlesRenderer as smithay::backend::renderer::RendererSuper>::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), <GlesRenderer as smithay::backend::renderer::RendererSuper>::Error> {
        use smithay::backend::renderer::Texture;
        use smithay::backend::renderer::element::surface::WaylandSurfaceTexture;

        let tex = match self.inner.texture() {
            WaylandSurfaceTexture::Texture(t) => t,
            WaylandSurfaceTexture::SolidColor(_) => {
                return self.inner.draw(frame, src, _dst, _damage, _opaque_regions);
            }
        };

        let mut mat = Matrix3::<f32>::identity();
        mat = mat
            * Matrix3::from_translation(Vector2::new(self.center.x as f32, self.center.y as f32));
        mat = mat * Matrix3::from_angle_z(cgmath::Rad(self.angle as f32));
        mat = mat * Matrix3::from_scale(self.zoom as f32);
        let orig = self.inner.geometry(Scale::from(1.0));
        let dx = orig.loc.x as f32 - self.center.x as f32;
        let dy = orig.loc.y as f32 - self.center.y as f32;
        mat = mat * Matrix3::from_translation(Vector2::new(dx, dy));

        let tex_size = tex.size();
        let src_size = src.size;
        if src_size.is_empty() || tex_size.is_empty() {
            return Ok(());
        }

        let scale_x = src.size.w / orig.size.w as f64;
        let scale_y = src.size.h / orig.size.h as f64;
        let mut tex_mat = Matrix3::<f32>::identity();
        tex_mat = Matrix3::from_nonuniform_scale(scale_x as f32, scale_y as f32) * tex_mat;
        tex_mat =
            Matrix3::from_translation(Vector2::new(src.loc.x as f32, src.loc.y as f32)) * tex_mat;
        tex_mat = Matrix3::from_nonuniform_scale(1.0 / tex_size.w as f32, 1.0 / tex_size.h as f32)
            * tex_mat;

        let instances = [0.0f32, 0.0f32, orig.size.w as f32, orig.size.h as f32];

        frame.render_texture(
            tex,
            tex_mat,
            mat,
            Some(instances),
            self.inner.alpha(),
            None,
            &[],
        )?;

        Ok(())
    }
}

/// Сколько прошлых bounding-rect'ов держать для damage (DRM DamageBag = 4).
const PHYSICS_DAMAGE_HISTORY: usize = 5;

/// Собирает повёрнутые элементы окон для физического режима и обновляет
/// историю damage для следующих кадров (buffer-age).
pub fn collect_physics_elements(
    renderer: &mut GlesRenderer,
    state: &mut AppState,
) -> Vec<CustomRenderElements> {
    let mut phys_elements = Vec::new();

    let visual_commit = CommitCounter::from(state.physics_visual_gen);
    let cam = state.camera_offset;
    let zoom = state.camera_zoom;

    let snapshots: Vec<(Window, f64, f64, f64, f64, i32, i32)> = {
        let Some(phys) = state.physics.as_ref() else {
            return phys_elements;
        };
        state
            .space
            .elements()
            .filter_map(|win| {
                let win_geom = state.space.element_geometry(win).unwrap_or_default();
                if let Some(&handle) = state.window_bodies.get(win) {
                    let (cx, cy, angle) = phys.body_transform(handle)?;
                    let screen_cx = (cx as f64 - cam.0) * zoom;
                    let screen_cy = (cy as f64 - cam.1) * zoom;
                    Some((
                        win.clone(),
                        screen_cx,
                        screen_cy,
                        angle as f64,
                        zoom,
                        win_geom.size.w,
                        win_geom.size.h,
                    ))
                } else if win
                    .x11_surface()
                    .map_or(false, |x| x.is_override_redirect())
                {
                    // Меню/тултипы без тела: клиент ставит их в ЭКРАННЫХ
                    // координатах (X-геометрия теперь экранная) — рисуем как есть.
                    let screen_cx = win_geom.loc.x as f64 + win_geom.size.w as f64 * 0.5;
                    let screen_cy = win_geom.loc.y as f64 + win_geom.size.h as f64 * 0.5;
                    Some((
                        win.clone(),
                        screen_cx,
                        screen_cy,
                        0.0,
                        1.0,
                        win_geom.size.w,
                        win_geom.size.h,
                    ))
                } else {
                    None
                }
            })
            .collect()
    };

    for (win, screen_cx, screen_cy, angle, zoom, w, h) in snapshots {
        let location = (
            (screen_cx - w as f64 / 2.0).round() as i32,
            (screen_cy - h as f64 / 2.0).round() as i32,
        );
        let damage_history: Vec<Rectangle<i32, Physical>> = state
            .physics_damage_history
            .get(&win)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default();
        let center: Point<f64, Physical> = (screen_cx, screen_cy).into();

        let Some(surface) = win.wl_surface() else {
            continue;
        };
        let render_elements =
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &*surface,
                location,
                1.0,
                1.0,
                Kind::Unspecified,
            );

        let mut frame_geo: Option<Rectangle<i32, Physical>> = None;
        for e in render_elements {
            let elem = PhysicsElement {
                inner: e,
                angle,
                zoom,
                center,
                damage_history: damage_history.clone(),
                visual_commit,
            };
            let geo = elem.geometry(Scale::from(1.0));
            frame_geo = Some(match frame_geo {
                Some(acc) => acc.merge(geo),
                None => geo,
            });
            phys_elements.push(CustomRenderElements::Physics(elem));
        }
        if let Some(geo) = frame_geo {
            let hist = state.physics_damage_history.entry(win).or_default();
            hist.push_back(geo);
            while hist.len() > PHYSICS_DAMAGE_HISTORY {
                hist.pop_front();
            }
        }
    }

    // Front-to-back: последние в space — сверху. elements() уже bottom-to-top,
    // render_frame ждёт front-to-back → разворачиваем.
    phys_elements.reverse();
    phys_elements
}
