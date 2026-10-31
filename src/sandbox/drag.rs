//! Перетаскивание и вращение окна (Super+ЛКМ, Super+A/D).

use std::time::Instant;

use smithay::desktop::Window;

use crate::state::{AppState, LayoutMode};

use super::v2;

impl AppState {
    /// Super+ЛКМ: схватить окно в точке под курсором.
    pub fn physics_drag_begin(&mut self, sx: f64, sy: f64) -> Option<Window> {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return None;
        }
        let (piece, _) = self.piece_at(sx, sy)?;
        let window = self.piece_by_id(piece)?.window.clone();
        let (wx, wy) = self.screen_to_world(sx, sy);
        if self.physics.as_mut()?.drag_begin(piece, v2(wx, wy)) {
            self.dragging = true;
            self.last_physics_tick.get_or_insert_with(Instant::now);
            self.needs_render = true;
            Some(window)
        } else {
            None
        }
    }

    pub fn physics_drag_end(&mut self) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        if let Some(phys) = self.physics.as_mut() {
            phys.drag_end();
        }
        self.needs_render = true;
    }

    /// Удержание Super+A/D: крутить кусок, который держат мышью (Super+ЛКМ),
    /// иначе кусок под курсором, иначе основной кусок активного окна.
    pub fn physics_spin_hold(&mut self, dir: f32) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        let held = self.physics.as_ref().and_then(|p| p.drag_piece());
        let hovered = || {
            let pos = self.pointer_location;
            self.piece_at(pos.x, pos.y).map(|(id, _)| id)
        };
        let focused = || {
            let window = self.get_focused_window()?;
            self.main_piece(&window).map(|p| p.id)
        };
        let Some(piece) = held.or_else(hovered).or_else(focused) else {
            return;
        };
        self.spin_dir = dir.signum();
        self.spin_piece = Some(piece);
        self.needs_render = true;
    }

    pub fn physics_spin_release(&mut self) {
        if self.spin_dir == 0.0 {
            return;
        }
        if let Some(phys) = self.physics.as_mut() {
            phys.set_spin(None);
        }
        self.spin_dir = 0.0;
        self.spin_piece = None;
        self.needs_render = true;
    }
}
