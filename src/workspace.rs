//! Рабочие столы.
//!
//! У каждого стола свои окна, BSP-дерево, режим (тайлинг/физика), физический
//! мир с линиями и связями, куски окон, мягкие тела, камера и полноэкранное
//! окно. Данные активного стола живут прямо в полях [`AppState`] (весь
//! остальной код работает с ними как раньше), а данные остальных — в
//! [`Workspace`]. Переключение меняет их местами (`std::mem::swap`), окна
//! скрытых столов убираются из [`Space`](smithay::desktop::Space): их не
//! видно, они не получают ввод и кадровые колбэки.

use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Size};
use smithay::wayland::seat::WaylandFocus;

use crate::physics::WindowPhysics;
use crate::sandbox::{Piece, SoftPiece};
use crate::state::{AppState, LayoutMode};
use crate::tiling::TileNode;

/// Окно на весь экран и размер, который был у него до этого.
#[derive(Debug, Clone)]
pub struct Fullscreen {
    pub window: Window,
    pub restore_size: Size<i32, Logical>,
}

/// Данные неактивного рабочего стола. Слот активного стола пуст: его
/// данные — в полях [`AppState`].
pub struct Workspace {
    /// Окна в порядке стека (снизу вверх).
    pub windows: Vec<Window>,
    /// Окно с фокусом в момент ухода со стола.
    pub focused: Option<Window>,
    pub tile_tree: Option<TileNode>,
    pub layout_mode: LayoutMode,
    pub physics: Option<WindowPhysics>,
    pub pieces: Vec<Piece>,
    pub soft: HashMap<u32, SoftPiece>,
    pub camera_offset: (f64, f64),
    pub target_camera_offset: (f64, f64),
    pub camera_zoom: f64,
    pub saved_camera: (f64, f64, f64),
    pub physics_sizes: HashMap<Window, Size<i32, Logical>>,
    pub fullscreen: Option<Fullscreen>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            focused: None,
            tile_tree: None,
            layout_mode: LayoutMode::default(),
            physics: None,
            pieces: Vec::new(),
            soft: HashMap::new(),
            camera_offset: (0.0, 0.0),
            target_camera_offset: (0.0, 0.0),
            camera_zoom: 1.0,
            saved_camera: (0.0, 0.0, 1.0),
            physics_sizes: HashMap::new(),
            fullscreen: None,
        }
    }
}

fn is_override_redirect(window: &Window) -> bool {
    window
        .x11_surface()
        .is_some_and(|x| x.is_override_redirect())
}

/// Окно уходит со стола: клиент больше не считает себя активным.
fn deactivate(window: &Window) {
    window.set_activated(false);
    if let Some(toplevel) = window.toplevel() {
        let mut changed = false;
        toplevel.with_pending_state(|state| {
            changed = state.states.contains(xdg_toplevel::State::Activated);
            state.states.unset(xdg_toplevel::State::Activated);
        });
        if changed && toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.set_activated(false);
    }
}

impl AppState {
    /// Меняет местами данные стола `idx` и поля `AppState` (кроме Space).
    fn swap_workspace(&mut self, idx: usize) {
        let ws = &mut self.workspaces[idx];
        std::mem::swap(&mut self.tile_tree, &mut ws.tile_tree);
        std::mem::swap(&mut self.layout_mode, &mut ws.layout_mode);
        std::mem::swap(&mut self.physics, &mut ws.physics);
        std::mem::swap(&mut self.pieces, &mut ws.pieces);
        std::mem::swap(&mut self.soft, &mut ws.soft);
        std::mem::swap(&mut self.camera_offset, &mut ws.camera_offset);
        std::mem::swap(&mut self.target_camera_offset, &mut ws.target_camera_offset);
        std::mem::swap(&mut self.camera_zoom, &mut ws.camera_zoom);
        std::mem::swap(&mut self.saved_camera, &mut ws.saved_camera);
        std::mem::swap(&mut self.physics_sizes, &mut ws.physics_sizes);
        std::mem::swap(&mut self.fullscreen, &mut ws.fullscreen);
    }

    /// Выполняет `f` с данными стола `idx` в полях `AppState`. Space при
    /// этом остаётся от активного стола — `f` не должна на него опираться.
    pub fn with_workspace<R>(&mut self, idx: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        if idx == self.active_workspace || idx >= self.workspaces.len() {
            return f(self);
        }
        self.swap_workspace(idx);
        let r = f(self);
        self.swap_workspace(idx);
        r
    }

    /// Номер скрытого стола, на котором лежит окно (`None` — окно на
    /// активном столе или неизвестно).
    pub fn hidden_workspace_of(&self, window: &Window) -> Option<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .find(|(i, ws)| *i != self.active_workspace && ws.windows.contains(window))
            .map(|(i, _)| i)
    }

    /// Окно скрытого стола, подходящее под условие.
    pub fn find_hidden_window(&self, pred: impl Fn(&Window) -> bool) -> Option<Window> {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.windows.iter())
            .find(|w| pred(w))
            .cloned()
    }

    /// Убирает окно скрытого стола (окно закрылось). `false` — окна там нет.
    pub fn remove_hidden_window(&mut self, window: &Window) -> bool {
        let Some(idx) = self.hidden_workspace_of(window) else {
            return false;
        };
        self.with_workspace(idx, |s| {
            s.physics_remove_window(window);
            if let Some(tree) = s.tile_tree.take() {
                s.tile_tree = tree.remove(window);
            }
            s.physics_sizes.remove(window);
            if s.fullscreen.as_ref().is_some_and(|f| &f.window == window) {
                s.fullscreen = None;
            }
        });
        let ws = &mut self.workspaces[idx];
        ws.windows.retain(|w| w != window);
        if ws.focused.as_ref() == Some(window) {
            ws.focused = None;
        }
        self.pending_configures.remove(window);
        true
    }

    /// Super+цифра: перейти на стол `target` (с нуля).
    pub fn switch_workspace(&mut self, target: usize) {
        let current = self.active_workspace;
        if target >= self.workspaces.len() || target == current {
            return;
        }
        // Анимация перехода и незавершённые захваты относятся к уходящему
        // столу — доводим/сбрасываем их сразу.
        self.finish_transition();
        self.end_resize();
        self.cancel_interactions();

        let focused = self.get_focused_window();
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !is_override_redirect(w))
            .cloned()
            .collect();
        for w in &windows {
            self.space.unmap_elem(w);
            deactivate(w);
        }

        self.swap_workspace(current);
        self.workspaces[current].windows = windows;
        self.workspaces[current].focused = focused;
        self.swap_workspace(target);
        self.active_workspace = target;

        let windows = std::mem::take(&mut self.workspaces[target].windows);
        let focused = self.workspaces[target].focused.take();
        for w in &windows {
            self.space.map_element(w.clone(), (0, 0), false);
        }
        self.after_workspace_shown();

        let focus = self
            .fullscreen_window()
            .or_else(|| focused.filter(|w| windows.contains(w)))
            .or_else(|| windows.last().cloned());
        match focus {
            Some(w) => self.focus_window(&w),
            None => {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(kbd) = self.seat.get_keyboard() {
                    kbd.set_focus(self, None, serial);
                }
            }
        }
        // Под курсором теперь другие окна.
        self.queue_pointer_refresh();
        self.ext_workspace_changed();
        println!("[workspace] рабочий стол {}", target + 1);
    }

    /// Окна стола только что вернулись в Space: расставить по тайлам или
    /// по телам, полноэкранное — на весь экран.
    fn after_workspace_shown(&mut self) {
        // Кэш элементов рендера держит id по номерам кусков, а номера в
        // мирах разных столов совпадают.
        self.render_cache = Default::default();
        self.last_physics_tick = None;
        match self.layout_mode {
            LayoutMode::Tiling => self.recalculate_layout(),
            LayoutMode::Physics => {
                // Окна, перенесённые сюда из другого стола или открытые до
                // первого показа, получают тела.
                let orphans: Vec<Window> = self
                    .space
                    .elements()
                    .filter(|w| !is_override_redirect(w))
                    .filter(|w| self.pieces_of(w).next().is_none())
                    .cloned()
                    .collect();
                for w in orphans {
                    self.physics_spawn_window(&w);
                }
                self.sync_space_positions();
            }
        }
        self.place_fullscreen_window();
        self.needs_render = true;
    }

    /// Super+Shift+цифра: перенести активное окно на стол `target`.
    pub fn move_focused_to_workspace(&mut self, target: usize) {
        if let Some(window) = self.get_focused_window() {
            self.move_window_to_workspace(&window, target);
        }
    }

    /// Переносит окно активного стола на скрытый стол `target`.
    pub fn move_window_to_workspace(&mut self, window: &Window, target: usize) {
        if target >= self.workspaces.len()
            || target == self.active_workspace
            || is_override_redirect(window)
            || !self.space.elements().any(|w| w == window)
        {
            return;
        }
        self.finish_transition();
        self.end_resize();
        self.cancel_interactions();
        if self
            .fullscreen
            .as_ref()
            .is_some_and(|f| &f.window == window)
        {
            self.set_fullscreen(window, false);
        }
        let was_focused = self.get_focused_window().as_ref() == Some(window);

        // Физика у каждого стола своя: тело (и связи) остаются здесь, на
        // новом столе окно получит новое тело.
        self.physics_remove_window(window);
        self.physics_sizes.remove(window);
        self.pending_configures.remove(window);
        if let Some(tree) = self.tile_tree.take() {
            self.tile_tree = tree.remove(window);
        }
        self.space.unmap_elem(window);
        deactivate(window);

        let area = self.tiling_area();
        let ws = &mut self.workspaces[target];
        ws.tile_tree = Some(match ws.tile_tree.take() {
            Some(tree) => {
                let anchor = ws
                    .focused
                    .clone()
                    .filter(|f| tree.contains(f))
                    .or_else(|| tree.windows().last().cloned());
                match anchor {
                    Some(anchor) => tree.insert(&anchor, window.clone(), area),
                    None => tree.append(window.clone(), area),
                }
            }
            None => TileNode::Leaf(window.clone()),
        });
        ws.windows.push(window.clone());
        ws.focused = Some(window.clone());

        if self.layout_mode == LayoutMode::Tiling {
            self.recalculate_layout();
        }
        if was_focused {
            let next = self
                .space
                .elements()
                .rev()
                .find(|w| !is_override_redirect(w))
                .cloned();
            match next {
                Some(w) => self.focus_window(&w),
                None => {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    if let Some(kbd) = self.seat.get_keyboard() {
                        kbd.set_focus(self, None, serial);
                    }
                }
            }
        }
        self.queue_pointer_refresh();
        self.needs_render = true;
        println!(
            "[workspace] окно {:?} → рабочий стол {}",
            crate::sandbox::window_app_id(window),
            target + 1
        );
    }

    /// Окно, которому принадлежит wl_surface, на любом столе.
    pub fn any_window_for_surface(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
            .or_else(|| self.find_hidden_window(|w| w.wl_surface().as_deref() == Some(surface)))
    }
}
