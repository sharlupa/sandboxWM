//! Цель клавиатурного фокуса: Wayland-поверхность или X11-окно.
//!
//! Раньше фокус всегда был `WlSurface`, и X11-окнам (Steam, игры через
//! Wine/Proton, TF2) X-фокус никто не ставил: XWayland оставался в режиме
//! PointerRoot, и клавиши уходили тому X-окну, что оказалось под X-курсором
//! (часто — окну под игрой). Для X11-окна фокус ставится через
//! [`X11Surface`]: `SetInputFocus` и/или `WM_TAKE_FOCUS` по ICCCM, а затем
//! `wl_keyboard.enter` на его поверхность.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::{PopupKind, Window};
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Rectangle, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use crate::state::AppState;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// Фокус для окна: X11-окно — через X11Surface, Wayland — его поверхность.
    pub fn for_window(window: &Window) -> Option<Self> {
        match window.x11_surface() {
            Some(x11) => Some(Self::X11(x11.clone())),
            None => window.wl_surface().map(|s| Self::Wayland(s.into_owned())),
        }
    }

    /// Поверхность фокуса (у X11-окна — после связывания с XWayland).
    pub fn surface(&self) -> Option<WlSurface> {
        self.wl_surface().map(Cow::into_owned)
    }

    /// Фокус принадлежит этому окну.
    pub fn is_window(&self, window: &Window) -> bool {
        match self {
            Self::X11(x11) => window.x11_surface() == Some(x11),
            Self::Wayland(surface) => window.wl_surface().as_deref() == Some(surface),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

/// Попап-грабы смотрят на фокус как на поверхность указателя (корень попапа
/// всегда Wayland-поверхность).
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        match target {
            KeyboardFocusTarget::Wayland(surface) => surface,
            KeyboardFocusTarget::X11(x11) => {
                x11.wl_surface().expect("X11-окно в фокусе без wl_surface")
            }
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.alive(),
            Self::X11(x11) => x11.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            Self::X11(x11) => x11.wl_surface().map(Cow::Owned),
        }
    }
}

impl KeyboardTarget<AppState> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<AppState>,
        data: &mut AppState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
            Self::X11(x11) => KeyboardTarget::enter(x11, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<AppState>, data: &mut AppState, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            Self::X11(x11) => KeyboardTarget::leave(x11, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<AppState>,
        data: &mut AppState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            Self::X11(x11) => KeyboardTarget::key(x11, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<AppState>,
        data: &mut AppState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            Self::X11(x11) => KeyboardTarget::modifiers(x11, seat, data, modifiers, serial),
        }
    }
}

impl AppState {
    /// Set keyboard focus to a specific normal toplevel window.
    pub fn focus_window(&mut self, window: &Window) {
        // X11 override-redirect windows (Steam/CEF menus, tooltips and
        // notifications) manage their own focus/grabs. Treating them as normal
        // toplevels generates FocusOut before ButtonPress and makes clicks pass
        // through the popup.
        if window
            .x11_surface()
            .map_or(false, |x| x.is_override_redirect())
        {
            return;
        }
        // Фокус ушёл на другое окно (Super+стрелки, перенос) — полноэкранное
        // окно возвращается в обычный размер, иначе фокус был бы невидим.
        if let Some(fs) = self.fullscreen_window() {
            if &fs != window {
                self.set_fullscreen(&fs, false);
            }
        }

        let already_focused = self
            .get_focused_window()
            .as_ref()
            .map_or(false, |focused| focused == window);

        // Raise visually first, then mirror the complete order to X11. Raising
        // just the target with X11Wm::raise_window loses CEF popup ordering.
        self.space.raise_element(window, true);
        if already_focused {
            self.sync_x11_stacking();
            self.needs_render = true;
            return;
        }

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for w in &windows {
            let is_target = w == window;
            if let Some(toplevel) = w.toplevel() {
                let mut changed = false;
                toplevel.with_pending_state(|state| {
                    let already = state.states.contains(xdg_toplevel::State::Activated);
                    if is_target && !already {
                        state.states.set(xdg_toplevel::State::Activated);
                        changed = true;
                    } else if !is_target && already {
                        state.states.unset(xdg_toplevel::State::Activated);
                        changed = true;
                    }
                });
                w.set_activated(is_target);
                if changed && toplevel.is_initial_configure_sent() {
                    toplevel.send_configure();
                }
            } else if let Some(x11) = w.x11_surface() {
                // Never deactivate OR popups. Steam's locally-active focus
                // model closes or defocuses them before the pending click.
                if x11.is_override_redirect() {
                    continue;
                }
                let _ = x11.set_activated(is_target);
                w.set_activated(is_target);
            }
        }

        // X11-окну фокус ставится и в X-сервере (SetInputFocus/WM_TAKE_FOCUS
        // по его ICCCM-модели), см. focus.rs; Wayland-окну — его поверхности.
        if let Some(target) = crate::focus::KeyboardFocusTarget::for_window(window) {
            if let Some(kbd) = self.seat.get_keyboard() {
                kbd.set_focus(self, Some(target), serial);
            }
        }

        self.sync_x11_stacking();
        self.needs_render = true;
    }

    /// Move focus to the next or previous window in the stack.
    pub fn focus_next(&mut self, forward: bool) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !w.x11_surface().map_or(false, |x| x.is_override_redirect()))
            .cloned()
            .collect();
        if windows.is_empty() {
            return;
        }
        // Порядок по дереву тайлов стабилен (стек Space меняется при фокусе).
        let ordered: Vec<Window> = match &self.tile_tree {
            Some(tree) => {
                let mut v: Vec<Window> = tree
                    .windows()
                    .into_iter()
                    .filter(|w| windows.contains(w))
                    .collect();
                let rest: Vec<Window> =
                    windows.iter().filter(|w| !v.contains(w)).cloned().collect();
                v.extend(rest);
                v
            }
            None => windows,
        };
        let current = self.get_focused_window();
        let next_idx = match current.and_then(|c| ordered.iter().position(|w| *w == c)) {
            Some(idx) if forward => (idx + 1) % ordered.len(),
            Some(idx) => (idx + ordered.len() - 1) % ordered.len(),
            None => 0,
        };
        let target = ordered[next_idx].clone();
        self.focus_window(&target);
    }

    /// Фокус на ближайшее окно в направлении `(dx, dy)` (тайлинг, Super+стрелки).
    /// Если в ту сторону окон нет — по кругу, как раньше.
    pub fn focus_direction(&mut self, dx: f64, dy: f64) {
        let Some(current) = self.get_focused_window() else {
            self.focus_next(true);
            return;
        };
        let Some(cur) = self.space.element_geometry(&current) else {
            return;
        };
        let center = |r: Rectangle<i32, Logical>| {
            (
                r.loc.x as f64 + r.size.w as f64 * 0.5,
                r.loc.y as f64 + r.size.h as f64 * 0.5,
            )
        };
        let (cx, cy) = center(cur);
        let best = self
            .space
            .elements()
            .filter(|w| **w != current)
            .filter(|w| !w.x11_surface().map_or(false, |x| x.is_override_redirect()))
            .filter_map(|w| {
                let (x, y) = center(self.space.element_geometry(w)?);
                let along = (x - cx) * dx + (y - cy) * dy;
                let across = ((x - cx) * dy - (y - cy) * dx).abs();
                // Окна «впереди»; боковое смещение штрафуется сильнее.
                (along > 1.0).then(|| (w.clone(), along + across * 2.0))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(w, _)| w);
        match best {
            Some(w) => self.focus_window(&w),
            None => self.focus_next(dx + dy > 0.0),
        }
    }

    /// Get currently focused window
    pub fn get_focused_window(&self) -> Option<Window> {
        let kbd = self.seat.get_keyboard()?;
        let focus = kbd.current_focus()?;
        self.space.elements().find(|w| focus.is_window(w)).cloned()
    }
}
