//! Игры: захват указателя (zwp_pointer_constraints) и относительное
//! движение мыши (zwp_relative_pointer).
//!
//! Захват включается, когда указатель над поверхностью, которая его
//! попросила, и ей можно его дать ([`allowed`]): у её окна клавиатурный
//! фокус или это полноэкранное окно (игра). Ушёл клавиатурный фокус —
//! захват снимается.
//!
//! Пока захват (lock) активен, курсор композитора стоит, клиент получает
//! только относительное движение, а положение курсора следует подсказке
//! клиента (cursor position hint). Так XWayland сообщает, где X-курсор игры:
//! с ним совпадают и `enter` при возврате указателя на игру, и курсор после
//! снятия захвата.

use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::PointerHandle;
use smithay::reexports::wayland_protocols::wp::pointer_constraints::zv1::server::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintUserData, PointerConstraintsHandler,
    PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::XWaylandClientData;

use super::debug_pointer;
use crate::state::{AppState, LayoutMode};

/// Активный захват указателя.
pub enum Active {
    /// Курсор стоит на месте.
    Locked,
    /// Курсор не выходит из прямоугольника окна (экранные координаты).
    Confined(Rectangle<f64, Logical>),
}

/// Активный захват у поверхности под указателем.
pub fn active(state: &AppState, ptr: &PointerHandle<AppState>) -> Option<Active> {
    let surface = ptr.current_focus()?;
    let locked = with_pointer_constraint(&surface, ptr, |c| match c.as_deref() {
        Some(c) if c.is_active() => Some(matches!(c, PointerConstraint::Locked(_))),
        _ => None,
    })?;
    if locked {
        return Some(Active::Locked);
    }
    // В физике окна повёрнуты — прямоугольника на экране нет, курсор не держим.
    confine_rect(state, &surface).map(Active::Confined)
}

/// У поверхности активен захват lock.
pub fn is_locked(ptr: &PointerHandle<AppState>, surface: &WlSurface) -> bool {
    with_pointer_constraint(surface, ptr, |c| {
        c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
    })
}

/// Захват у поверхности под указателем — для отладочного лога.
pub fn describe(ptr: &PointerHandle<AppState>) -> String {
    let Some(surface) = ptr.current_focus() else {
        return "-".into();
    };
    with_pointer_constraint(&surface, ptr, |c| match c.as_deref() {
        Some(c) => {
            let kind = match c {
                PointerConstraint::Locked(_) => "lock",
                PointerConstraint::Confined(_) => "confine",
            };
            format!("{kind}{}", if c.is_active() { "+active" } else { "" })
        }
        None => "-".into(),
    })
}

/// Корень дерева поверхностей (подповерхность → её окно).
pub fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
        root = parent;
    }
    root
}

/// Можно ли дать захват поверхности `surface` при клавиатурном фокусе
/// `keyboard_focus`. Окну с фокусом — да (иначе мышь «залипала» бы при
/// наведении на чужое окно). Полноэкранному окну — и без фокуса (другого
/// окна под курсором всё равно нет; у X11-игры фокус появляется только
/// после связи окна с поверхностью), но не когда клавиатуру забрала
/// поверхность слоя: лаунчеру поверх игры нужна мышь.
fn allowed(state: &AppState, surface: &WlSurface, keyboard_focus: Option<&WlSurface>) -> bool {
    let root = root_surface(surface);
    if keyboard_focus == Some(&root) {
        return true;
    }
    let fullscreen = state
        .fullscreen_window()
        .is_some_and(|w| w.wl_surface().as_deref() == Some(&root));
    let layer_focused = keyboard_focus.is_some_and(|f| {
        state.space.outputs().any(|o| {
            layer_map_for_output(o)
                .layer_for_surface(f, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
    });
    fullscreen && !layer_focused
}

fn keyboard_focus(state: &AppState) -> Option<WlSurface> {
    state
        .seat
        .get_keyboard()
        .and_then(|k| k.current_focus())
        .and_then(|f| f.surface())
}

/// Захват, который включится, как только указатель окажется на
/// поверхности (см. [`on_enter`]).
pub struct Activation {
    surface: WlSurface,
    /// Начало поверхности в координатах фокуса.
    origin: Point<f64, Logical>,
    /// Где клиент видит курсор (координаты поверхности), если сообщил.
    hint: Option<Point<f64, Logical>>,
}

/// Захват поверхности `surface` (начало `origin` в координатах фокуса),
/// который нужно включить, когда указатель встанет в `location`.
pub fn on_enter(
    state: &AppState,
    ptr: &PointerHandle<AppState>,
    surface: &WlSurface,
    origin: Point<f64, Logical>,
    location: Point<f64, Logical>,
) -> Option<Activation> {
    if !allowed(state, surface, keyboard_focus(state).as_ref()) {
        return None;
    }
    with_pointer_constraint(surface, ptr, |c| {
        let c = c?;
        if c.is_active() {
            return None;
        }
        let hint = match &*c {
            PointerConstraint::Locked(locked) => locked.cursor_position_hint(),
            PointerConstraint::Confined(_) => None,
        };
        // Захват действует только в своей области поверхности.
        let point = hint.unwrap_or(location - origin).to_i32_round();
        c.region()
            .is_none_or(|r| r.contains(point))
            .then(|| Activation {
                surface: surface.clone(),
                origin,
                hint,
            })
    })
}

impl Activation {
    /// Курсор — туда, где его видит клиент (без событий клиенту).
    /// Возвращает это место в координатах фокуса.
    pub fn place(&self, state: &mut AppState) -> Option<Point<f64, Logical>> {
        let hint = self.hint?;
        if let Some(origin) = state.surface_screen_origin(&self.surface) {
            state.pointer_location = origin + hint;
            state.needs_render = true;
        }
        Some(self.origin + hint)
    }

    /// Включить захват (`locked`/`confined` клиенту).
    pub fn activate(self, ptr: &PointerHandle<AppState>) {
        with_pointer_constraint(&self.surface, ptr, |c| {
            if let Some(c) = c.filter(|c| !c.is_active()) {
                if debug_pointer() {
                    eprintln!(
                        "[pointer] constraint on: surface {} hint {:?}",
                        self.surface.id().protocol_id(),
                        self.hint
                    );
                }
                c.activate();
            }
        });
    }
}

impl PointerConstraintsHandler for AppState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if debug_pointer() {
            eprintln!(
                "[pointer] new constraint surface={} pointer_focus={:?}",
                surface.id().protocol_id(),
                pointer.current_focus().map(|s| s.id().protocol_id())
            );
        }
        // Smithay 0.7 зовёт этот обработчик посреди запроса lock_pointer /
        // confine_pointer, пока объект захвата ещё создаётся. Включаем захват
        // на ближайшем коммите поверхности (или при движении мыши), уже вне
        // этого запроса.
        if !self.pending_constraints.contains(surface) {
            self.pending_constraints.push(surface.clone());
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        });
        if debug_pointer() {
            eprintln!("[pointer] cursor hint {location:?} active={active}");
        }
        // Подсказку неактивного захвата Smithay запоминает: её применит
        // включение захвата (см. `Activation::place`).
        if !active {
            return;
        }
        if let Some(origin) = self.surface_screen_origin(surface) {
            let pos = origin + location;
            pointer.set_location(pos);
            self.pointer_location = pos;
            self.needs_render = true;
        }
    }
}

impl AppState {
    /// Коммит поверхности: включить захват, который она создала после
    /// прошлого коммита.
    pub(crate) fn activate_pending_constraint(&mut self, surface: &WlSurface) {
        self.pending_constraints.retain(|s| s.is_alive());
        let Some(i) = self.pending_constraints.iter().position(|s| s == surface) else {
            return;
        };
        self.pending_constraints.swap_remove(i);
        let Some(ptr) = self.seat.get_pointer() else {
            return;
        };
        if ptr.current_focus().as_ref() != Some(surface) {
            // Полноэкранная игра закрывает весь экран: указатель переводим на
            // неё сразу, не дожидаясь движения мыши (захват включится при
            // входе). Остальным — при входе указателя на поверхность.
            let fullscreen = self
                .fullscreen_window()
                .is_some_and(|w| w.wl_surface().as_deref() == Some(surface));
            if fullscreen {
                crate::input::refresh_pointer_focus(self);
            }
            return;
        }
        let Some((_, origin)) = self.pointer_focus.clone() else {
            return;
        };
        if let Some(activation) = on_enter(self, &ptr, surface, origin, ptr.current_location()) {
            if let Some(location) = activation.place(self) {
                ptr.set_location(location);
            }
            activation.activate(&ptr);
        }
    }

    /// Клавиатурный фокус перешёл на `focused`: захват поверхности под
    /// указателем снимается, если больше не разрешён, — иначе
    /// заблокированный курсор так и не покинул бы окно игры.
    ///
    /// Вызывается из `SeatHandler::focus_changed`, поэтому фокус берётся из
    /// аргумента: клавиатура Smithay в этот момент занята.
    pub(crate) fn release_constraint_on_focus_change(&mut self, focused: Option<&WlSurface>) {
        let Some(ptr) = self.seat.get_pointer() else {
            return;
        };
        let Some(surface) = ptr.current_focus() else {
            return;
        };
        if allowed(self, &surface, focused) {
            return;
        }
        with_pointer_constraint(&surface, &ptr, |constraint| {
            if let Some(c) = constraint.filter(|c| c.is_active()) {
                if debug_pointer() {
                    eprintln!(
                        "[pointer] constraint off: keyboard focus {:?} left pointer surface {}",
                        focused.map(|s| s.id().protocol_id()),
                        surface.id().protocol_id()
                    );
                }
                c.deactivate();
            }
        });
    }

    /// Где на экране начало поверхности окна (тайлинг или полноэкранное
    /// окно; в физике окна повёрнуты/масштабированы — `None`).
    pub fn surface_screen_origin(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let window = self.window_for_surface(surface)?;
        let geo_loc = window.geometry().loc;
        if self.is_fullscreen(&window) {
            return Some((Point::<i32, Logical>::default() - geo_loc).to_f64());
        }
        if self.layout_mode != LayoutMode::Tiling {
            return None;
        }
        let loc = self.space.element_location(&window)?;
        Some((loc - geo_loc).to_f64())
    }
}

/// Экранный прямоугольник окна, в котором удерживается курсор (confine).
fn confine_rect(state: &AppState, surface: &WlSurface) -> Option<Rectangle<f64, Logical>> {
    let origin = state.surface_screen_origin(surface)?;
    let window = state.any_window_for_surface(surface)?;
    let geo = window.geometry();
    Some(Rectangle::new(origin + geo.loc.to_f64(), geo.size.to_f64()))
}

// ── Глобалы ────────────────────────────────────────────────────────────

/// Данные глобала zwp_pointer_constraints_v1.
pub struct ConstraintsGlobal {
    /// Виден ли глобал клиенту XWayland (`[input] xwayland_pointer_lock`).
    xwayland: bool,
}

/// Глобал захвата указателя. Свой вместо `PointerConstraintsState::new`,
/// чтобы его можно было спрятать от XWayland: с захватом XWayland
/// эмулирует «вернуть курсор в центр» у X11-игр, без него игры получают
/// только «сырые» движения мыши (диагностика).
pub fn create_global(dh: &DisplayHandle, xwayland: bool) -> GlobalId {
    dh.create_global::<AppState, ZwpPointerConstraintsV1, _>(1, ConstraintsGlobal { xwayland })
}

impl GlobalDispatch<ZwpPointerConstraintsV1, ConstraintsGlobal> for AppState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpPointerConstraintsV1>,
        _global_data: &ConstraintsGlobal,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &ConstraintsGlobal) -> bool {
        global_data.xwayland || client.get_data::<XWaylandClientData>().is_none()
    }
}

smithay::reexports::wayland_server::delegate_dispatch!(AppState: [
    ZwpPointerConstraintsV1: ()
] => PointerConstraintsState);
smithay::reexports::wayland_server::delegate_dispatch!(AppState: [
    ZwpConfinedPointerV1: PointerConstraintUserData<AppState>
] => PointerConstraintsState);
smithay::reexports::wayland_server::delegate_dispatch!(AppState: [
    ZwpLockedPointerV1: PointerConstraintUserData<AppState>
] => PointerConstraintsState);
smithay::delegate_relative_pointer!(AppState);
