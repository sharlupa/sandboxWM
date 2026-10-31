//! Ввод: события бэкендов (winit во вложенном режиме, libinput в TTY) →
//! клавиатура и указатель.
//!
//! - [`keyboard`] — клавиши клиентам и горячие клавиши WM;
//! - [`pointer`] — движение, кнопки, колесо и фокус указателя;
//! - [`constraints`] — захват указателя и относительная мышь для игр.

pub mod constraints;
mod keyboard;
pub mod pointer;

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, Event, InputBackend, InputEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};

use crate::state::AppState;

pub use pointer::refresh_pointer_focus;

/// Событие ввода любого бэкенда.
pub fn process_event<B: InputBackend>(state: &mut AppState, event: InputEvent<B>) {
    match event {
        InputEvent::Keyboard { event } => keyboard::handle_keyboard::<B>(state, event),
        InputEvent::PointerMotion { event } => {
            let target = state.pointer_location + event.delta();
            pointer::pointer_moved(
                state,
                target,
                event.delta(),
                event.delta_unaccel(),
                event.time(),
                event.time_msec(),
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            // Хост (winit), планшет или VM-указатель дают только абсолютные
            // координаты. Относительное движение для игр считается от прошлой
            // позиции курсора хоста: под захватом наш курсор стоит на месте,
            // и разница с ним накапливалась бы.
            let pos = event.position_transformed(state.output_size());
            let delta = pos - state.host_pointer_location.unwrap_or(pos);
            state.host_pointer_location = Some(pos);
            pointer::pointer_moved(state, pos, delta, delta, event.time(), event.time_msec());
        }
        InputEvent::PointerAxis { event } => {
            pointer::handle_pointer_axis(
                state,
                event.amount(Axis::Horizontal).unwrap_or(0.0),
                event.amount(Axis::Vertical).unwrap_or(0.0),
                event.amount_v120(Axis::Horizontal),
                event.amount_v120(Axis::Vertical),
                event.source(),
                event.time_msec(),
            );
        }
        InputEvent::PointerButton { event } => {
            pointer::handle_pointer_button(
                state,
                event.button_code(),
                event.state(),
                event.time_msec(),
            );
        }
        _ => {}
    }
}

/// `SANDBOXWM_DEBUG_POINTER=1`: лог захвата указателя и движений мыши
/// (диагностика обзора мышью в играх).
pub fn debug_pointer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SANDBOXWM_DEBUG_POINTER").is_some_and(|v| v != "0"))
}
