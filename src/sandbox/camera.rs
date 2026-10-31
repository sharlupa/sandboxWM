//! Мир и камера: перевод экран ↔ мир, зум и сдвиг камеры.

use crate::physics::WindowPhysics;
use crate::state::{AppState, LayoutMode};

impl AppState {
    pub fn screen_to_world(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x / self.camera_zoom + self.camera_offset.0,
            y / self.camera_zoom + self.camera_offset.1,
        )
    }

    /// Создаёт физический мир при первой необходимости. Пол — на нижнем
    /// краю экрана над нижней панелью (или из конфига/сессии).
    pub fn ensure_physics(&mut self) -> &mut WindowPhysics {
        if self.physics.is_none() {
            let usable = self.usable_area();
            let floor = self
                .restored_floor_y
                .or(self.config.physics.floor_y)
                .unwrap_or_else(|| (usable.loc.y + usable.size.h) as f32);
            self.physics = Some(WindowPhysics::new(&self.config, floor));
        }
        self.physics.as_mut().expect("physics just created")
    }

    pub fn zoom_camera(&mut self, factor: f64) {
        let (sx, sy) = (self.pointer_location.x, self.pointer_location.y);
        self.zoom_camera_at(factor, sx, sy);
    }

    /// Зум вокруг экранной точки: мировая точка под ней остаётся на месте.
    pub fn zoom_camera_at(&mut self, factor: f64, sx: f64, sy: f64) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        let (wx, wy) = self.screen_to_world(sx, sy);
        let zoom = (self.camera_zoom * factor)
            .clamp(self.config.physics.zoom_min, self.config.physics.zoom_max);
        self.camera_zoom = zoom;
        self.camera_offset = (wx - sx / zoom, wy - sy / zoom);
        self.target_camera_offset = self.camera_offset;
        self.camera_changed();
    }

    pub fn reset_camera(&mut self) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        self.camera_offset = (0.0, 0.0);
        self.target_camera_offset = (0.0, 0.0);
        self.camera_zoom = 1.0;
        self.camera_changed();
    }

    pub fn move_camera(&mut self, dx: f64, dy: f64) {
        self.target_camera_offset.0 += dx / self.camera_zoom;
        self.target_camera_offset.1 += dy / self.camera_zoom;
        self.needs_render = true;
    }

    fn camera_changed(&mut self) {
        self.sync_space_positions();
        self.needs_render = true;
    }

    pub fn physics_camera_pan_begin(&mut self, screen_x: f64, screen_y: f64) -> bool {
        if self.layout_mode != LayoutMode::Physics {
            return false;
        }
        self.camera_pan_active = true;
        self.camera_pan_last_ptr = Some((screen_x, screen_y));
        self.target_camera_offset = self.camera_offset;
        self.needs_render = true;
        true
    }

    /// Сдвигает камеру как схваченный холст; true — событие съедено.
    pub fn physics_camera_pan_update(&mut self, screen_x: f64, screen_y: f64) -> bool {
        if !self.camera_pan_active {
            return false;
        }
        if let Some((last_x, last_y)) = self.camera_pan_last_ptr {
            self.camera_offset.0 -= (screen_x - last_x) / self.camera_zoom;
            self.camera_offset.1 -= (screen_y - last_y) / self.camera_zoom;
            self.target_camera_offset = self.camera_offset;
        }
        self.camera_pan_last_ptr = Some((screen_x, screen_y));
        self.camera_changed();
        true
    }

    pub fn physics_camera_pan_end(&mut self) {
        self.camera_pan_active = false;
        self.camera_pan_last_ptr = None;
    }
}
