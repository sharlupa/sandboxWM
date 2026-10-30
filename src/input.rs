use crate::state::AppState;
use crate::state::LayoutMode;
use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
            KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        libinput::LibinputInputBackend,
        session::Session,
        winit::WinitInput,
    },
    input::{
        keyboard::{FilterResult, keysyms as xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint},
    wayland::seat::WaylandFocus,
    wayland::shell::wlr_layer::Layer,
};

// BTN_LEFT = 272 (0x110)
const BTN_LEFT: u32 = 272;
const BTN_MIDDLE: u32 = 274;

/// Клавиатура — общий путь для winit и libinput.
fn handle_keyboard<B: InputBackend>(state: &mut AppState, event: B::KeyboardKeyEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let keycode = event.key_code();
    let key_state = event.state();

    let Some(kbd) = state.seat.get_keyboard() else {
        return;
    };
    kbd.input(
        state,
        keycode,
        key_state,
        serial,
        time,
        |app_state, modifiers, keysym| {
            let modified = u32::from(keysym.modified_sym());
            // Латинская клавиша без учёта Shift и раскладки: Super+Shift+3
            // совпадает с биндом «3», а Super+W работает и в русской раскладке.
            let base = keysym
                .raw_latin_sym_or_raw_current_sym()
                .map(u32::from)
                .unwrap_or(modified);
            let evdev = keysym.raw_code().raw().saturating_sub(8);
            handle_key(app_state, modifiers, modified, base, evdev, key_state)
        },
    );
}

pub fn process_input_event(state: &mut AppState, event: InputEvent<WinitInput>) {
    match event {
        InputEvent::Keyboard { event } => handle_keyboard::<WinitInput>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            // Хост даёт только абсолютные координаты — относительное движение
            // для игр считается разницей с прошлой позицией.
            // Считаем от прошлой позиции курсора хоста: под захватом наш
            // курсор стоит на месте, и разница с ним накапливалась бы.
            let size = state.output_size();
            let pos = event.position_transformed(size);
            let delta = pos - state.host_pointer_location.unwrap_or(pos);
            state.host_pointer_location = Some(pos);
            pointer_moved(state, pos, delta, delta, event.time(), event.time_msec());
        }
        InputEvent::PointerAxis { event } => {
            handle_pointer_axis(
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
            handle_pointer_button(state, event.button_code(), event.state(), event.time_msec());
        }
        _ => {}
    }
}

/// Движение мыши: относительное событие (игры: обзор мышью через
/// zwp_relative_pointer, у XWayland — XI2 raw motion) и, если указатель
/// не захвачен, обычное перемещение курсора. Захват (lock) держит курсор на
/// месте, confine не выпускает его из окна.
fn pointer_moved(
    state: &mut AppState,
    target: Point<f64, Logical>,
    delta: Point<f64, Logical>,
    delta_unaccel: Point<f64, Logical>,
    utime: u64,
    time_msec: u32,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let (mut x, mut y) = (target.x, target.y);
    if let Some(ptr) = state.seat.get_pointer() {
        let focus = ptr.current_focus();
        if delta != Point::default() || delta_unaccel != Point::default() {
            ptr.relative_motion(
                state,
                focus.clone().map(|s| (s, Point::default())),
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel,
                    utime,
                },
            );
        }
        match focus.as_ref().and_then(|s| active_constraint(&ptr, s)) {
            Some(Constraint::Locked) => {
                ptr.frame(state);
                return;
            }
            Some(Constraint::Confined) => {
                if let Some(rect) = focus.as_ref().and_then(|s| confine_rect(state, s)) {
                    x = x.clamp(rect.loc.x, rect.loc.x + rect.size.w - 1.0);
                    y = y.clamp(rect.loc.y, rect.loc.y + rect.size.h - 1.0);
                }
            }
            None => {}
        }
    }
    handle_pointer_motion(state, x, y, serial, time_msec);
}

/// Указатель сейчас захвачен (pointer lock) поверхностью под ним.
fn pointer_locked(state: &AppState) -> bool {
    let Some(ptr) = state.seat.get_pointer() else {
        return false;
    };
    ptr.current_focus()
        .is_some_and(|s| matches!(active_constraint(&ptr, &s), Some(Constraint::Locked)))
}

/// Handler for libinput events (TTY/DRM backend)
pub fn process_libinput_event(state: &mut AppState, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::Keyboard { event } => handle_keyboard::<LibinputInputBackend>(state, event),
        InputEvent::PointerMotion { event } => {
            let target = state.pointer_location + event.delta();
            pointer_moved(
                state,
                target,
                event.delta(),
                event.delta_unaccel(),
                event.time(),
                event.time_msec(),
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            // Планшеты / тачпады в абсолютном режиме / VM-указатели.
            let size = state.output_size();
            let pos = event.position_transformed(size);
            let delta = pos - state.host_pointer_location.unwrap_or(pos);
            state.host_pointer_location = Some(pos);
            pointer_moved(state, pos, delta, delta, event.time(), event.time_msec());
        }
        InputEvent::PointerAxis { event } => {
            handle_pointer_axis(
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
            handle_pointer_button(state, event.button_code(), event.state(), event.time_msec());
        }
        _ => {}
    }
}

/// Активный захват указателя на поверхности.
enum Constraint {
    Locked,
    Confined,
}

fn active_constraint(ptr: &PointerHandle<AppState>, surface: &WlSurface) -> Option<Constraint> {
    with_pointer_constraint(surface, ptr, |constraint| match constraint {
        Some(c) if c.is_active() => Some(match &*c {
            PointerConstraint::Locked(_) => Constraint::Locked,
            PointerConstraint::Confined(_) => Constraint::Confined,
        }),
        _ => None,
    })
}

/// Экранный прямоугольник окна, в котором удерживается курсор (confine).
fn confine_rect(
    state: &AppState,
    surface: &WlSurface,
) -> Option<smithay::utils::Rectangle<f64, Logical>> {
    let origin = state.surface_screen_origin(surface)?;
    let window = state.any_window_for_surface(surface)?;
    let geo = window.geometry();
    Some(smithay::utils::Rectangle::new(
        origin + geo.loc.to_f64(),
        geo.size.to_f64(),
    ))
}

/// Корень дерева поверхностей (подповерхность → её окно).
fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
        root = parent;
    }
    root
}

/// Указатель вошёл на поверхность с захватом — включаем его, но только у
/// окна с клавиатурным фокусом (иначе мышь «залипла» бы при наведении).
fn maybe_activate_constraint(
    state: &AppState,
    ptr: &PointerHandle<AppState>,
    focus: Option<&(WlSurface, Point<f64, Logical>)>,
    location: Point<f64, Logical>,
) {
    let Some((surface, origin)) = focus else {
        return;
    };
    let keyboard_focus = state
        .seat
        .get_keyboard()
        .and_then(|k| k.current_focus())
        .and_then(|f| f.surface());
    if keyboard_focus.as_ref() != Some(&root_surface(surface)) {
        return;
    }
    with_pointer_constraint(surface, ptr, |constraint| {
        if let Some(c) = constraint {
            if !c.is_active() {
                let point = (location - *origin).to_i32_round();
                if c.region().is_none_or(|r| r.contains(point)) {
                    c.activate();
                }
            }
        }
    });
}

/// Пересчитать фокус указателя на месте (сменился рабочий стол, окно ушло
/// на весь экран и т.п.).
pub fn refresh_pointer_focus(state: &mut AppState) {
    let pos = state.pointer_location;
    let time = std::time::Duration::from(state.clock.now()).as_millis() as u32;
    handle_pointer_motion(state, pos.x, pos.y, SERIAL_COUNTER.next_serial(), time);
}

/// Номер рабочего стола по физической клавише верхнего ряда:
/// 1…9 → 0…8, 0 → 9.
fn workspace_from_evdev(evdev: u32) -> Option<usize> {
    match evdev {
        2..=10 => Some((evdev - 2) as usize),
        11 => Some(9),
        _ => None,
    }
}

fn handle_pointer_axis(
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

fn super_held(state: &AppState) -> bool {
    state
        .seat
        .get_keyboard()
        .map(|k| k.modifier_state().logo)
        .unwrap_or(false)
}

/// Указатель захвачен (неявный захват клика, popup-grab, DnD).
fn pointer_grabbed(state: &AppState) -> bool {
    state
        .seat
        .get_pointer()
        .map(|p| p.is_grabbed())
        .unwrap_or(false)
}

/// Centralized keyboard shortcut handler (shared between winit & libinput).
/// `keysym` — с учётом модификаторов, `base` — латинская клавиша без Shift.
fn handle_key(
    app_state: &mut AppState,
    modifiers: &smithay::input::keyboard::ModifiersState,
    keysym: u32,
    base: u32,
    evdev: u32,
    key_state: KeyState,
) -> FilterResult<()> {
    // Ctrl+Alt+F1..F12 → switch virtual terminal (TTY / libseat mode).
    // On standard Linux XKB keymaps, Ctrl+Alt+Fn produces XKB_KEY_XF86Switch_VT_N
    // (not KEY_Fn). We handle both cases.
    if key_state == KeyState::Pressed {
        let vt_from_xf86 = (0x1008_FE01..=0x1008_FE0C)
            .contains(&keysym)
            .then(|| (keysym - 0x1008_FE00) as i32);
        let vt_from_fn = if modifiers.ctrl && modifiers.alt && !modifiers.logo {
            vt_number_from_keysym(keysym)
        } else {
            None
        };
        if let Some(vt) = vt_from_xf86.or(vt_from_fn) {
            if let Some(mut session) = app_state.session.clone() {
                match session.change_vt(vt) {
                    Ok(_) => {
                        println!("[VT] переключение на VT{vt} успешно");
                        return FilterResult::Intercept(());
                    }
                    Err(e) => {
                        eprintln!("[VT] ошибка переключения на VT{vt}: {e:?}");
                    }
                }
            }
        }
    }

    // Клавиши внутри активного инструмента (материал 1–4, Delete, Esc) —
    // без Super, чтобы рисовать было удобно. Только в физике: в тайлинге
    // инструментов нет, и клавиши принадлежат клиенту.
    if key_state == KeyState::Pressed
        && !modifiers.logo
        && app_state.layout_mode == LayoutMode::Physics
        && app_state.tool_active()
        && app_state.tool_key(keysym, evdev)
    {
        return FilterResult::Intercept(());
    }

    // Super+цифра — рабочий стол, Super+Shift+цифра — перенести туда окно.
    // По физической клавише: работает в любой раскладке и с Shift.
    if key_state == KeyState::Pressed && modifiers.logo {
        if let Some(idx) = workspace_from_evdev(evdev) {
            if idx < app_state.workspaces.len() {
                if modifiers.shift {
                    app_state.move_focused_to_workspace(idx);
                } else {
                    app_state.switch_workspace(idx);
                }
                return FilterResult::Intercept(());
            }
        }
    }

    // Бинд совпадает по символу с модификаторами, по латинскому символу
    // без них (кириллица) или — для цифр — по физической клавише
    // (AZERTY: «1» на первом уровне даёт `&`).
    let mk = |name: &str| {
        use crate::config::ControlsConfig as C;
        C::matches_key(name, keysym)
            || C::matches_key(name, base)
            || C::matches_digit_key(name, evdev)
    };

    // Камера и вращение не трогают полноэкранное окно (игра).
    if app_state.layout_mode == LayoutMode::Physics && app_state.fullscreen_window().is_none() {
        if modifiers.logo && key_state == KeyState::Pressed {
            if base == xkb::KEY_minus {
                app_state.zoom_camera(1.0 / 1.1);
                return FilterResult::Intercept(());
            }
            if base == xkb::KEY_equal {
                app_state.zoom_camera(1.1);
                return FilterResult::Intercept(());
            }
            if base == xkb::KEY_0 {
                app_state.reset_camera();
                return FilterResult::Intercept(());
            }
        }

        // Spin hold/release: клавиши из controls.spin_left / spin_right.
        let ctrl = &app_state.config.controls;
        let is_left = mk(&ctrl.spin_left);
        let is_right = mk(&ctrl.spin_right);
        if is_left || is_right {
            if key_state == KeyState::Pressed && modifiers.logo {
                app_state.physics_spin_hold(if is_left { -1.0 } else { 1.0 });
                return FilterResult::Intercept(());
            }
            if key_state == KeyState::Released && app_state.spin_dir != 0.0 {
                let holding_left = app_state.spin_dir < 0.0;
                let holding_right = app_state.spin_dir > 0.0;
                if (is_left && holding_left) || (is_right && holding_right) {
                    app_state.physics_spin_release();
                    return FilterResult::Intercept(());
                }
            }
        }
    }

    if key_state != KeyState::Pressed || !modifiers.logo {
        return FilterResult::Forward;
    }

    let ctrl = app_state.config.controls.clone();
    let step = app_state.config.physics.camera_step;

    if mk(&ctrl.spawn_terminal) {
        let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        let mut parts = ctrl.terminal.split_whitespace();
        if let Some(cmd) = parts.next() {
            let mut c = std::process::Command::new(cmd);
            c.args(parts)
                // DISPLAY не вырезаем: нативные модули Electron-приложений
                // (discord_utils и т.п.) используют X11 и падают без него.
                .env("WAYLAND_DISPLAY", &wayland_display);
            if let Err(e) = c.spawn() {
                eprintln!("[input] не удалось запустить терминал {cmd:?}: {e}");
            }
        }
        return FilterResult::Intercept(());
    }

    if ctrl.quit.iter().any(|k| mk(k)) {
        app_state.running = false;
        return FilterResult::Intercept(());
    }

    type Action = fn(&mut AppState);
    let actions: [(&str, Action); 17] = [
        (&ctrl.fullscreen, AppState::toggle_fullscreen_focused),
        (&ctrl.toggle_physics, AppState::toggle_physics),
        // Режимы гравитации (концепт §1).
        (&ctrl.gravity_normal, AppState::gravity_normal),
        (&ctrl.gravity_tiling, AppState::gravity_tiling),
        (&ctrl.float_window, AppState::toggle_float_focused),
        (&ctrl.gravity_zero, AppState::toggle_zero_gravity),
        // Инструменты и окна.
        (&ctrl.draw_tool, AppState::toggle_draw_tool),
        (&ctrl.knife_tool, AppState::toggle_knife_tool),
        (&ctrl.weld_windows, |s| {
            s.start_link_tool(crate::physics::LinkKind::Weld)
        }),
        (&ctrl.spring_windows, |s| {
            s.start_link_tool(crate::physics::LinkKind::Spring)
        }),
        (&ctrl.rope_windows, |s| {
            s.start_link_tool(crate::physics::LinkKind::Rope)
        }),
        (&ctrl.unlink_window, AppState::unlink_focused),
        (&ctrl.toggle_soft, AppState::toggle_soft_focused),
        (&ctrl.restore_window, AppState::restore_focused_window),
        (&ctrl.gather_windows, AppState::gather_windows),
        (&ctrl.save_session, AppState::save_session),
        (&ctrl.close_window, close_focused),
    ];
    for (name, action) in actions {
        if mk(name) {
            action(app_state);
            return FilterResult::Intercept(());
        }
    }

    let arrows = [
        (&ctrl.camera_right, (1.0, 0.0)),
        (&ctrl.camera_left, (-1.0, 0.0)),
        (&ctrl.camera_up, (0.0, -1.0)),
        (&ctrl.camera_down, (0.0, 1.0)),
    ];
    for (name, (dx, dy)) in arrows {
        if mk(name) {
            if app_state.layout_mode == LayoutMode::Physics {
                if app_state.fullscreen_window().is_none() {
                    app_state.move_camera(dx * step, dy * step);
                }
            } else {
                app_state.focus_direction(dx, dy);
            }
            return FilterResult::Intercept(());
        }
    }

    FilterResult::Forward
}

fn close_focused(state: &mut AppState) {
    if let Some(win) = state.get_focused_window() {
        if let Some(toplevel) = win.toplevel() {
            toplevel.send_close();
        } else if let Some(x11) = win.x11_surface() {
            let _ = x11.close();
        }
    }
}

fn vt_number_from_keysym(keysym: u32) -> Option<i32> {
    (xkb::KEY_F1..=xkb::KEY_F12)
        .contains(&keysym)
        .then(|| (keysym - xkb::KEY_F1 + 1) as i32)
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
) -> (
    Option<(WlSurface, Point<f64, Logical>)>,
    Point<f64, Logical>,
) {
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
) -> (
    Option<(WlSurface, Point<f64, Logical>)>,
    Point<f64, Logical>,
    Option<smithay::desktop::Window>,
) {
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
        let focus = window
            .surface_under(pos - render_loc, smithay::desktop::WindowSurfaceType::ALL)
            .map(|(surface, loc)| (surface, render_loc + loc.to_f64()));
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

/// Update pointer position and find the window under cursor
fn handle_pointer_motion(state: &mut AppState, mut x: f64, mut y: f64, serial: Serial, time: u32) {
    // Clamp to screen bounds
    let screen = state.output_size();
    x = x.clamp(0.0, (screen.w - 1) as f64);
    y = y.clamp(0.0, (screen.h - 1) as f64);
    let pos: Point<f64, Logical> = (x, y).into();
    state.pointer_location = pos;
    // DRM: программный курсор → нужен кадр. Winit: курсор рисует хост.
    if state.session.is_some() {
        state.needs_render = true;
    }

    let (focus, location) = if state.fullscreen_window().is_some() {
        let (focus, location, _) = pointer_focus(state, x, y);
        (focus, location)
    } else if state.layout_mode == LayoutMode::Physics {
        // Удержание средней кнопки двигает камеру как схваченный холст.
        if state.physics_camera_pan_update(x, y) {
            return;
        }
        state.tool_motion(x, y);
        state.soft_grab_update(x, y);
        if state.dragging {
            state.needs_render = true;
        }
        if state.dragging || state.tool_active() || state.soft_grab_active() {
            // Пока тащим окно или работаем инструментом, ховер клиентам не нужен.
            (None, pos)
        } else {
            let (focus, location, _) = pointer_focus(state, x, y);
            (focus, location)
        }
    } else {
        // --- Super+LMB tile resize ---
        if let (Some(resize_win), Some((start_x, start_y)), Some((drag_left, drag_top))) = (
            state.resize_window.clone(),
            state.resize_start_ptr,
            state.resize_edges,
        ) {
            let dx = x - start_x;
            let dy = y - start_y;
            if dx.abs() < 1.0 && dy.abs() < 1.0 {
                return;
            }
            let area = state.tiling_area();
            if let Some(mut tree) = state.tile_tree.take() {
                if tree.resize_target(&resize_win, dx, dy, drag_left, drag_top, area) {
                    state.resize_start_ptr = Some((x, y));
                    // Раскладка — раз в кадр через layout_dirty, а не на каждое
                    // событие мыши (100–1000 Гц).
                    state.layout_dirty = true;
                    state.needs_render = true;
                }
                state.tile_tree = Some(tree);
            }
            return;
        }
        let (focus, location, _) = pointer_focus(state, x, y);
        (focus, location)
    };

    reset_cursor_over_desktop(state, focus.is_none());
    if let Some(ptr) = state.seat.get_pointer() {
        ptr.motion(
            state,
            focus.clone(),
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        ptr.frame(state);
        maybe_activate_constraint(state, &ptr, focus.as_ref(), location);
    }
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

/// Поверхность и окно под точкой в тайлинге (координаты Space = экранные).
/// Окно ловит указатель только внутри своего тайла (больший тайла клиент
/// обрезан по нему), попапы — где угодно.
fn tiling_focus(
    state: &AppState,
    pos: Point<f64, Logical>,
) -> Option<((WlSurface, Point<f64, Logical>), smithay::desktop::Window)> {
    use smithay::desktop::WindowSurfaceType;
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

/// Shared click-to-focus + Super+LMB tile resize (Tiling) / drag body (Physics)
///
/// Отпускание всегда идёт туда же, куда ушло нажатие: кнопка, нажатая для
/// клиента, отпускается у клиента (даже если в это время включили инструмент
/// или сменили режим), а нажатие, съеденное WM, не даёт клиенту «лишнего»
/// release без press.
fn handle_pointer_button(
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

/// Отдаёт кнопку клиенту: motion с актуальным фокусом, фокус по клику
/// (`focus_on_press`), button и frame.
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
    // Под захватом (обзор мышью в игре) абсолютных движений нет: лишний
    // wl_pointer.motion сбивал бы XWayland эмулируемую позицию курсора.
    let locked = pointer_locked(state);
    if let Some(ptr) = state.seat.get_pointer().filter(|_| !locked) {
        ptr.motion(
            state,
            focus,
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
    }
    if focus_on_press && btn_state == ButtonState::Pressed {
        click_focus(state, window);
    }
    send_button(state, button, btn_state, serial, time);
}

/// Фокус по клику (без лишних переключений для X11-попапов).
fn click_focus(state: &mut AppState, window: Option<smithay::desktop::Window>) {
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
