pub mod state;
pub mod input;
pub mod backend_drm;
pub mod tiling;
pub mod render;

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

    let mut event_loop: EventLoop<AppState> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let mut display: Display<AppState> = Display::new()?;
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
        println!("=====> Режим: DRM/KMS (TTY)");
        backend_drm::run_tty(&mut event_loop, display_handle, &mut state)?;

        while state.running {
            event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)?;
            display.dispatch_clients(&mut state)?;
            display.flush_clients()?;
        }
    } else {
        // ── Winit режим (вложенное окно в X11 / Wayland) ─────────────────
        println!("=====> Режим: Winit (вложенное окно)");
        run_winit(event_loop, display, state, socket_name)?;
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

    loop_handle.insert_source(winit_loop, move |event, _, state| {
        match event {
            WinitEvent::Resized { size, .. } => {
                let new_mode = Mode { size, refresh: 60_000 };
                output.change_current_state(Some(new_mode), None, None, None);
            }
            WinitEvent::Input(input_event) => {
                input::process_input_event(state, input_event);
            }
            WinitEvent::Redraw => {
                let size = backend.window_size();
                if size.w == 0 || size.h == 0 {
                    return;
                }

                // Damage-gated redraw: only do the heavy render_output / send_frame
                // work when something actually changed. The outer event loop is
                // already throttled by dispatch(16ms), so even a no-op Redraw is
                // cheap; this just avoids burning a full GPU frame on an idle
                // desktop. Always request the next redraw to keep the loop alive.
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

                backend.window().request_redraw();
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
