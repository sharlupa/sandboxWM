use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Event, InputEvent, KeyboardKeyEvent,
            PointerButtonEvent, PointerMotionEvent, ButtonState, KeyState,
        },
        session::Session,
        winit::WinitInput,
        libinput::LibinputInputBackend,
    },
    input::{
        keyboard::{FilterResult, keysyms as xkb},
        pointer::{ButtonEvent, MotionEvent},
    },
    utils::SERIAL_COUNTER,
};
use crate::state::AppState;
use crate::state::LayoutMode;


// BTN_LEFT = 272 (0x110)
const BTN_LEFT: u32 = 272;

pub fn process_input_event(state: &mut AppState, event: InputEvent<WinitInput>) {
    match event {
        InputEvent::Keyboard { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let keycode = event.key_code();
            let key_state = event.state();

            let Some(kbd) = state.seat.get_keyboard() else { return };
            kbd.input(
                state,
                keycode,
                key_state,
                serial,
                time,
                |app_state, modifiers, keysym| {
                    if key_state == KeyState::Pressed {
                        handle_key(app_state, modifiers, u32::from(keysym.modified_sym()))
                    } else {
                        FilterResult::Forward
                    }
                },
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            // No output mapped yet (hotplug / early init) — ignore the event
            // instead of panicking on the missing mode.
            let Some(out) = state.space.outputs().next() else { return };
            let Some(mode) = out.current_mode() else { return };
            let logical_size = smithay::utils::Size::<i32, smithay::utils::Logical>::from((mode.size.w, mode.size.h));
            let pos = event.position_transformed(logical_size);

            handle_pointer_motion(state, pos.x, pos.y, serial, event.time_msec());
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
        InputEvent::Keyboard { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let keycode = event.key_code();
            let key_state = event.state();

            let Some(kbd) = state.seat.get_keyboard() else { return };
            kbd.input(
                state,
                keycode,
                key_state,
                serial,
                time,
                |app_state, modifiers, keysym| {
                    if key_state == KeyState::Pressed {
                        handle_key(app_state, modifiers, u32::from(keysym.modified_sym()))
                    } else {
                        FilterResult::Forward
                    }
                },
            );
        }
        InputEvent::PointerMotion { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let dx = event.delta_x();
            let dy = event.delta_y();
            let cur = state.pointer_location;
            let new_pos = (cur.x + dx, cur.y + dy);

            handle_pointer_motion(state, new_pos.0, new_pos.1, serial, event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            handle_pointer_button(state, event.button_code(), event.state(), event.time_msec());
        }
        _ => {}
    }
}

/// Centralized keyboard shortcut handler (shared between winit & libinput)
fn handle_key(
    app_state: &mut AppState,
    modifiers: &smithay::input::keyboard::ModifiersState,
    keysym: u32,
) -> FilterResult<()> {
    // Ctrl+Alt+F1..F12 → switch virtual terminal (TTY / libseat mode).
    // On standard Linux XKB keymaps, Ctrl+Alt+Fn produces XKB_KEY_XF86Switch_VT_N
    // (not KEY_Fn). We handle both cases:
    //   1. modified_sym() returns XF86Switch_VT_N directly (most common).
    //   2. modified_sym() returns KEY_Fn + Ctrl+Alt modifiers (rare layouts).
    {
        let sym = keysym;
        // Case 1: XKB already mapped it to XF86Switch_VT_N
        let vt_from_xf86 = match sym {
            0x1008_FE01 => Some(1),
            0x1008_FE02 => Some(2),
            0x1008_FE03 => Some(3),
            0x1008_FE04 => Some(4),
            0x1008_FE05 => Some(5),
            0x1008_FE06 => Some(6),
            0x1008_FE07 => Some(7),
            0x1008_FE08 => Some(8),
            0x1008_FE09 => Some(9),
            0x1008_FE0A => Some(10),
            0x1008_FE0B => Some(11),
            0x1008_FE0C => Some(12),
            _ => None,
        };
        // Case 2: layout keeps KEY_Fn but Ctrl+Alt are held
        let vt_from_fn = if modifiers.ctrl && modifiers.alt && !modifiers.logo {
            vt_number_from_keysym(sym)
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

    if !modifiers.logo {
        return FilterResult::Forward;
    }

    match keysym {
        // Super+Enter → spawn kitty
        xkb::KEY_Return => {
            let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
            std::process::Command::new("kitty")
                .env("WAYLAND_DISPLAY", &wayland_display)
                .env_remove("DISPLAY")
                .spawn()
                .ok();
            FilterResult::Intercept(())
        }

        // Super+G → toggle Tiling ↔ Physics
        xkb::KEY_g | xkb::KEY_G => {
            app_state.toggle_physics();
            FilterResult::Intercept(())
        }

        // Super+Q → quit WM
        xkb::KEY_q | xkb::KEY_Q => {
            app_state.running = false;
            FilterResult::Intercept(())
        }

        // Super+Escape → quit WM
        xkb::KEY_Escape => {
            app_state.running = false;
            FilterResult::Intercept(())
        }

        // Super+Right/Left/Up/Down → focus (Tiling) или камера (Physics)
        xkb::KEY_Right => {
            if app_state.layout_mode == LayoutMode::Physics {
                app_state.move_camera(120.0, 0.0);
            } else {
                app_state.focus_next(true);
            }
            FilterResult::Intercept(())
        }

        xkb::KEY_Left => {
            if app_state.layout_mode == LayoutMode::Physics {
                app_state.move_camera(-120.0, 0.0);
            } else {
                app_state.focus_next(false);
            }
            FilterResult::Intercept(())
        }

        xkb::KEY_Up => {
            if app_state.layout_mode == LayoutMode::Physics {
                app_state.move_camera(0.0, -120.0);
            } else {
                app_state.focus_next(true);
            }
            FilterResult::Intercept(())
        }

        xkb::KEY_Down => {
            if app_state.layout_mode == LayoutMode::Physics {
                app_state.move_camera(0.0, 120.0);
            } else {
                app_state.focus_next(false);
            }
            FilterResult::Intercept(())
        }

        // Super+W → close focused window
        xkb::KEY_w | xkb::KEY_W => {
            let Some(kbd) = app_state.seat.get_keyboard() else {
                return FilterResult::Intercept(());
            };
            let focused = kbd.current_focus();
            if let Some(surf) = focused {
                if let Some(win) = app_state.space.elements()
                    .find(|w| {
                        w.toplevel()
                            .map(|t| t.wl_surface() == &surf)
                            .unwrap_or(false)
                    })
                    .cloned()
                {
                    if let Some(toplevel) = win.toplevel() {
                        toplevel.send_close();
                    }
                }
            }
            FilterResult::Intercept(())
        }

        _ => FilterResult::Forward,
    }
}

fn vt_number_from_keysym(keysym: u32) -> Option<i32> {
    match keysym {
        xkb::KEY_F1 => Some(1),
        xkb::KEY_F2 => Some(2),
        xkb::KEY_F3 => Some(3),
        xkb::KEY_F4 => Some(4),
        xkb::KEY_F5 => Some(5),
        xkb::KEY_F6 => Some(6),
        xkb::KEY_F7 => Some(7),
        xkb::KEY_F8 => Some(8),
        xkb::KEY_F9 => Some(9),
        xkb::KEY_F10 => Some(10),
        xkb::KEY_F11 => Some(11),
        xkb::KEY_F12 => Some(12),
        _ => None,
    }
}

/// Update pointer position and find the window under cursor
fn handle_pointer_motion(
    state: &mut AppState,
    x: f64,
    y: f64,
    serial: smithay::utils::Serial,
    time: u32,
) {
    // ── Физический режим: drag тела мышью ───────────────────────────────
    // Курсор здесь — экранные координаты; переводим в мировые (+ camera_offset).
    // В физическом режиме нет clamp к экрану — мир бесконечен, но сам курсор
    // устройства ограничен output'ом, что нормально.
    if state.layout_mode == LayoutMode::Physics {
        let world_x = x + state.camera_offset.0;
        let world_y = y + state.camera_offset.1;
        state.physics_drag_update(world_x, world_y);
        // Обновим позицию устройства (для курсора/фокуса).
        let pos: smithay::utils::Point<f64, smithay::utils::Logical> = (x, y).into();
        state.pointer_location = pos;
        if let Some(ptr) = state.seat.get_pointer() {
            ptr.motion(state, None, &MotionEvent { location: pos, serial, time });
        }
        return;
    }

    // Clamp to screen bounds
    let screen = state.output_size();
    let x = x.clamp(0.0, (screen.w - 1) as f64);
    let y = y.clamp(0.0, (screen.h - 1) as f64);
    let pos: smithay::utils::Point<f64, smithay::utils::Logical> = (x, y).into();

    state.pointer_location = pos;

    // --- Super+LMB tile resize ---
    if let (Some(resize_win), Some((start_x, start_y)), Some((drag_left, drag_top))) = (
        state.resize_window.clone(),
        state.resize_start_ptr,
        state.resize_edges,
    ) {
        let dx = x - start_x;
        let dy = y - start_y;

        // Remove time-based throttling to restore perfectly smooth live resizes.
        // The high CPU usage is primarily due to running the compositor in Debug mode.
        // We will rely on --release optimizations to solve the CPU overhead.
        const MIN_DELTA: f64 = 1.0;
        let dist_ok = dx.abs() >= MIN_DELTA || dy.abs() >= MIN_DELTA;

        if !dist_ok {
            return;
        }

        let screen = state.output_size();
        let gaps_out = 8i32;
        let screen_rect = smithay::utils::Rectangle::new(
            (gaps_out, gaps_out).into(),
            (screen.w - gaps_out * 2, screen.h - gaps_out * 2).into()
        );

        if let Some(mut tree) = state.tile_tree.take() {
            if tree.resize_target(&resize_win, dx, dy, drag_left, drag_top, screen_rect) {
                state.tile_tree = Some(tree);
                state.resize_start_ptr = Some((x, y));
                // Don't call recalculate_layout() here — it was running
                // on every mouse event (100-1000 Hz), sending configure
                // events to ALL windows each time. Defer to the render
                // loop which runs at ~60 Hz via layout_dirty.
                state.layout_dirty = true;
                state.needs_render = true;
                state.last_resize_time = std::time::Instant::now();
            } else {
                state.tile_tree = Some(tree);
            }
        }
        return;
    }

    // Find surface under pointer for hover focus
    let under = state.space.element_under(pos).and_then(|(w, loc)| {
        let loc_f64: smithay::utils::Point<f64, smithay::utils::Logical> = (loc.x as f64, loc.y as f64).into();
        w.toplevel().map(|t| (t.wl_surface().clone(), loc_f64))
    });

    if let Some(ptr) = state.seat.get_pointer() {
        ptr.motion(state, under, &MotionEvent { location: pos, serial, time });
        // Only mark needs_render for cursor-visible updates (DRM software cursor).
        // In winit mode the host compositor draws the cursor, so pointer motion
        // by itself never damages our framebuffer. The flag will be set by
        // commit() when a client actually submits a new buffer.
        // For DRM mode we always need to redraw the software cursor square.
        if state.session.is_some() {
            state.needs_render = true;
        }
    }
}

/// Shared click-to-focus + Super+LMB tile resize (Tiling) / LMB drag body (Physics)
fn handle_pointer_button(state: &mut AppState, button: u32, btn_state: ButtonState, time_msec: u32) {
    let serial = SERIAL_COUNTER.next_serial();
    let pos = state.pointer_location;

    // ── Физический режим: ЛКМ тащит тело ────────────────────────────────
    if state.layout_mode == LayoutMode::Physics && button == BTN_LEFT {
        let world_x = pos.x + state.camera_offset.0;
        let world_y = pos.y + state.camera_offset.1;
        if btn_state == ButtonState::Pressed {
            // Фокусируем окно под курсором и начинаем drag тела.
            if state.physics_drag_begin(world_x, world_y) {
                // Возьмём окно из drag_body для фокуса.
                if let Some((win, _)) = state.drag_body.clone() {
                    state.focus_window(&win);
                }
            }
        } else if btn_state == ButtonState::Released {
            state.physics_drag_end();
        }
        state.needs_render = true;
        return;
    }

    // End resize on LMB release even if Super was released first
    if button == BTN_LEFT && btn_state == ButtonState::Released && state.resize_window.is_some() {
        state.end_resize();
        return;
    }

    // Check if Super is held
    // No keyboard yet (early init) — can't determine Super state, bail safely.
    let Some(kbd) = state.seat.get_keyboard() else { return };
    let super_held = kbd.modifier_state().logo;

    // Super + Left Mouse Button → start tile resize
    if button == BTN_LEFT && super_held && btn_state == ButtonState::Pressed {
        let win = state.space.element_under(pos).map(|(w, _)| w.clone());
        if let Some(win) = win {
            let geo = state.space.element_geometry(&win).unwrap_or_default();

            // Determine which quadrant the user clicked in to decide which edges to move
            let rel_x = (pos.x - geo.loc.x as f64) / geo.size.w as f64;
            let rel_y = (pos.y - geo.loc.y as f64) / geo.size.h as f64;
            let drag_left = rel_x < 0.5;
            let drag_top = rel_y < 0.5;

            state.resize_window = Some(win);
            state.resize_start_ptr = Some((pos.x, pos.y));
            state.resize_edges = Some((drag_left, drag_top));
        }
        return;
    }

    // While resizing, ignore other pointer button events
    if state.resize_window.is_some() {
        return;
    }

    if btn_state == ButtonState::Pressed {
        let window = state.space.element_under(pos).map(|(w, _)| w.clone());
        if let Some(window) = window {
            let win_clone = window.clone();
            state.focus_window(&win_clone);
        } else {
            // Click on empty space → unfocus
            let serial2 = SERIAL_COUNTER.next_serial();
            if let Some(kbd) = state.seat.get_keyboard() {
                kbd.set_focus(state, None, serial2);
                state.needs_render = true;
            }
        }
    }

    if let Some(ptr) = state.seat.get_pointer() {
        ptr.button(
            state,
            &ButtonEvent {
                button,
                state: btn_state,
                serial,
                time: time_msec,
            },
        );
        state.needs_render = true;
    }
}
