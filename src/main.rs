pub mod state;
pub mod input;
pub mod backend_drm;
pub mod tiling;
pub mod render;
pub mod physics;

use std::sync::Arc;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::Transform;
use smithay::desktop::space::render_output;
use smithay::backend::winit::{self, WinitEvent};

use crate::state::{AppState, ClientState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Определяем, где запущены: в TTY или в графической сессии
    let in_tty = std::env::var("WAYLAND_DISPLAY").is_err()
        && std::env::var("DISPLAY").is_err();

    let event_loop: EventLoop<AppState> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let display: Display<AppState> = Display::new()?;
    let display_handle = display.handle();

    let mut state = AppState::new(display_handle.clone());

    // Запускаем Wayland сокет (для клиентских приложений)
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_string_lossy().into_owned();
    println!("=====> Сокет: {}", socket_name);
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };

    loop_handle.insert_source(listening_socket, |client_stream, _, state| {
        state.display_handle.insert_client(
            client_stream,
            Arc::new(ClientState::default()),
        ).unwrap();
    })?;

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
        run_winit(event_loop, display, state, socket_name)?;
        return Ok(());
    }

    Ok(())
}

fn run_winit(
    mut event_loop: EventLoop<AppState>,
    mut display: Display<AppState>,
    mut state: AppState,
    _socket_name: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let loop_handle = event_loop.handle();

    let (mut backend, winit_loop) =
        winit::init::<smithay::backend::renderer::gles::GlesRenderer>()?;

    // Создаём Output для winit окна
    let size = backend.window_size();
    backend.window().request_redraw();

    let mode = Mode { size, refresh: 60_000 };
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
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // DMA-BUF (zero-copy) for winit/nested mode.
    //
    // Bind the GLES renderer's EGL reader to the display (Mesa clients need
    // either this wl_drm binding or a v4 dmabuf feedback). Then advertise the
    // linux-dmabuf global built from the renderer's supported formats, so GPU
    // clients can submit buffers as dmabuf file descriptors instead of wl_shm.
    use smithay::backend::renderer::{ImportEgl, ImportDma};
    if let Err(e) = backend.renderer().bind_wl_display(&state.display_handle) {
        eprintln!("[Winit] EGL bind_wl_display failed: {e:?}");
    }

    let dmabuf_formats = backend.renderer().dmabuf_formats();
    use smithay::backend::egl::EGLDevice;
    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
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

    loop_handle.insert_source(winit_loop, move |event, _, state| {
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

                // ── Физический режим: шаг симуляции + камера ─────────────
                // map_output смещает вид: world = screen + camera_offset.
                // В Tiling-режиме камера всегда (0,0) — output на месте.
                if state.layout_mode == crate::state::LayoutMode::Physics {
                    state.space.map_output(
                        &output,
                        (-state.camera_offset.0 as i32, -state.camera_offset.1 as i32),
                    );
                    // Шаг физики продвигает симуляцию и применяет трансформы
                    // тел к окнам. Держит needs_render поднятым, пока тела
                    // двигаются; когда всё улеглось — рендер уснёт.
                    state.step_physics();
                }

                // Process deferred layout (resize, winit window resize).
                // This runs at most once per frame (~60 Hz) instead of
                // once per mouse event (100-1000 Hz).
                if state.layout_dirty {
                    state.recalculate_layout();
                    state.layout_dirty = false;
                }

                if state.needs_render {
                    let age = backend.buffer_age().unwrap_or(0);
                    let render_res = if let Ok((renderer, mut fb)) = backend.bind() {
                        let custom: &[smithay::backend::renderer::element::solid::SolidColorRenderElement] = &[];
                        Some(render_output(
                            &output,
                            renderer,
                            &mut fb,
                            1.0,
                            age,
                            [&state.space],
                            custom,
                            &mut damage_tracker,
                            [0.08, 0.08, 0.12, 1.0],
                        ))
                    } else {
                        None
                    };

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
                    let now = state.clock.now();
                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output,
                            now,
                            Some(std::time::Duration::ZERO),
                            |_, _| Some(output.clone()),
                        );
                    });
                    state.needs_render = false;
                }

                // Only request the next frame when there is something to
                // draw. Previously this was UNCONDITIONAL — it kept a
                // Redraw event permanently in the winit event queue, so
                // calloop's dispatch(16ms) NEVER actually blocked, turning
                // the main loop into a busy-spin that constantly ran
                // dispatch_clients + flush_clients even on a fully idle
                // desktop.
                if state.needs_render || state.layout_dirty {
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
    }

    Ok(())
}
