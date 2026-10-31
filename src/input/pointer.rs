//! Указатель: движение, кнопки, колесо и фокус.
//!
//! Движение сначала достаётся взаимодействиям WM (ресайз тайла,
//! панорамирование камеры, инструменты и перетаскивание в физике), иначе —
//! клиенту под курсором ([`pointer_focus`]). Фокус и `wl_pointer.motion`
//! клиентам отдаёт одна функция, [`send_motion`].
//!
//! Лишний `enter` или `motion` без относительного движения — не мелочь:
//! XWayland переносит X-курсор в его координаты, и игра, которая держит
//! курсор в центре окна, видит рывок к курсору композитора. Поэтому
//! [`send_motion`] не повторяет то же положение и молчит, пока указатель
//! захвачен (lock), а фокус пересчитывается, как только под курсором
//! появилось окно ([`AppState::queue_pointer_refresh`]): игра получает
//! `enter` до того, как захватит мышь и вернёт курсор в центр.

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::desktop::{Window, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer;

use super::constraints::{self, Active};
use super::debug_pointer;
use super::keyboard::super_held;
use crate::state::{AppState, LayoutMode};

// BTN_LEFT = 272 (0x110)
const BTN_LEFT: u32 = 272;
const BTN_MIDDLE: u32 = 274;

/// Поверхность под указателем и её начало в координатах, в которых
/// считается положение для клиента (экран; в физике — плоскость окна).
pub type FocusSurface = (WlSurface, Point<f64, Logical>);

/// Фокус указателя (`None` — над пустым столом).
pub type PointerFocus = Option<FocusSurface>;

/// Движение мыши: относительное событие (игры: обзор мышью через
/// zwp_relative_pointer, у XWayland — XI2 raw motion) и, если указатель
/// не захвачен, обычное перемещение курсора. Захват (lock) держит курсор на
/// месте, confine не выпускает его из окна.
pub(super) fn pointer_moved(
    state: &mut AppState,
    mut target: Point<f64, Logical>,
    delta: Point<f64, Logical>,
    delta_unaccel: Point<f64, Logical>,
    utime: u64,
    time_msec: u32,
) {
    let Some(ptr) = state.seat.get_pointer() else {
        return;
    };
    if debug_pointer() {
        eprintln!(
            "[pointer] move delta=({:.1},{:.1}) at=({:.0},{:.0}) focus={:?} constraint={} fullscreen={}",
            delta.x,
            delta.y,
            state.pointer_location.x,
            state.pointer_location.y,
            ptr.current_focus().map(|s| s.id().protocol_id()),
            constraints::describe(&ptr),
            state.fullscreen_window().is_some()
        );
    }
    let relative = (delta != Point::default() || delta_unaccel != Point::default()).then_some(
        RelativeMotionEvent {
            delta,
            delta_unaccel,
            utime,
        },
    );
    match constraints::active(state, &ptr) {
        Some(Active::Locked) => {
            // Курсор стоит: клиенту — только относительное движение.
            if let Some(event) = relative {
                let focus = state.pointer_focus.clone();
                ptr.relative_motion(state, focus, &event);
            }
            ptr.frame(state);
            return;
        }
        Some(Active::Confined(rect)) => {
            target.x = target.x.clamp(rect.loc.x, rect.loc.x + rect.size.w - 1.0);
            target.y = target.y.clamp(rect.loc.y, rect.loc.y + rect.size.h - 1.0);
        }
        None => {}
    }
    if !move_pointer(state, target, SERIAL_COUNTER.next_serial(), time_msec) {
        return;
    }
    if let Some(event) = relative {
        let focus = state.pointer_focus.clone();
        ptr.relative_motion(state, focus, &event);
    }
    ptr.frame(state);
}

/// Курсор в точку экрана `target`: взаимодействия WM или фокус и motion
/// клиенту (frame — за вызывающим). `false` — движение съел WM (ресайз
/// тайла, панорамирование камеры) и клиентам ничего не ушло.
fn move_pointer(
    state: &mut AppState,
    target: Point<f64, Logical>,
    serial: Serial,
    time: u32,
) -> bool {
    let screen = state.output_size();
    let pos: Point<f64, Logical> = (
        target.x.clamp(0.0, (screen.w - 1) as f64),
        target.y.clamp(0.0, (screen.h - 1) as f64),
    )
        .into();
    state.pointer_location = pos;
    // DRM: программный курсор → нужен кадр. Winit: курсор рисует хост.
    if state.session.is_some() {
        state.needs_render = true;
    }

    // Полноэкранное окно получает указатель как есть.
    if state.fullscreen_window().is_none() {
        match state.layout_mode {
            LayoutMode::Physics => {
                // Удержание средней кнопки двигает камеру как схваченный холст.
                if state.physics_camera_pan_update(pos.x, pos.y) {
                    return false;
                }
                state.tool_motion(pos.x, pos.y);
                state.soft_grab_update(pos.x, pos.y);
                if state.dragging {
                    state.needs_render = true;
                }
                if state.dragging || state.tool_active() || state.soft_grab_active() {
                    // Пока тащим окно или работаем инструментом, ховер клиентам не нужен.
                    send_motion(state, None, pos, serial, time);
                    return true;
                }
            }
            LayoutMode::Tiling => {
                if tile_resize_motion(state, pos) {
                    return false;
                }
            }
        }
    }
    let (focus, location, _) = pointer_focus(state, pos.x, pos.y);
    send_motion(state, focus, location, serial, time);
    true
}

/// Super+ЛКМ в тайлинге: ресайз тайла за курсором. `true` — ресайз идёт.
fn tile_resize_motion(state: &mut AppState, pos: Point<f64, Logical>) -> bool {
    let (Some(resize_win), Some((start_x, start_y)), Some((drag_left, drag_top))) = (
        state.resize_window.clone(),
        state.resize_start_ptr,
        state.resize_edges,
    ) else {
        return false;
    };
    let dx = pos.x - start_x;
    let dy = pos.y - start_y;
    if dx.abs() < 1.0 && dy.abs() < 1.0 {
        return true;
    }
    let area = state.tiling_area();
    if let Some(mut tree) = state.tile_tree.take() {
        if tree.resize_target(&resize_win, dx, dy, drag_left, drag_top, area) {
            state.resize_start_ptr = Some((pos.x, pos.y));
            // Раскладка — раз в кадр через layout_dirty, а не на каждое
            // событие мыши (100–1000 Гц).
            state.layout_dirty = true;
            state.needs_render = true;
        }
        state.tile_tree = Some(tree);
    }
    true
}

/// WM сам ведёт указатель: ресайз тайла, камера, инструмент, перетаскивание.
fn wm_pointer_interaction(state: &AppState) -> bool {
    state.resize_window.is_some()
        || state.camera_pan_active
        || state.dragging
        || state.tool_active()
        || state.soft_grab_active()
}

/// Отдать клиентам фокус и положение указателя: `enter`/`leave` при смене
/// поверхности и `motion`. То же положение повторно не шлётся, а
/// поверхности с активным захватом (lock) `motion` не положен вовсе. Входя
/// на поверхность, где ждёт захват, указатель встаёт в точку, где курсор
/// видит клиент (cursor position hint), и захват включается.
///
/// `true` — клиенту ушли события (нужен `frame`).
fn send_motion(
    state: &mut AppState,
    focus: PointerFocus,
    location: Point<f64, Logical>,
    serial: Serial,
    time: u32,
) -> bool {
    let Some(ptr) = state.seat.get_pointer() else {
        return false;
    };
    let current = ptr.current_focus();
    let same_surface = current.as_ref() == focus.as_ref().map(|(s, _)| s);
    if same_surface
        && current
            .as_ref()
            .is_some_and(|s| constraints::is_locked(&ptr, s))
    {
        return false;
    }
    reset_cursor_over_desktop(state, focus.is_none());
    let activation = focus.as_ref().and_then(|(surface, origin)| {
        constraints::on_enter(state, &ptr, surface, *origin, location)
    });
    let location = activation
        .as_ref()
        .and_then(|a| a.place(state))
        .unwrap_or(location);
    let sent = !same_surface || ptr.current_location() != location;
    if sent {
        ptr.motion(
            state,
            focus.clone(),
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
    }
    state.pointer_focus = focus;
    if let Some(activation) = activation {
        activation.activate(&ptr);
    }
    sent
}

/// Пересчитать фокус указателя на месте: под курсором появилось или
/// пропало окно, сменился рабочий стол, окно ушло на весь экран.
pub fn refresh_pointer_focus(state: &mut AppState) {
    let Some(ptr) = state.seat.get_pointer() else {
        return;
    };
    // Указатель ведёт WM — фокус обновит следующее движение мыши.
    if wm_pointer_interaction(state) {
        return;
    }
    // Захваченный указатель остаётся у игры, пока её поверхность на экране:
    // панель или уведомление под стоящим курсором мышь не отнимают.
    let keep = ptr
        .current_focus()
        .is_some_and(|s| constraints::is_locked(&ptr, &s) && surface_on_screen(state, &s));
    if keep {
        return;
    }
    let pos = state.pointer_location;
    let (focus, location, _) = pointer_focus(state, pos.x, pos.y);
    let time = std::time::Duration::from(state.clock.now()).as_millis() as u32;
    if send_motion(state, focus, location, SERIAL_COUNTER.next_serial(), time) {
        ptr.frame(state);
    }
}

impl AppState {
    /// Пересчитать фокус указателя после текущей пачки запросов клиентов
    /// (см. [`Self::flush_pointer_refresh`]).
    pub fn queue_pointer_refresh(&mut self) {
        self.pointer_refresh_pending = true;
    }

    /// Отложенный пересчёт фокуса указателя. Вызывается главным циклом
    /// после обработки клиентов: к этому моменту новые окна уже закоммитили
    /// буфер, а X11-окна связаны со своими поверхностями.
    pub fn flush_pointer_refresh(&mut self) {
        if std::mem::take(&mut self.pointer_refresh_pending) {
            refresh_pointer_focus(self);
        }
    }
}

/// Поверхность (или её корень) — окно активного стола или слой на выходе.
fn surface_on_screen(state: &AppState, surface: &WlSurface) -> bool {
    let root = constraints::root_surface(surface);
    state.window_for_surface(&root).is_some()
        || state.space.outputs().any(|o| {
            layer_map_for_output(o)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
}

/// Над пустым столом — стандартный курсор: клиент после leave свой курсор
/// не сбрасывает, и над фоном оставался, например, I-beam терминала.
fn reset_cursor_over_desktop(state: &mut AppState, nothing_under: bool) {
    use smithay::input::pointer::CursorImageStatus;
    if nothing_under
        && !pointer_grabbed(state)
        && !matches!(&state.cursor_status, CursorImageStatus::Named(i) if *i == Default::default())
    {
        state.cursor_status = CursorImageStatus::default_named();
        state.needs_render = true;
    }
}

/// Фокус указателя в физическом режиме: поверхность и её начало в
/// «виртуальном» пространстве окна + положение указателя в нём же.
///
/// Клиент получает `location − origin` = координаты в своём плоском буфере
/// (обратное проецирование через поворот/масштаб/разрез). Пока кнопка
/// зажата, smithay держит фокус нажатия, и мы продолжаем считать координаты
/// относительно того же окна — выделение текста в повёрнутом окне не
/// перескакивает на соседние строки.
fn physics_pointer_focus(
    state: &mut AppState,
    x: f64,
    y: f64,
) -> (PointerFocus, Point<f64, Logical>) {
    // Что под курсором прямо сейчас: (поверхность, её начало, точка) в
    // виртуальном пространстве окна под курсором.
    let hover = state.physics_pointer_target(x, y).map(|(target, local)| {
        let point = local + target.window.geometry().loc.to_f64();
        let surface = state.physics_surface_under(&target.window, local);
        (surface, point)
    });
    if !state.forwarded_buttons.is_empty() {
        if let Some(target) = state.pointer_grab_target.clone() {
            if let Some(local) = state.physics_target_point(&target, x, y) {
                let location = local + target.window.geometry().loc.to_f64();
                // Неявный захват клика фокус игнорирует и считает от окна
                // нажатия; popup- и DnD-захваты берут фокус отсюда — начало
                // подбираем так, чтобы location − начало = точке в поверхности
                // под курсором.
                let focus = hover.and_then(|(surface, point)| {
                    surface.map(|(s, origin)| (s, location - (point - origin)))
                });
                return (focus, location);
            }
        }
    }
    match hover {
        Some((surface, point)) => (surface, point),
        None => (None, (x, y).into()),
    }
}

/// Что под указателем вне активных взаимодействий WM: (поверхность и её
/// начало, позиция для клиента, окно). Порядок: панели и лаунчеры
/// (layer-shell `overlay`/`top`), полноэкранное окно, окна режима, фоновые
/// слои (`bottom`/`background`).
fn pointer_focus(
    state: &mut AppState,
    x: f64,
    y: f64,
) -> (PointerFocus, Point<f64, Logical>, Option<Window>) {
    let pos: Point<f64, Logical> = (x, y).into();
    // Кнопка зажата над панелью — до отпускания всё идёт панели.
    if state.pointer_on_layer && !state.forwarded_buttons.is_empty() {
        let focus = state
            .layer_surface_under(pos, state.upper_layers())
            .map(|(focus, _)| focus);
        return (focus, pos, None);
    }
    if state.forwarded_buttons.is_empty() {
        if let Some((focus, _)) = state.layer_surface_under(pos, state.upper_layers()) {
            return (Some(focus), pos, None);
        }
    }
    let (focus, location, window) = if let Some(window) = state.fullscreen_window() {
        let render_loc = (Point::<i32, Logical>::default() - window.geometry().loc).to_f64();
        // X11-окно растянуто на весь экран ещё до первого буфера, а игра
        // может захватить мышь сразу после map: указатель — на нём, как
        // только окно связано со своей поверхностью.
        let focus = window
            .surface_under(pos - render_loc, WindowSurfaceType::ALL)
            .map(|(surface, loc)| (surface, render_loc + loc.to_f64()))
            .or_else(|| {
                let surface = window.x11_surface()?.wl_surface()?;
                Some((surface, render_loc))
            });
        (focus, pos, Some(window))
    } else if state.layout_mode == LayoutMode::Physics {
        let (focus, location) = physics_pointer_focus(state, x, y);
        let window = state.physics_pointer_target(x, y).map(|(t, _)| t.window);
        (focus, location, window)
    } else {
        match tiling_focus(state, pos) {
            Some((focus, window)) => (Some(focus), pos, Some(window)),
            None => (None, pos, None),
        }
    };
    if focus.is_none() && window.is_none() && state.forwarded_buttons.is_empty() {
        // Над пустым столом — фоновые слои (виджеты рабочего стола).
        if let Some((focus, _)) =
            state.layer_surface_under(pos, &[Layer::Bottom, Layer::Background])
        {
            return (Some(focus), pos, None);
        }
    }
    (focus, location, window)
}

/// Поверхность и окно под точкой в тайлинге (координаты Space = экранные).
/// Окно ловит указатель только внутри своего тайла (больший тайла клиент
/// обрезан по нему), попапы — где угодно.
fn tiling_focus(state: &AppState, pos: Point<f64, Logical>) -> Option<(FocusSurface, Window)> {
    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else {
            continue;
        };
        let render_loc = (loc - window.geometry().loc).to_f64();
        let is_or = window
            .x11_surface()
            .is_some_and(|x| x.is_override_redirect());
        let in_tile = is_or
            || state
                .tile_rect(window)
                .is_none_or(|r| r.to_f64().contains(pos));
        let kind = if in_tile {
            WindowSurfaceType::ALL
        } else {
            WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE
        };
        if let Some((surface, surface_loc)) = window.surface_under(pos - render_loc, kind) {
            if !in_tile && window.wl_surface().as_deref() == Some(&surface) {
                continue;
            }
            return Some(((surface, render_loc + surface_loc.to_f64()), window.clone()));
        }
    }
    None
}

pub(super) fn handle_pointer_axis(
    state: &mut AppState,
    horizontal: f64,
    vertical: f64,
    horizontal_discrete: Option<f64>,
    vertical_discrete: Option<f64>,
    source: AxisSource,
    time: u32,
) {
    // Super+колесо в физическом режиме — зум камеры к курсору, клиенту не шлём.
    if state.layout_mode == LayoutMode::Physics
        && state.fullscreen_window().is_none()
        && super_held(state)
        && vertical != 0.0
    {
        state.zoom_camera(if vertical < 0.0 { 1.1 } else { 1.0 / 1.1 });
        return;
    }

    let Some(ptr) = state.seat.get_pointer() else {
        return;
    };

    let mut frame = AxisFrame::new(time).source(source);

    if horizontal != 0.0 {
        frame = frame.value(Axis::Horizontal, horizontal);
        if let Some(discrete) = horizontal_discrete {
            frame = frame.v120(Axis::Horizontal, discrete as i32);
        }
    } else if source == AxisSource::Finger {
        frame = frame.stop(Axis::Horizontal);
    }

    if vertical != 0.0 {
        frame = frame.value(Axis::Vertical, vertical);
        if let Some(discrete) = vertical_discrete {
            frame = frame.v120(Axis::Vertical, discrete as i32);
        }
    } else if source == AxisSource::Finger {
        frame = frame.stop(Axis::Vertical);
    }

    ptr.axis(state, frame);
    ptr.frame(state);
}

/// Указатель захвачен (неявный захват клика, popup-grab, DnD).
fn pointer_grabbed(state: &AppState) -> bool {
    state
        .seat
        .get_pointer()
        .map(|p| p.is_grabbed())
        .unwrap_or(false)
}

/// Shared click-to-focus + Super+LMB tile resize (Tiling) / drag body (Physics)
///
/// Отпускание всегда идёт туда же, куда ушло нажатие: кнопка, нажатая для
/// клиента, отпускается у клиента (даже если в это время включили инструмент
/// или сменили режим), а нажатие, съеденное WM, не даёт клиенту «лишнего»
/// release без press.
pub(super) fn handle_pointer_button(
    state: &mut AppState,
    button: u32,
    btn_state: ButtonState,
    time_msec: u32,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let pos = state.pointer_location;
    let super_down = super_held(state);
    let pressed = btn_state == ButtonState::Pressed;

    if !pressed && state.forwarded_buttons.remove(&button) {
        forward_button(state, button, btn_state, serial, time_msec, false);
        if state.forwarded_buttons.is_empty() {
            state.pointer_grab_target = None;
            state.pointer_on_layer = false;
        }
        return;
    }

    if pressed {
        // Панель, лаунчер, уведомление (layer-shell) — клик им, без
        // инструментов и перетаскиваний WM.
        if state.forwarded_buttons.is_empty() || state.pointer_on_layer {
            if let Some((_, layer)) = state.layer_surface_under(pos, state.upper_layers()) {
                if state.forwarded_buttons.is_empty() {
                    state.pointer_on_layer = true;
                    state.pointer_grab_target = None;
                }
                if layer.can_receive_keyboard_focus() {
                    state.focus_layer(&layer);
                }
                forward_button(state, button, btn_state, serial, time_msec, false);
                state.forwarded_buttons.insert(button);
                return;
            }
        }
        // Полноэкранное окно (игра) получает все кнопки как есть.
        if state.fullscreen_window().is_some() {
            forward_button(state, button, btn_state, serial, time_msec, true);
            state.forwarded_buttons.insert(button);
            return;
        }
    }

    if state.layout_mode == LayoutMode::Physics {
        // Средняя кнопка панорамирует камеру.
        if button == BTN_MIDDLE {
            if pressed {
                state.physics_camera_pan_begin(pos.x, pos.y);
            } else {
                state.physics_camera_pan_end();
            }
            state.needs_render = true;
            return;
        }
        // Активный инструмент забирает клики себе.
        if state.tool_button(button, pressed, pos.x, pos.y) {
            return;
        }
        if !pressed {
            // Отпускание кнопки, нажатие которой съел WM (drag, захват края).
            state.physics_drag_end();
            state.soft_grab_end();
            return;
        }
        if button == BTN_LEFT && super_down {
            let shift = state
                .seat
                .get_keyboard()
                .map(|k| k.modifier_state().shift)
                .unwrap_or(false);
            // Super+Shift+ЛКМ — тянуть окно за край (мягкое тело);
            // Super+ЛКМ — схватить и тащить окно.
            let grabbed = if shift {
                state.soft_grab_begin(pos.x, pos.y)
            } else {
                state.physics_drag_begin(pos.x, pos.y)
            };
            if let Some(win) = grabbed {
                state.focus_window(&win);
                state.needs_render = true;
                return;
            }
        }
        if state.forwarded_buttons.is_empty() {
            state.pointer_grab_target = state.physics_pointer_target(pos.x, pos.y).map(|(t, _)| t);
        }
        forward_button(state, button, btn_state, serial, time_msec, true);
        state.forwarded_buttons.insert(button);
        return;
    }

    if !pressed {
        // End resize on LMB release even if Super was released first.
        if button == BTN_LEFT && state.resize_window.is_some() {
            state.end_resize();
        }
        return;
    }

    // Super + Left Mouse Button → start tile resize (TILING ONLY)
    if button == BTN_LEFT && super_down {
        if let Some((_, win)) = tiling_focus(state, pos) {
            let geo = state
                .tile_rect(&win)
                .or_else(|| state.space.element_geometry(&win))
                .unwrap_or_default();
            // Квадрант клика решает, какие края двигать.
            let rel_x = (pos.x - geo.loc.x as f64) / geo.size.w.max(1) as f64;
            let rel_y = (pos.y - geo.loc.y as f64) / geo.size.h.max(1) as f64;
            state.resize_window = Some(win);
            state.resize_start_ptr = Some((pos.x, pos.y));
            state.resize_edges = Some((rel_x < 0.5, rel_y < 0.5));
        }
        return;
    }

    // While resizing, ignore other pointer button events
    if state.resize_window.is_some() {
        return;
    }

    forward_button(state, button, btn_state, serial, time_msec, true);
    state.forwarded_buttons.insert(button);
}

/// Отдаёт кнопку клиенту: фокус и положение (см. [`send_motion`]), фокус по
/// клику (`focus_on_press`), button и frame.
fn forward_button(
    state: &mut AppState,
    button: u32,
    btn_state: ButtonState,
    serial: Serial,
    time: u32,
    focus_on_press: bool,
) {
    let pos = state.pointer_location;
    let (focus, location, window) = pointer_focus(state, pos.x, pos.y);
    send_motion(state, focus, location, serial, time);
    if focus_on_press && btn_state == ButtonState::Pressed {
        click_focus(state, window);
    }
    send_button(state, button, btn_state, serial, time);
}

/// Фокус по клику (без лишних переключений для X11-попапов).
fn click_focus(state: &mut AppState, window: Option<Window>) {
    match window {
        Some(window) => {
            // Не дёргаем фокус на каждом клике: меню Steam/CEF — locally-active
            // X11-окна, лишний переход фокуса перед ButtonPress отправляет
            // клик в окно под меню.
            if !window
                .x11_surface()
                .map_or(false, |x| x.is_override_redirect())
                && state.get_focused_window().as_ref() != Some(&window)
            {
                state.focus_window(&window);
            }
        }
        None => {
            // Клик в пустоту снимает фокус (кроме активного popup-grab).
            let grabbed = state
                .seat
                .get_keyboard()
                .map(|k| k.is_grabbed())
                .unwrap_or(false);
            if !grabbed {
                let serial = SERIAL_COUNTER.next_serial();
                if let Some(kbd) = state.seat.get_keyboard() {
                    kbd.set_focus(state, None, serial);
                    state.needs_render = true;
                }
            }
        }
    }
}

fn send_button(
    state: &mut AppState,
    button: u32,
    btn_state: ButtonState,
    serial: Serial,
    time: u32,
) {
    if let Some(ptr) = state.seat.get_pointer() {
        ptr.button(
            state,
            &ButtonEvent {
                button,
                state: btn_state,
                serial,
                time,
            },
        );
        // wl_pointer.frame завершает группу событий (motion + button) —
        // строгие frame-клиенты (Chromium/CEF, Steam) иначе копят события и
        // превращают клик в выделение.
        ptr.frame(state);
        state.needs_render = true;
    }
}
