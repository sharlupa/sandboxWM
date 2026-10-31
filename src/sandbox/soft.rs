//! Мягкие тела (концепт §5): сетка окна, захват края, снимки.

use std::collections::HashSet;
use std::time::Instant;

use rapier2d::math::Vector;
use smithay::desktop::Window;

use crate::config::BodyKind;
use crate::render::Affine;
use crate::softbody::SoftParams;
use crate::state::{AppState, LayoutMode};

use super::{Piece, SoftPiece, window_app_id};

impl AppState {
    /// Параметры тела `kind` для окна: конфиг + override по app_id. Желе
    /// не пластично — всегда возвращает форму.
    pub(super) fn soft_params(&self, window: &Window, kind: BodyKind) -> SoftParams {
        let cfg = &self.config.soft_body;
        if kind == BodyKind::Jelly {
            return SoftParams {
                softness: cfg.jelly_softness.clamp(0.0, 1.0),
                impact_squash: cfg.jelly_impact_squash.clamp(0.0, 1.0),
                plasticity: 0.0,
            };
        }
        let softness = self
            .window_override(window)
            .softness
            .filter(|s| *s > 0.0)
            .unwrap_or(cfg.softness);
        SoftParams {
            softness: softness.clamp(0.0, 1.0),
            impact_squash: cfg.impact_squash.clamp(0.0, 1.0),
            plasticity: cfg.plasticity.clamp(0.0, 1.0),
        }
    }

    /// Делает кусок мягким или желейным (`kind`, параметры `params`); у уже
    /// мягкого меняет материал, сохраняя форму. Сетка мягкого тела
    /// прямоугольная, поэтому косо отрезанные куски остаются жёсткими.
    pub(super) fn make_soft(&mut self, piece: u32, kind: BodyKind, params: SoftParams) {
        if let Some(soft) = self.soft.get_mut(&piece) {
            soft.kind = kind;
            soft.params = params;
            return;
        }
        let Some(rect) = self
            .piece_by_id(piece)
            .filter(|p| p.shape.is_none())
            .map(|p| p.rect)
        else {
            return;
        };
        let mesh = crate::softbody::SoftMesh::new(
            rect.size.w as f32,
            rect.size.h as f32,
            self.config.soft_body.mesh_resolution.clamp(1, 32),
        );
        let rest_generation = mesh.rest_generation();
        self.soft.insert(
            piece,
            SoftPiece {
                mesh,
                params,
                kind,
                prev_vel: None,
                load: Default::default(),
                rest_generation,
            },
        );
    }

    /// Тело окна: материал его мягких кусков, иначе жёсткое.
    pub(super) fn window_body(&self, window: &Window) -> BodyKind {
        self.pieces_of(window)
            .find_map(|p| self.soft.get(&p.id))
            .map_or(BodyKind::Rigid, |s| s.kind)
    }

    /// Тело всех кусков окна.
    pub(super) fn set_window_body(&mut self, window: &Window, kind: BodyKind) {
        let ids: Vec<u32> = self.pieces_of(window).map(|p| p.id).collect();
        let params = self.soft_params(window, kind);
        for id in ids {
            match kind {
                BodyKind::Rigid => self.drop_soft(id),
                _ => self.make_soft(id, kind, params),
            }
        }
    }

    pub(super) fn drop_soft(&mut self, piece: u32) {
        if self.soft.remove(&piece).is_some() {
            if self.soft_grab.as_ref().map_or(false, |(p, _)| *p == piece) {
                self.soft_grab = None;
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_hull(piece, None);
            }
        }
    }

    /// Super+J: мягкое ↔ жёсткое окно.
    /// Super+J: тело активного окна по кругу — жёсткое → мягкое → желе.
    pub fn toggle_soft_focused(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let next = match self.window_body(&window) {
            BodyKind::Rigid => BodyKind::Soft,
            BodyKind::Soft => BodyKind::Jelly,
            BodyKind::Jelly => BodyKind::Rigid,
        };
        self.set_window_body(&window, next);
        let name = match next {
            BodyKind::Rigid => "жёсткое",
            BodyKind::Soft => "мягкое",
            BodyKind::Jelly => "желе",
        };
        println!("[soft] окно {:?}: {name}", window_app_id(&window));
        self.needs_render = true;
    }

    /// Super+Shift+ЛКМ: схватить мягкое окно за точку и тянуть (деформация).
    pub fn soft_grab_begin(&mut self, sx: f64, sy: f64) -> Option<Window> {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return None;
        }
        let (piece_id, _) = self.piece_at(sx, sy)?;
        // Жёсткое окно не мнётся (Super+Shift+ЛКМ его просто тащит).
        if !self.soft.contains_key(&piece_id) {
            return None;
        }
        let piece = self.piece_by_id(piece_id)?.clone();
        let local = self.rigid_local(&piece, sx, sy)?;
        let radius = (piece.rect.size.w.max(piece.rect.size.h)) as f32;
        let soft = self.soft.get_mut(&piece_id)?;
        let grab = soft.mesh.grab(local, radius)?;
        self.soft_grab = Some((piece_id, grab));
        self.last_physics_tick.get_or_insert_with(Instant::now);
        self.needs_render = true;
        Some(piece.window)
    }

    pub fn soft_grab_update(&mut self, sx: f64, sy: f64) {
        let Some((piece_id, _)) = self.soft_grab.as_ref() else {
            return;
        };
        let piece_id = *piece_id;
        let Some(piece) = self.piece_by_id(piece_id).cloned() else {
            self.soft_grab = None;
            return;
        };
        let Some(target) = self.rigid_local(&piece, sx, sy) else {
            return;
        };
        let target = self.push_grab_out(piece_id, target);
        if let (Some((_, grab)), Some(soft)) =
            (self.soft_grab.as_ref(), self.soft.get_mut(&piece_id))
        {
            let params = soft.params;
            soft.mesh.drag(grab, target, &params);
        }
        self.needs_render = true;
    }

    pub fn soft_grab_end(&mut self) {
        if let Some((piece, grab)) = self.soft_grab.take() {
            if let Some(soft) = self.soft.get_mut(&piece) {
                soft.mesh.release(grab);
            }
            self.needs_render = true;
        }
    }

    pub fn soft_grab_active(&self) -> bool {
        self.soft_grab.is_some()
    }

    /// Экран → локальные координаты куска (от центра) без учёта сетки.
    /// Точка захвата (локальные координаты куска), вытолкнутая из пола и
    /// соседних окон: тянущийся край упирается в них, а не проходит насквозь.
    fn push_grab_out(&self, piece: u32, local: [f32; 2]) -> [f32; 2] {
        let Some(phys) = self.physics.as_ref() else {
            return local;
        };
        let Some((c, angle)) = phys.piece_transform(piece) else {
            return local;
        };
        let world = c + crate::physics::rotate(Vector::new(local[0], local[1]), angle);
        let pushed = phys.push_out_of_obstacles(piece, world);
        let back = crate::physics::rotate(pushed - c, -angle);
        [back.x, back.y]
    }

    fn rigid_local(&self, piece: &Piece, sx: f64, sy: f64) -> Option<[f32; 2]> {
        let inv = self.piece_xf(piece)?.inverse()?;
        let (x, y) = inv.apply(sx, sy);
        let (cx, cy) = piece.center();
        Some([(x - cx) as f32, (y - cy) as f32])
    }

    /// Шаг мягких тел за кадр: инерционные псевдосилы из ускорения тела.
    /// Возвращает true, пока сетки ещё колеблются.
    pub(super) fn step_soft(&mut self, dt: f32) -> bool {
        if self.soft.is_empty() || dt <= 0.0 {
            return false;
        }
        let Some(phys) = self.physics.as_mut() else {
            return false;
        };
        let mut active = false;
        let mut hulls = Vec::new();
        for (&id, soft) in self.soft.iter_mut() {
            let (Some((vel, angvel)), Some((_, angle))) =
                (phys.piece_velocity(id), phys.piece_transform(id))
            else {
                continue;
            };
            // Собственное ускорение (a − g): в свободном падении сетка не
            // деформируется, при ударе об пол — сплющивается. Деформирует
            // только его изменение: лёжа на полу, окно не оседает навсегда.
            let accel = soft.prev_vel.map_or(Vector::ZERO, |prev| (vel - prev) / dt);
            soft.prev_vel = Some(vel);
            let proper = crate::physics::rotate(accel - phys.piece_gravity(id), -angle);
            let load = soft.load.apply([proper.x, proper.y], dt);
            soft.mesh.step(dt, load, angvel, &soft.params);
            active |= soft.mesh.is_active();
            let generation = soft.mesh.rest_generation();
            if generation != soft.rest_generation {
                soft.rest_generation = generation;
                let hull: Vec<Vector> = soft
                    .mesh
                    .rest_hull()
                    .into_iter()
                    .map(|p| Vector::new(p[0], p[1]))
                    .collect();
                hulls.push((id, hull));
            }
        }
        for (id, hull) in hulls {
            // Вытянутое в треугольник окно и сталкивается как треугольник.
            phys.set_piece_hull(id, Some(&hull));
        }
        active || self.soft_grab.is_some()
    }

    /// Сетка для отрисовки, если кусок деформирован.
    pub(super) fn mesh_draw(&self, piece: &Piece, xf: &Affine) -> Option<crate::render::MeshDraw> {
        let soft = self.soft.get(&piece.id)?;
        if !(soft.mesh.is_deformed() || soft.mesh.is_active()) {
            return None;
        }
        let (cx, cy) = piece.center();
        let size = piece.window.geometry().size;
        let (gw, gh) = (size.w.max(1) as f64, size.h.max(1) as f64);
        let r = piece.rect;
        let mut fingerprint: u64 = 0xcbf2_9ce4_8422_2325;
        let positions: Vec<[f32; 2]> = soft
            .mesh
            .positions()
            .iter()
            .map(|p| {
                let (x, y) = xf.apply(cx + p[0] as f64, cy + p[1] as f64);
                for v in [x as f32, y as f32] {
                    fingerprint ^= v.to_bits() as u64;
                    fingerprint = fingerprint.wrapping_mul(0x0100_0000_01b3);
                }
                [x as f32, y as f32]
            })
            .collect();
        let geometry_uv = soft
            .mesh
            .uvs()
            .iter()
            .map(|uv| {
                [
                    ((r.loc.x + uv[0] as f64 * r.size.w) / gw) as f32,
                    ((r.loc.y + uv[1] as f64 * r.size.h) / gh) as f32,
                ]
            })
            .collect();
        Some(crate::render::MeshDraw {
            positions,
            geometry_uv,
            indices: soft.mesh.indices().to_vec(),
            fingerprint,
        })
    }

    /// Все id кусков (для проверки целостности в тестах/отладке).
    #[allow(dead_code)]
    pub fn piece_ids(&self) -> HashSet<u32> {
        self.pieces.iter().map(|p| p.id).collect()
    }
}
