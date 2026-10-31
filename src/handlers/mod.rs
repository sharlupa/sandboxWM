//! Обработчики Wayland-протоколов, которые реализует [`AppState`].
//!
//! Здесь только ответы на запросы клиентов и делегирование в Smithay;
//! логика WM (раскладка, фокус, физика, ввод) живёт в своих модулях.
//!
//! [`AppState`]: crate::state::AppState

mod compositor;
mod output;
mod seat;
mod sync;
mod xdg_shell;
