//! xdg-shell (окна и попапы Wayland-клиентов) и xdg-decoration.

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
    find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::input::{Seat, pointer::Focus};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::utils::Serial;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    decoration::XdgDecorationHandler,
};

use crate::state::{AppState, LayoutMode};

impl XdgShellHandler for AppState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Сначала помещаем в (0,0) с заглушкой — размер пересчитается при layout
        surface.with_pending_state(|state| {
            state.size = Some((800, 600).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        // Пока кладём наверху (будет перемещён layout-ом / физикой)
        self.space.map_element(window.clone(), (0, 0), true);

        // Обновляем BSP дерево ВСЕГДА, чтобы при возврате из физического режима
        // окно корректно стало в тайл.
        let focused = self.get_focused_window();
        if let Some(tree) = self.tile_tree.take() {
            let target = focused.unwrap_or_else(|| window.clone());
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.tiling_area()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        // В физике тело появится в commit(), когда клиент впервые отрисует окно.
        if self.layout_mode == LayoutMode::Tiling {
            self.recalculate_layout();
        }

        // Передаём клавиатурный фокус новому окну. Поверх полноэкранного
        // (игра) новое окно не выскакивает: остаётся под ним без фокуса.
        if self.fullscreen_window().is_some() {
            self.place_fullscreen_window();
        } else {
            self.focus_window(&window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        if let Some(window) = window {
            self.remove_window_common(&window);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        match window {
            Some(window) => self.set_fullscreen(&window, true),
            None => {
                // До map: просто подтверждаем состояние (окно встанет в тайл).
                surface.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                });
                if surface.is_initial_configure_sent() {
                    surface.send_configure();
                }
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        if let Some(window) = window {
            self.set_fullscreen(&window, false);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Раньше геометрия попапа не задавалась вовсе: меню GTK/Qt получали
        // configure с нулевым прямоугольником и появлялись в углу окна.
        self.position_popup(&surface);
        let _ = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()));
        let _ = surface.send_configure();
        self.needs_render = true;
    }

    /// Захват для меню: клавиатура и указатель уходят попапу, клик мимо
    /// закрывает цепочку меню (раньше grab игнорировался — меню не
    /// закрывались по клику снаружи).
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Some(root) = find_popup_root_surface(&kind)
            .ok()
            .and_then(|root| self.window_for_surface(&root))
            .and_then(|w| w.wl_surface().map(|s| s.into_owned()))
        else {
            return;
        };
        let root = crate::focus::KeyboardFocusTarget::Wayland(root);
        let Ok(mut grab) = self.popup_manager.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        self.position_popup(&surface);
        surface.send_repositioned(token);
        let _ = surface.send_configure();
        self.needs_render = true;
    }
}
smithay::delegate_xdg_shell!(AppState);

// XDG Decoration — сообщаем клиентам использовать серверные декорации (без CSD)
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

impl XdgDecorationHandler for AppState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}
smithay::delegate_xdg_decoration!(AppState);

impl AppState {
    /// Подгоняет попап под экран (в тайлинге) или просто берёт геометрию
    /// позиционера (в физике окна повёрнуты — «экран» в координатах окна
    /// не определён).
    fn position_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let target = (|| {
            if self.layout_mode != LayoutMode::Tiling {
                return None;
            }
            let root = find_popup_root_surface(&kind).ok()?;
            let window = self.window_for_surface(&root)?;
            let window_geo = self.space.element_geometry(&window)?;
            let output = self.space.outputs().next()?;
            let mut target = self.space.output_geometry(output)?;
            target.loc -= get_popup_toplevel_coords(&kind);
            target.loc -= window_geo.loc;
            Some(target)
        })();
        popup.with_pending_state(|state| {
            state.geometry = match target {
                Some(t) => state.positioner.get_unconstrained_geometry(t),
                None => state.positioner.get_geometry(),
            };
        });
    }
}
