//! Шаг симуляции: физика, переходы и положение окон в Space.

use std::time::Instant;

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use crate::state::{AppState, LayoutMode};

use super::{ease, v2};

impl AppState {
    /// Один кадр физического режима. Вызывается из рендер-цикла.
    pub fn step_physics(&mut self) {
        let now = Instant::now();
        // Автосохранение и ожидание восстанавливаемых окон — в любом режиме.
        self.maybe_autosave(now);
        if self.layout_mode != LayoutMode::Physics {
            return;
        }
        if let Some(t) = self.transition.as_ref() {
            let p = t.progress(now);
            let e = ease(p);
            let cam = (
                t.cam_from.0 + (t.cam_to.0 - t.cam_from.0) * e,
                t.cam_from.1 + (t.cam_to.1 - t.cam_from.1) * e,
                t.cam_from.2 + (t.cam_to.2 - t.cam_from.2) * e,
            );
            self.camera_offset = (cam.0, cam.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = cam.2;
            self.needs_render = true;
            if p >= 1.0 {
                self.finish_transition();
            }
            return;
        }

        let elapsed = self
            .last_physics_tick
            .map_or(self.config.physics.physics_dt as f64, |t| {
                now.duration_since(t).as_secs_f64()
            })
            .min(0.25);
        self.last_physics_tick = Some(now);

        let spin_rate = self.config.physics.spin_rate;
        let spin = if self.spin_dir != 0.0 {
            self.spin_piece.map(|p| (p, self.spin_dir * spin_rate))
        } else {
            None
        };
        let drag_target = self
            .dragging
            .then(|| self.screen_to_world(self.pointer_location.x, self.pointer_location.y));
        let Some(phys) = self.physics.as_mut() else {
            return;
        };
        phys.set_spin(spin);
        if let Some((x, y)) = drag_target {
            phys.drag_update(v2(x, y));
        }
        let steps = phys.advance(elapsed as f32);
        let mut moving = phys.any_moving() || spin.is_some();
        moving |= self.step_soft(elapsed as f32);

        // Плавная камера (Lerp).
        let lerp = self.config.physics.camera_lerp.clamp(0.01, 1.0);
        let cam_dx = self.target_camera_offset.0 - self.camera_offset.0;
        let cam_dy = self.target_camera_offset.1 - self.camera_offset.1;
        let mut camera_moved = false;
        if cam_dx * cam_dx + cam_dy * cam_dy > 0.25 / (self.camera_zoom * self.camera_zoom) {
            self.camera_offset.0 += cam_dx * lerp;
            self.camera_offset.1 += cam_dy * lerp;
            camera_moved = true;
            moving = true;
        } else if cam_dx != 0.0 || cam_dy != 0.0 {
            self.camera_offset = self.target_camera_offset;
            camera_moved = true;
        }

        if steps > 0 || camera_moved {
            self.sync_space_positions();
        }
        if moving || camera_moved {
            self.needs_render = true;
        }
    }

    /// Нужно ли держать кадры (физика/камера/переход ещё в движении).
    pub fn physics_animating(&self) -> bool {
        if self.layout_mode != LayoutMode::Physics {
            return false;
        }
        self.transition.is_some()
            || self.physics.as_ref().map_or(false, |p| p.any_moving())
            || self.spin_dir != 0.0
            || self.soft_grab.is_some()
            || self.soft.values().any(|s| s.mesh.is_active())
            || self.target_camera_offset != self.camera_offset
    }

    /// Обновляет позиции окон в [`Space`] (output enter/leave, кадры) и
    /// X11-геометрию, не меняя порядок окон в стеке.
    ///
    /// `Space::map_element` поднимает элемент наверх, а позиции применялись
    /// каждый кадр в порядке обхода HashMap — z-порядок окон в физике
    /// перемешивался каждый кадр, а меню Steam уходили под окна.
    pub fn sync_space_positions(&mut self) {
        if self.layout_mode != LayoutMode::Physics {
            return;
        }
        let order: Vec<Window> = self.space.elements().cloned().collect();
        let mut moved = false;
        for window in &order {
            // Полноэкранное окно стоит в начале экрана (place_fullscreen_window).
            if Self::is_override_redirect(window) || self.is_fullscreen(window) {
                continue;
            }
            let Some(piece) = self.main_piece(window).cloned() else {
                continue;
            };
            let Some(xf) = self.piece_xf(&piece) else {
                continue;
            };
            let bounds = xf.bounds(piece.rect);
            let loc = Point::<i32, Logical>::from((
                bounds.loc.x + self.camera_offset.0.round() as i32,
                bounds.loc.y + self.camera_offset.1.round() as i32,
            ));
            if self.space.element_location(window) != Some(loc) {
                self.space.map_element(window.clone(), loc, false);
                moved = true;
            }
            // X11: X-геометрия в ЭКРАННЫХ координатах — X-сервер клампит
            // указатель к своему экрану, мировые координаты рвут клики CEF.
            if let Some(x11) = window.x11_surface() {
                let (ox, oy) = xf.apply(0.0, 0.0);
                let target = Rectangle::new(
                    (ox.round() as i32, oy.round() as i32).into(),
                    window.geometry().size,
                );
                if x11.geometry() != target {
                    let _ = x11.configure(target);
                }
            }
        }
        if moved {
            for window in &order {
                self.space.raise_element(window, false);
            }
        }
        self.place_fullscreen_window();
    }
}
