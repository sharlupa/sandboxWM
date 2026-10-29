pub mod backend_drm;
pub mod config;
pub mod cursor;
pub mod input;
pub mod mesh_render;
pub mod output_manager;
pub mod overlay;
pub mod physics;
pub mod render;
pub mod sandbox;
pub mod screencopy;
pub mod session;
pub mod softbody;
pub mod state;
pub mod tiling;
pub mod xwm;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::Transform;
use smithay::wayland::socket::ListeningSocketSource;
use std::sync::Arc;

use crate::state::{AppState, ClientState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();

    // Определяем, где запущены: в TTY или в графической сессии
    let in_tty = std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err();

    let event_loop: EventLoop<AppState> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let display: Display<AppState> = Display::new()?;
    let display_handle = display.handle();

    let mut state = AppState::new(display_handle.clone());

    // Winit-бэкенд инициализируем ДО подмены WAYLAND_DISPLAY: иначе winit
    // подключается как клиент к нашему же (ещё не обслуживаемому) сокету и
    // навсегда зависает в roundtrip.
    let winit_backend = if in_tty {
        None
    } else {
        Some(winit::init::<smithay::backend::renderer::gles::GlesRenderer>()?)
    };

    // Запускаем Wayland сокет (для клиентских приложений)
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket
        .socket_name()
        .to_string_lossy()
        .into_owned();
    println!("=====> Сокет: {}", socket_name);
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
        std::env::set_var("XDG_CURRENT_DESKTOP", "sandboxWM");
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        // Electron/Chromium (Discord, VS Code, ...): без этой подсказки ozone
        // выбирает X11, а X-сервера/XWayland у нас нет →
        // "Missing X server or $DISPLAY". С auto они берут Wayland-сокет.
        std::env::set_var("ELECTRON_OZONE_PLATFORM_HINT", "auto");
    }

    if in_tty {
        // Notify dbus of the Wayland socket for xdg-desktop-portal
        let _ = std::process::Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .arg("WAYLAND_DISPLAY")
            .arg("XDG_CURRENT_DESKTOP")
            .arg("XDG_SESSION_TYPE")
            .spawn()
            .map(|mut child| child.wait());

        // Перезапускаем портальный стек, чтобы он подхватил новое окружение.
        // Уже работающий xdg-desktop-portal мог быть активирован из другой
        // сессии (например, Hyprland) со старым XDG_CURRENT_DESKTOP и не
        // перечитывает окружение сам — из-за этого запрос ScreenCast от OBS
        // уходил в бэкенд чужого композитора (диалог выбора экрана появлялся
        // в Hyprland). После рестарта портал увидит
        // XDG_CURRENT_DESKTOP=sandboxWM, возьмёт sandboxWM-portals.conf и
        // активирует xdg-desktop-portal-wlr, привязанный уже к нашему сокету.
        //
        // ВАЖНО: --no-block обязателен. Без него systemctl ждёт полного
        // старта сервисов, а xdg-desktop-portal-wlr при старте подключается
        // к нашему Wayland-сокету, event loop которого здесь ещё не запущен —
        // блокирующий вызов привёл бы к взаимной блокировке.
        // Отложенный и упорядоченный рестарт в фоне: сначала ждём пару
        // секунд, пока DRM-бэкенд поднимет выходы и начнётся диспетчеризация,
        // затем стартуем wlr-бэкенд и только после него — основной портал.
        // Основной портал опрашивает бэкенды один раз на старте: если wlr
        // ещё не на шине, интерфейс ScreenCast не экспортируется вовсе
        // (симптом: в OBS нет пункта «Захват экрана (PipeWire)»).
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 2; systemctl --user restart --no-block xdg-desktop-portal-wlr.service; sleep 1; systemctl --user restart --no-block xdg-desktop-portal.service")
            .spawn();
    }

    loop_handle.insert_source(listening_socket, |client_stream, _, state| {
        state
            .display_handle
            .insert_client(client_stream, Arc::new(ClientState::default()))
            .unwrap();
    })?;

    // XWayland: встроенный X-сервер для X11-приложений (Steam, старые игры).
    // Steam — X11-клиент: без X-сервера он падает с "XOpenDisplay failed".
    if let Err(e) = crate::xwm::spawn_xwayland(&loop_handle, &display_handle) {
        eprintln!("[xwayland] не удалось запустить Xwayland: {e}");
    }

    if in_tty {
        // ── TTY / DRM режим ──────────────────────────────────────────────
        // run_tty() takes ownership of event_loop + display, runs the main
        // dispatch loop internally, and handles all DRM/EGL/GBM resource
        // cleanup in the correct order before returning.
        //
        // state.session (the last LibSeatSession Arc) drops when main()
        // returns — AFTER all native DRM/EGL resources are already gone.
        println!("=====> Режим: DRM/KMS (TTY)");
        drop(loop_handle);
        backend_drm::run_tty(event_loop, display, &mut state)?;
    } else {
        // ── Winit режим (вложенное окно в X11 / Wayland) ─────────────────
        println!("=====> Режим: Winit (вложенное окно)");
        let (backend, winit_loop) = winit_backend.expect("winit backend initialized above");
        run_winit(event_loop, display, state, backend, winit_loop)?;
        return Ok(());
    }

    Ok(())
}

/// Паника → stderr и `$XDG_STATE_HOME/sandboxWM/panic.log` (раньше путь был
/// захардкожен в каталог репозитория и хук ставился только при старте XWayland).
fn install_panic_hook() {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local").join("state"))
        })
        .map(|d| d.join("sandboxWM"));
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("PANIC: {info}\n\nbacktrace:\n{bt}\n");
        eprintln!("{msg}");
        if let Some(dir) = &dir {
            use std::io::Write as _;
            let _ = std::fs::create_dir_all(dir);
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
            {
                let _ = f.write_all(msg.as_bytes());
            }
        }
    }));
}

fn run_winit(
    mut event_loop: EventLoop<AppState>,
    mut display: Display<AppState>,
    mut state: AppState,
    backend: winit::WinitGraphicsBackend<smithay::backend::renderer::gles::GlesRenderer>,
    winit_loop: winit::WinitEventLoop,
) -> Result<(), Box<dyn std::error::Error>> {
    let loop_handle = event_loop.handle();
    // Бэкенд нужен и обработчику winit-событий, и основному циклу: после
    // dispatch_clients коммиты клиентов поднимают needs_render, а без
    // request_redraw winit спит до следующего события ввода — окно
    // появлялось только после движения мыши.
    let backend = std::rc::Rc::new(std::cell::RefCell::new(backend));
    let mut backend_guard = backend.borrow_mut();
    let backend_ref = &mut *backend_guard;
    let backend_loop = backend.clone();
    // Курсор нужен только для захвата экрана с overlay_cursor (на экране
    // указатель рисует хост); screencopy общий с основным циклом, чтобы
    // ожидающий кадр будил рендер и на неподвижном столе.
    let mut cursor_renderer = crate::cursor::CursorRenderer::new();
    let screencopy = std::rc::Rc::new(std::cell::RefCell::new(
        crate::screencopy::ScreencopyRenderer::new(),
    ));
    let screencopy_loop = screencopy.clone();

    // Создаём Output для winit окна
    let size = backend_ref.window_size();
    backend_ref.window().request_redraw();

    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
        },
    );
    let _global = output.create_global::<AppState>(&state.display_handle);
    // Framebuffer winit-окна — обычный GL с началом координат внизу слева,
    // поэтому как в примерах Smithay выход помечается Flipped180. С Normal
    // вложенный режим рисовал всё вверх ногами.
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let output_main = output.clone();
    // Выход есть — можно грузить сессию (пол/камера зависят от размера экрана).
    state.on_output_ready();

    // DMA-BUF (zero-copy) for winit/nested mode.
    //
    // Bind the GLES renderer's EGL reader to the display (Mesa clients need
    // either this wl_drm binding or a v4 dmabuf feedback). Then advertise the
    // linux-dmabuf global built from the renderer's supported formats, so GPU
    // clients can submit buffers as dmabuf file descriptors instead of wl_shm.
    use smithay::backend::renderer::{ImportDma, ImportEgl};
    if let Err(e) = backend_ref.renderer().bind_wl_display(&state.display_handle) {
        eprintln!("[Winit] EGL bind_wl_display failed: {e:?}");
    }

    let dmabuf_formats = backend_ref.renderer().dmabuf_formats();
    state.screencopy_state.set_renderer(backend_ref.renderer());
    use smithay::backend::egl::EGLDevice;
    let render_node = EGLDevice::device_for_display(backend_ref.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    let dmabuf_state = &mut state.dmabuf_state;
    match render_node {
        Ok(Some(node)) => {
            let feedback =
                smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
                    .build()
                    .expect("failed to build dmabuf feedback");
            let global = dmabuf_state
                .create_global_with_default_feedback::<AppState>(&state.display_handle, &feedback);
            state.dmabuf_global = Some(global);
        }
        _ => {
            // No render node (e.g. running on a non-Mesa stack): fall back to
            // the simpler v3 global that advertises only the format list.
            eprintln!("[Winit] render node not found, dmabuf using v3 fallback");
            let global =
                dmabuf_state.create_global::<AppState>(&state.display_handle, dmabuf_formats);
            state.dmabuf_global = Some(global);
        }
    }

    drop(backend_guard);

    loop_handle.insert_source(winit_loop, move |event, _, state| {
        let mut backend = backend_loop.borrow_mut();
        match event {
            WinitEvent::Resized { size, .. } => {
                let new_mode = Mode { size, refresh: 60_000 };
                output.change_current_state(Some(new_mode), None, None, None);
                // Defer layout to the Redraw handler — during drag-resize
                // of the winit window, Resized fires at the mouse event rate
                // (100-1000 Hz). layout_dirty batches them to ~60 Hz.
                state.layout_dirty = true;
                state.needs_render = true;
                backend.window().request_redraw();
            }
            WinitEvent::Input(input_event) => {
                input::process_input_event(state, input_event);
                // If input set needs_render (resize, etc.), schedule a
                // Redraw so the render loop picks it up.
                if state.needs_render {
                    backend.window().request_redraw();
                }
            }
            WinitEvent::Redraw => {
                let size = backend.window_size();
                if size.w == 0 || size.h == 0 {
                    return;
                }

                // map_output смещает вид Space: world = screen + camera_offset
                // (output enter/leave для окон в физике).
                state.space.map_output(
                    &output,
                    (state.camera_offset.0 as i32, state.camera_offset.1 as i32),
                );
                // Шаг физики (no-op в тайлинге): держит needs_render поднятым,
                // пока тела двигаются; когда всё улеглось — рендер засыпает.
                state.step_physics();

                // Process deferred layout (resize, winit window resize).
                // This runs at most once per frame (~60 Hz) instead of
                // once per mouse event (100-1000 Hz).
                if state.layout_dirty {
                    state.recalculate_layout();
                    state.layout_dirty = false;
                }

                if screencopy_loop
                    .borrow()
                    .wants_render(&state.screencopy_state, &output)
                {
                    state.needs_render = true;
                }

                if state.needs_render {
                    let age = backend.buffer_age().unwrap_or(0);
                    let clear = state.background_color();
                    // Элементы собираем до bind(): снимки мягких окон и захват
                    // экрана рендерят в свои offscreen-текстуры.
                    let renderer = backend.renderer();
                    let mut elements = crate::render::frame_elements(renderer, state, &output);
                    let cursor = cursor_renderer.elements(
                        renderer,
                        &mut state.cursor_status,
                        state.pointer_location,
                        1.0,
                    );
                    let cursor_count = cursor.len();
                    elements.splice(0..0, cursor);
                    screencopy_loop.borrow_mut().fulfill_pending(
                        renderer,
                        &state.screencopy_state,
                        &output,
                        &elements,
                        cursor_count,
                        clear,
                    );
                    let render_res = backend.bind().ok().map(|(renderer, mut fb)| {
                        damage_tracker.render_output(
                            renderer,
                            &mut fb,
                            age,
                            &elements[cursor_count..],
                            clear,
                        )
                    });

                    if let Some(res) = render_res {
                        match res {
                            Ok(result) => {
                                let _ = backend.submit(result.damage.as_deref().map(|v| &**v));
                            }
                            Err(err) => {
                                eprintln!("[Winit] Ошибка рендера: {err:?}");
                                let _ = backend.submit(None);
                            }
                        }
                    }

                    state.space.refresh();
                    state.popup_manager.cleanup();
                    let now = state.clock.now();
                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output,
                            now,
                            Some(std::time::Duration::ZERO),
                            |_, _| Some(output.clone()),
                        );
                    });
                    crate::cursor::send_frames(&state.cursor_status, &output, now);
                    state.needs_render = false;
                }

                // Only request the next frame when there is something to
                // draw — otherwise a permanent Redraw kept calloop's
                // dispatch() from ever blocking (busy-spin on an idle desktop).
                if state.needs_render || state.layout_dirty || state.physics_animating() {
                    backend.window().request_redraw();
                }
            }
            WinitEvent::CloseRequested => {
                state.running = false;
            }
            _ => {}
        }
    })?;

    while state.running {
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)?;
        display.dispatch_clients(&mut state)?;
        display.flush_clients()?;
        // Redraw в winit приходит только при изменениях — автосохранение
        // проверяем и в простое.
        state.maybe_autosave(std::time::Instant::now());
        let capture = screencopy
            .borrow()
            .wants_render(&state.screencopy_state, &output_main);
        if state.needs_render || state.layout_dirty || state.physics_animating() || capture {
            backend.borrow().window().request_redraw();
        }
    }

    state.save_session();
    Ok(())
}
