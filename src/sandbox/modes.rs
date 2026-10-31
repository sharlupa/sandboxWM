//! Режимы: тайлинг ↔ физика с плавным переходом, режимы гравитации.

use std::collections::HashMap;
use std::time::Instant;

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};

use crate::state::{AppState, LayoutMode};

use super::{Piece, ScreenPose, Tool, Transition, TransitionItem, v2, window_app_id, wrap_angle};

impl AppState {
    /// Super+G: тайлинг ↔ физика (оба направления — плавно).
    pub fn toggle_physics(&mut self) {
        match self.layout_mode {
            LayoutMode::Tiling => self.enter_physics_mode(),
            LayoutMode::Physics => self.begin_tiling_transition(),
        }
        self.needs_render = true;
    }

    /// Режим 1: обычная гравитация.
    pub fn gravity_normal(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        if let Some(phys) = self.physics.as_mut() {
            phys.set_zero_gravity(false);
        }
        self.needs_render = true;
    }

    /// Режим 2: гравитация выключена — окна плавно встают в тайлинг.
    pub fn gravity_tiling(&mut self) {
        if self.layout_mode == LayoutMode::Physics {
            self.begin_tiling_transition();
        }
    }

    /// Режим 4: невесомость для всех (переключатель).
    pub fn toggle_zero_gravity(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let phys = self.ensure_physics();
        let zero = !phys.zero_gravity();
        phys.set_zero_gravity(zero);
        println!(
            "[physics] невесомость: {}",
            if zero { "вкл" } else { "выкл" }
        );
        self.needs_render = true;
    }

    /// Режим 3: невесомость только для активного окна (переключатель).
    pub fn toggle_float_focused(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let floating = !self.window_floating(&window);
        self.set_window_floating(&window, floating);
        println!(
            "[physics] невесомость окна {:?}: {}",
            window_app_id(&window),
            if floating { "вкл" } else { "выкл" }
        );
        self.needs_render = true;
    }

    pub(super) fn window_floating(&self, window: &Window) -> bool {
        let Some(phys) = self.physics.as_ref() else {
            return false;
        };
        self.pieces_of(window).any(|p| phys.is_floating(p.id))
    }

    pub(super) fn set_window_floating(&mut self, window: &Window, floating: bool) {
        let ids: Vec<u32> = self.pieces_of(window).map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.set_floating(id, floating);
            }
        }
    }

    /// Tiling → Physics. Окна, у которых уже есть тела (были в физике до
    /// тайлинга), плавно возвращаются на свои места; новые — получают тела
    /// в текущих тайловых позициях.
    pub fn enter_physics_mode(&mut self) {
        if self.layout_mode == LayoutMode::Physics {
            return;
        }
        self.exit_fullscreen();
        self.end_resize();
        self.ensure_physics();
        let geoms: Vec<(Window, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter(|w| !Self::is_override_redirect(w))
            .filter_map(|w| Some((w.clone(), self.space.element_geometry(w)?)))
            .collect();

        let cam_to = self.saved_camera;
        let mut items = Vec::new();
        for (window, geo) in geoms {
            let existing: Vec<Piece> = self.pieces_of(&window).cloned().collect();
            if existing.is_empty() {
                let rect = Rectangle::new(
                    (0.0, 0.0).into(),
                    (geo.size.w as f64, geo.size.h as f64).into(),
                );
                // Мир под сохранённой камерой: экранная тайловая позиция → мир.
                let cx = (geo.loc.x as f64 + geo.size.w as f64 * 0.5) / cam_to.2 + cam_to.0;
                let cy = (geo.loc.y as f64 + geo.size.h as f64 * 0.5) / cam_to.2 + cam_to.1;
                let props = self.window_override(&window);
                self.spawn_piece(
                    &window,
                    rect,
                    None,
                    v2(cx, cy),
                    0.0,
                    props.weight.unwrap_or(1.0),
                );
                if props.floating.unwrap_or(false) {
                    self.set_window_floating(&window, true);
                }
                continue;
            }
            let (ww, wh) = (
                window.geometry().size.w.max(1) as f64,
                window.geometry().size.h.max(1) as f64,
            );
            let main = self.main_piece(&window).map(|p| p.id);
            for piece in existing {
                let Some((c, angle)) = self
                    .physics
                    .as_ref()
                    .and_then(|p| p.piece_transform(piece.id))
                else {
                    continue;
                };
                let (pcx, pcy) = piece.center();
                let from = ScreenPose {
                    cx: geo.loc.x as f64 + pcx * geo.size.w as f64 / ww,
                    cy: geo.loc.y as f64 + pcy * geo.size.h as f64 / wh,
                    angle: 0.0,
                    scale: 1.0,
                };
                let angle = angle as f64;
                let to = ScreenPose {
                    cx: (c.x as f64 - cam_to.0) * cam_to.2,
                    cy: (c.y as f64 - cam_to.1) * cam_to.2,
                    angle,
                    scale: cam_to.2,
                };
                items.push(TransitionItem {
                    window: window.clone(),
                    key: piece.id,
                    clip: piece.rect,
                    shape: piece.shape.clone(),
                    popups: Some(piece.id) == main,
                    from: ScreenPose {
                        angle: angle - wrap_angle(angle),
                        ..from
                    },
                    to,
                });
            }
        }

        self.layout_mode = LayoutMode::Physics;
        self.tool = Tool::None;
        self.last_physics_tick = None;
        if items.is_empty() {
            self.camera_offset = (cam_to.0, cam_to.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = cam_to.2;
            self.set_pieces_enabled(true);
            self.restore_physics_sizes();
        } else {
            self.transition = Some(Transition {
                start: Instant::now(),
                duration: self.config.gravity_modes.freeze_transition_secs.max(0.0) as f64,
                to_tiling: false,
                items,
                cam_from: (0.0, 0.0, 1.0),
                cam_to,
            });
        }
        self.sync_space_positions();
        self.needs_render = true;
        println!(
            "[physics] режим включён, {} окон",
            self.space.elements().count()
        );
    }

    /// Physics → Tiling: окна плавно выстраиваются по сетке.
    fn begin_tiling_transition(&mut self) {
        if self.transition.is_some() {
            return;
        }
        self.exit_fullscreen();
        self.cancel_interactions();
        self.saved_camera = (self.camera_offset.0, self.camera_offset.1, self.camera_zoom);
        // Тайлинг поменяет размеры окон — запоминаем физические, чтобы
        // вернуть их при возврате в физику.
        self.physics_sizes = self
            .pieces
            .iter()
            .map(|p| (p.window.clone(), p.window.geometry().size))
            .collect();
        let tiles = self.tile_rects();
        let mut items = Vec::new();
        for piece in self.pieces.clone() {
            let Some(from) = self.piece_screen_pose(&piece) else {
                continue;
            };
            let Some(tile) = tiles.get(&piece.window) else {
                continue;
            };
            let size = piece.window.geometry().size;
            let (ww, wh) = (size.w.max(1) as f64, size.h.max(1) as f64);
            let (pcx, pcy) = piece.center();
            let to = ScreenPose {
                cx: tile.loc.x as f64 + pcx * tile.size.w as f64 / ww,
                cy: tile.loc.y as f64 + pcy * tile.size.h as f64 / wh,
                // Кратчайший доворот до 0.
                angle: from.angle - wrap_angle(from.angle),
                scale: 1.0,
            };
            let popups = self.main_piece(&piece.window).map(|p| p.id) == Some(piece.id);
            items.push(TransitionItem {
                window: piece.window.clone(),
                key: piece.id,
                clip: piece.rect,
                shape: piece.shape.clone(),
                popups,
                from,
                to,
            });
        }
        self.transition = Some(Transition {
            start: Instant::now(),
            duration: self.config.gravity_modes.freeze_transition_secs.max(0.0) as f64,
            to_tiling: true,
            items,
            cam_from: self.saved_camera,
            cam_to: (0.0, 0.0, 1.0),
        });
        self.needs_render = true;
    }

    /// Тайловые прямоугольники окон (как их расставит `recalculate_layout`).
    pub fn tile_rects(&self) -> HashMap<Window, Rectangle<i32, Logical>> {
        let mut rects = Vec::new();
        if let Some(tree) = &self.tile_tree {
            tree.collect_rects(self.tiling_area(), crate::layout::GAPS_IN, &mut rects);
        }
        rects.into_iter().collect()
    }

    pub fn finish_transition(&mut self) {
        let Some(t) = self.transition.take() else {
            return;
        };
        if t.to_tiling {
            self.set_pieces_enabled(false);
            self.camera_offset = (0.0, 0.0);
            self.target_camera_offset = (0.0, 0.0);
            self.camera_zoom = 1.0;
            self.layout_mode = LayoutMode::Tiling;
            // Инструмент, включённый во время анимации, в тайлинге не нужен.
            self.tool = Tool::None;
            self.recalculate_layout();
            println!("[physics] режим выключен, возврат к тайлингу");
        } else {
            self.camera_offset = (t.cam_to.0, t.cam_to.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = t.cam_to.2;
            self.set_pieces_enabled(true);
            self.last_physics_tick = None;
            self.restore_physics_sizes();
            // Окна, открытые во время анимации: их коммит пришёлся на переход,
            // а статичный клиент второго коммита может и не прислать.
            let orphans: Vec<Window> = self
                .space
                .elements()
                .filter(|w| !Self::is_override_redirect(w))
                .filter(|w| self.pieces_of(w).next().is_none())
                .cloned()
                .collect();
            for window in orphans {
                self.physics_spawn_window(&window);
            }
            self.sync_space_positions();
        }
        self.needs_render = true;
    }

    /// Возвращает окнам размеры, которые были у них в физике до тайлинга.
    fn restore_physics_sizes(&mut self) {
        for (window, size) in std::mem::take(&mut self.physics_sizes) {
            if window.geometry().size == size || !self.space.elements().any(|w| *w == window) {
                continue;
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(size));
                toplevel.send_configure();
                self.pending_configures.insert(window.clone());
            } else if let Some(x11) = window.x11_surface() {
                let mut geo = x11.geometry();
                geo.size = size;
                let _ = x11.configure(geo);
            }
        }
    }

    fn set_pieces_enabled(&mut self, enabled: bool) {
        let ids: Vec<u32> = self.pieces.iter().map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.set_piece_enabled(id, enabled);
            }
        }
    }

    /// Сбрасывает незавершённые drag/spin/инструменты.
    pub fn cancel_interactions(&mut self) {
        if let Some(phys) = self.physics.as_mut() {
            phys.drag_end();
            phys.set_spin(None);
        }
        self.dragging = false;
        self.spin_dir = 0.0;
        self.spin_piece = None;
        self.camera_pan_active = false;
        self.camera_pan_last_ptr = None;
        self.tool = Tool::None;
        self.pointer_grab_target = None;
        self.soft_grab_end();
    }
}
