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
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
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
