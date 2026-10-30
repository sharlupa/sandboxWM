//! Окно на весь экран — в тайлинге и в физике (игры, видео).
//!
//! Полноэкранное окно рисуется одно, без трансформ камеры и обводки, в
//! экранных координатах `(0, 0)` и получает ввод напрямую. В физике его тело
//! на это время выключено (остальные окна сквозь него проходят), после
//! выхода окно возвращается к прежнему размеру и месту. Верхние панели
//! (layer-shell `top`, waybar) под полноэкранным окном не видны, `overlay` —
//! видны.

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::state::{AppState, LayoutMode};
use crate::workspace::Fullscreen;

impl AppState {
    /// Полноэкранное окно активного стола.
    pub fn fullscreen_window(&self) -> Option<Window> {
        self.fullscreen
            .as_ref()
            .map(|f| f.window.clone())
            .filter(|w| self.space.elements().any(|e| e == w))
    }

    pub fn is_fullscreen(&self, window: &Window) -> bool {
        self.fullscreen
            .as_ref()
            .is_some_and(|f| &f.window == window)
    }

    /// Super+F: активное окно на весь экран ↔ обратно.
    pub fn toggle_fullscreen_focused(&mut self) {
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let on = !self.is_fullscreen(&window);
        self.set_fullscreen(&window, on);
    }

    /// Выйти из полноэкранного режима на активном столе (перед сменой
    /// режима тайлинг ↔ физика и т.п.).
    pub fn exit_fullscreen(&mut self) {
        if let Some(f) = self.fullscreen.clone() {
            self.set_fullscreen(&f.window, false);
        }
    }

    /// Включить/выключить полноэкранный режим окна (запрос клиента или Super+F).
    pub fn set_fullscreen(&mut self, window: &Window, on: bool) {
        if window
            .x11_surface()
            .is_some_and(|x| x.is_override_redirect())
        {
            return;
        }
        if self.space.elements().any(|w| w == window) {
            self.set_fullscreen_impl(window, on, true);
        } else if let Some(idx) = self.hidden_workspace_of(window) {
            // Окно на другом столе: запоминаем, на экран оно выйдет при
            // переключении на его стол.
            self.with_workspace(idx, |s| s.set_fullscreen_impl(window, on, false));
        } else {
            // Ещё не в Space (запрос до первого map) — просто отвечаем клиенту.
            self.configure_fullscreen(window, on, self.output_size());
        }
        self.needs_render = true;
    }

    fn set_fullscreen_impl(&mut self, window: &Window, on: bool, visible: bool) {
        let screen = self.output_size();
        if on {
            match self.fullscreen.clone() {
                Some(cur) if &cur.window == window => {
                    // Повторный запрос: клиент ждёт configure в ответ.
                    self.configure_fullscreen(window, true, screen);
                    if visible {
                        self.place_fullscreen_window();
                    }
                    return;
                }
                Some(cur) => self.set_fullscreen_impl(&cur.window, false, visible),
                None => {}
            }
            if visible {
                self.finish_transition();
                self.end_resize();
                self.cancel_interactions();
            }
            let restore_size = self
                .physics_sizes
                .get(window)
                .copied()
                .unwrap_or_else(|| window.geometry().size);
            self.fullscreen = Some(Fullscreen {
                window: window.clone(),
                restore_size,
            });
            self.set_window_bodies_enabled(window, false);
            self.configure_fullscreen(window, true, screen);
            if visible {
                self.place_fullscreen_window();
                self.focus_window(window);
                crate::input::refresh_pointer_focus(self);
            }
            println!(
                "[fullscreen] {:?}: на весь экран",
                crate::sandbox::window_app_id(window)
            );
        } else {
            let Some(cur) = self.fullscreen.clone().filter(|f| &f.window == window) else {
                // Окно и не было полноэкранным — подтверждаем клиенту.
                self.configure_fullscreen(window, false, window.geometry().size);
                return;
            };
            self.fullscreen = None;
            let size = match self.layout_mode {
                LayoutMode::Tiling => self
                    .tile_rects()
                    .get(window)
                    .map(|r| r.size)
                    .unwrap_or(cur.restore_size),
                LayoutMode::Physics => cur.restore_size,
            };
            self.configure_fullscreen(window, false, size);
            if self.layout_mode == LayoutMode::Physics && self.transition.is_none() {
                self.set_window_bodies_enabled(window, true);
            }
            if visible {
                match self.layout_mode {
                    LayoutMode::Tiling => self.recalculate_layout(),
                    LayoutMode::Physics => {
                        // Окно открылось сразу на весь экран — тела ещё нет.
                        if self.transition.is_none() && self.pieces_of(window).next().is_none() {
                            self.physics_spawn_window(window);
                        }
                        self.sync_space_positions();
                    }
                }
                crate::input::refresh_pointer_focus(self);
            }
            println!(
                "[fullscreen] {:?}: обычный размер",
                crate::sandbox::window_app_id(window)
            );
        }
    }

    /// Тела кусков окна в физическом мире стола.
    fn set_window_bodies_enabled(&mut self, window: &Window, enabled: bool) {
        let ids: Vec<u32> = self.pieces_of(window).map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.set_piece_enabled(id, enabled);
            }
        }
        self.last_physics_tick = None;
    }

    /// Состояние Fullscreen и размер — клиенту (configure).
    fn configure_fullscreen(&mut self, window: &Window, on: bool, size: Size<i32, Logical>) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                if on {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                }
                state.size = Some(size);
            });
            if toplevel.is_initial_configure_sent() {
                // Без backpressure: смена режима — не поток ресайзов.
                toplevel.send_configure();
                self.pending_configures.insert(window.clone());
            }
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_fullscreen(on);
            let geo = if on {
                Rectangle::from_size(size)
            } else {
                Rectangle::new(x11.geometry().loc, size)
            };
            let _ = x11.configure(geo);
        }
    }

    /// Начало экрана в координатах Space (в физике выход сдвинут камерой).
    fn screen_origin_in_space(&self) -> Point<i32, Logical> {
        match self.layout_mode {
            LayoutMode::Tiling => (0, 0).into(),
            LayoutMode::Physics => {
                (self.camera_offset.0 as i32, self.camera_offset.1 as i32).into()
            }
        }
    }

    /// Ставит полноэкранное окно в начало экрана поверх остальных.
    pub fn place_fullscreen_window(&mut self) {
        let Some(window) = self.fullscreen_window() else {
            return;
        };
        let loc = self.screen_origin_in_space();
        if self.space.element_location(&window) != Some(loc) {
            self.space.map_element(window.clone(), loc, false);
        }
        self.space.raise_element(&window, false);
        if let Some(x11) = window.x11_surface() {
            let target = Rectangle::from_size(self.output_size());
            if x11.geometry() != target {
                let _ = x11.configure(target);
            }
        }
        self.sync_x11_stacking();
    }
}
