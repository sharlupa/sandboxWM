//! Клавиатура: клавиши клиентам и горячие клавиши WM (Super+…).

use smithay::backend::input::{Event, InputBackend, KeyState, KeyboardKeyEvent};
use smithay::backend::session::Session;
use smithay::input::keyboard::{FilterResult, keysyms as xkb};
use smithay::utils::SERIAL_COUNTER;

use crate::state::{AppState, LayoutMode};

/// Клавиатура — общий путь для winit и libinput.
pub(super) fn handle_keyboard<B: InputBackend>(state: &mut AppState, event: B::KeyboardKeyEvent) {
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

/// Номер рабочего стола по физической клавише верхнего ряда:
/// 1…9 → 0…8, 0 → 9.
fn workspace_from_evdev(evdev: u32) -> Option<usize> {
    match evdev {
        2..=10 => Some((evdev - 2) as usize),
        11 => Some(9),
        _ => None,
    }
}

/// Зажат Super.
pub(super) fn super_held(state: &AppState) -> bool {
    state
        .seat
        .get_keyboard()
        .map(|k| k.modifier_state().logo)
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
