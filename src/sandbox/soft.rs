//! Мягкие тела (концепт §5): сетка окна, захват края, снимки.

use std::collections::HashSet;
use std::time::Instant;

use rapier2d::math::Vector;
use smithay::desktop::Window;

use crate::render::Affine;
use crate::state::{AppState, LayoutMode};

use super::{Piece, SoftPiece, window_app_id};

impl AppState {
    /// Параметры мягкости для окна: конфиг + override по app_id.
    pub(super) fn soft_params(&self, window: &Window) -> crate::softbody::SoftParams {
        let cfg = &self.config.soft_body;
        let softness = self
            .window_override(window)
            .softness
            .filter(|s| *s > 0.0)
            .unwrap_or(cfg.softness);
        crate::softbody::SoftParams {
            softness: softness.clamp(0.0, 1.0),
            impact_squash: cfg.impact_squash.clamp(0.0, 1.0),
            plasticity: cfg.plasticity.clamp(0.0, 1.0),
        }
    }

    /// Делает кусок мягким (если ещё не).
    pub(super) fn make_soft(&mut self, piece: u32, params: crate::softbody::SoftParams) {
        if self.soft.contains_key(&piece) {
            return;
        }
        let Some(rect) = self.piece_by_id(piece).map(|p| p.rect) else {
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
                prev_vel: None,
                rest_generation,
            },
        );
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
    pub fn toggle_soft_focused(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let ids: Vec<u32> = self.pieces_of(&window).map(|p| p.id).collect();
        let soft = ids.iter().any(|id| self.soft.contains_key(id));
        let params = self.soft_params(&window);
        for id in ids {
            if soft {
                self.drop_soft(id);
            } else {
                self.make_soft(id, params);
            }
        }
        println!(
            "[soft] окно {:?}: {}",
            window_app_id(&window),
            if soft {
                "жёсткое"
            } else {
                "мягкое"
            }
        );
        self.needs_render = true;
    }

    /// Super+Shift+ЛКМ: схватить окно за точку и тянуть (деформация).
    pub fn soft_grab_begin(&mut self, sx: f64, sy: f64) -> Option<Window> {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return None;
        }
        let (piece_id, _) = self.piece_at(sx, sy)?;
        let piece = self.piece_by_id(piece_id)?.clone();
        let params = self.soft_params(&piece.window);
        self.make_soft(piece_id, params);
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
            // деформируется, при ударе об пол — сплющивается, в покое — оседает.
            let accel = soft.prev_vel.map_or(Vector::ZERO, |prev| (vel - prev) / dt);
            soft.prev_vel = Some(vel);
            let proper = accel - phys.piece_gravity(id);
            let local = crate::physics::rotate(proper, -angle);
            soft.mesh.step(dt, [local.x, local.y], angvel, &soft.params);
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
