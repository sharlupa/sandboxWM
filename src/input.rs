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
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
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
            handle_key(app_state, modifiers, modified, base, key_state)
        },
    );
}

pub fn process_input_event(state: &mut AppState, event: InputEvent<WinitInput>) {
    match event {
        InputEvent::Keyboard { event } => handle_keyboard::<WinitInput>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let size = state.output_size();
            let pos = event.position_transformed(size);
            handle_pointer_motion(state, pos.x, pos.y, serial, event.time_msec());
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

/// Handler for libinput events (TTY/DRM backend)
pub fn process_libinput_event(state: &mut AppState, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::Keyboard { event } => handle_keyboard::<LibinputInputBackend>(state, event),
        InputEvent::PointerMotion { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let cur = state.pointer_location;
            handle_pointer_motion(
                state,
                cur.x + event.delta_x(),
                cur.y + event.delta_y(),
                serial,
                event.time_msec(),
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            // Планшеты / тачпады в абсолютном режиме / VM-указатели.
            let serial = SERIAL_COUNTER.next_serial();
            let size = state.output_size();
            let pos = event.position_transformed(size);
            handle_pointer_motion(state, pos.x, pos.y, serial, event.time_msec());
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
    if state.layout_mode == LayoutMode::Physics && super_held(state) && vertical != 0.0 {
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
    // без Super, чтобы рисовать было удобно.
    if key_state == KeyState::Pressed
        && !modifiers.logo
        && app_state.tool_active()
        && app_state.tool_key(keysym)
    {
        return FilterResult::Intercept(());
    }

    let mk = |name: &str| crate::config::ControlsConfig::matches_key(name, base);

    if app_state.layout_mode == LayoutMode::Physics {
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
    let actions: [(&str, Action); 15] = [
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
        (&ctrl.camera_right, (step, 0.0), true),
        (&ctrl.camera_left, (-step, 0.0), false),
        (&ctrl.camera_up, (0.0, -step), false),
        (&ctrl.camera_down, (0.0, step), true),
    ];
    for (name, (dx, dy), forward) in arrows {
        if mk(name) {
            if app_state.layout_mode == LayoutMode::Physics {
                app_state.move_camera(dx, dy);
            } else {
                app_state.focus_next(forward);
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
) -> (Option<(WlSurface, Point<f64, Logical>)>, Point<f64, Logical>) {
    if pointer_grabbed(state) {
        if let Some(target) = state.pointer_grab_target.clone() {
            if let Some(local) = state.physics_target_point(&target, x, y) {
                let geo = target.window.geometry().loc.to_f64();
                return (None, local + geo);
            }
        }
    }
    match state.physics_pointer_target(x, y) {
        Some((target, local)) => {
            let geo = target.window.geometry().loc.to_f64();
            let focus = state.physics_surface_under(&target.window, local);
            (focus, local + geo)
        }
        None => (None, (x, y).into()),
    }
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

    if state.layout_mode == LayoutMode::Physics {
        // Удержание средней кнопки двигает камеру как схваченный холст.
        if state.physics_camera_pan_update(x, y) {
            return;
        }
        state.tool_motion(x, y);
        state.soft_grab_update(x, y);
        let (focus, location) =
            if state.dragging || state.tool_active() || state.soft_grab_active() {
                // Пока тащим окно или работаем инструментом, ховер клиентам не нужен.
                (None, pos)
            } else {
                physics_pointer_focus(state, x, y)
            };
        if state.dragging {
            state.needs_render = true;
        }
        if let Some(ptr) = state.seat.get_pointer() {
            ptr.motion(
                state,
                focus,
                &MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            ptr.frame(state);
        }
        return;
    }

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

    let under = tiling_focus(state, pos);
    if let Some(ptr) = state.seat.get_pointer() {
        ptr.motion(
            state,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        ptr.frame(state);
    }
}

/// Поверхность под точкой в тайлинге (координаты Space = экранные).
fn tiling_focus(
    state: &AppState,
    pos: Point<f64, Logical>,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    let (w, loc) = state
        .space
        .element_under(pos)
        .map(|(w, loc)| (w.clone(), loc))?;
    let loc_f64 = loc.to_f64();
    w.surface_under(pos - loc_f64, smithay::desktop::WindowSurfaceType::ALL)
        .map(|(surf, surf_loc)| (surf, loc_f64 + surf_loc.to_f64()))
}

/// Shared click-to-focus + Super+LMB tile resize (Tiling) / drag body (Physics)
fn handle_pointer_button(state: &mut AppState, button: u32, btn_state: ButtonState, time_msec: u32) {
    let serial = SERIAL_COUNTER.next_serial();
    let pos = state.pointer_location;
    let super_down = super_held(state);
    let pressed = btn_state == ButtonState::Pressed;

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
        if button == BTN_LEFT {
            if pressed && super_down {
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
            } else if !pressed && (state.dragging || state.soft_grab_active()) {
                state.physics_drag_end();
                state.soft_grab_end();
                return;
            }
        }

        let (focus, location) = physics_pointer_focus(state, pos.x, pos.y);
        if let Some(ptr) = state.seat.get_pointer() {
            ptr.motion(
                state,
                focus,
                &MotionEvent {
                    location,
                    serial,
                    time: time_msec,
                },
            );
        }
        if pressed {
            let target = state.physics_pointer_target(pos.x, pos.y).map(|(t, _)| t);
            if !pointer_grabbed(state) {
                state.pointer_grab_target = target.clone();
            }
            click_focus(state, target.map(|t| t.window));
        }
        send_button(state, button, btn_state, serial, time_msec);
        if !pressed && !pointer_grabbed(state) {
            state.pointer_grab_target = None;
        }
        return;
    }

    // End resize on LMB release even if Super was released first
    if button == BTN_LEFT && !pressed && state.resize_window.is_some() {
        state.end_resize();
        return;
    }

    // Super + Left Mouse Button → start tile resize (TILING ONLY)
    if button == BTN_LEFT && super_down && pressed {
        if let Some(win) = state.space.element_under(pos).map(|(w, _)| w.clone()) {
            let geo = state.space.element_geometry(&win).unwrap_or_default();
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

    let under = tiling_focus(state, pos);
    if let Some(ptr) = state.seat.get_pointer() {
        ptr.motion(
            state,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time: time_msec,
            },
        );
    }
    if pressed {
        let window = state.space.element_under(pos).map(|(w, _)| w.clone());
        click_focus(state, window);
    }
    send_button(state, button, btn_state, serial, time_msec);
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

fn send_button(state: &mut AppState, button: u32, btn_state: ButtonState, serial: Serial, time: u32) {
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
