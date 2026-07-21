// XWayland-интеграция: спавн Xwayland и XWM (X11 window manager).
// X11-окна (Steam и другие) становятся обычными Window в Space и участвуют
// в тайлинге и физике наравне с Wayland-клиентами.

use std::process::Stdio;

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::{
    X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    xwm::{Reorder, ResizeEdge, XwmId},
};

use crate::state::{AppState, LayoutMode};

impl XWaylandShellHandler for AppState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
smithay::delegate_xwayland_shell!(AppState);

impl XwmHandler for AppState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XWM запрошен до инициализации")
    }

    fn new_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(e) = surface.set_mapped(true) {
            eprintln!("[xwayland] set_mapped failed: {e:?}");
            return;
        }
        let window = Window::new_x11_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);

        // Как в new_toplevel: вставляем в BSP-дерево всегда, чтобы окно
        // корректно встало в тайл при возврате из физического режима.
        let focused = self.get_focused_window();
        if let Some(tree) = self.tile_tree.take() {
            let target = focused.unwrap_or_else(|| window.clone());
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.output_rect()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        if self.layout_mode == LayoutMode::Physics {
            // Тело заспавнится в commit(), когда клиент отрисует реальный буфер.
        } else {
            self.recalculate_layout();
        }

        self.focus_window(&window);
        self.needs_render = true;
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        // Меню/тултипы/сплэши: кладём как есть, без тайлинга и без фокуса.
        let geo = surface.geometry();
        let window = Window::new_x11_window(surface);
        self.space.map_element(window, geo.loc, true);
        // Keep visible CEF popups above their parent in both compositor and X11.
        self.sync_x11_stacking();
        self.needs_render = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let window = self
            .space
            .elements()
            .find(|w| w.x11_surface().map(|x| x == &surface).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.remove_window_common(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, surface: X11Surface) {
        self.unmapped_window(xwm, surface);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // До map — честно отвечаем на запрос размера, чтобы клиент не завис.
        // После map размер диктует наш тайлинг/физика.
        let mapped = self
            .space
            .elements()
            .any(|win| win.x11_surface().map(|x| x == &surface).unwrap_or(false));
        if !mapped {
            let mut geo = surface.geometry();
            if let Some(w) = w {
                geo.size.w = w as i32;
            }
            if let Some(h) = h {
                geo.size.h = h as i32;
            }
            let _ = surface.configure(geo);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Override-redirect окна двигают себя сами (меню Steam и т.п.).
        if surface.is_override_redirect() {
            let window = self
                .space
                .elements()
                .find(|w| w.x11_surface().map(|x| x == &surface).unwrap_or(false))
                .cloned();
            if let Some(window) = window {
                self.space.map_element(window, geometry.loc, true);
                self.sync_x11_stacking();
                self.needs_render = true;
            }
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _surface: X11Surface,
        _button: u32,
        _edges: ResizeEdge,
    ) {
    }
    fn move_request(&mut self, _xwm: XwmId, _surface: X11Surface, _button: u32) {}
}

/// Спавнит Xwayland и вешает обработчик готовности на event loop.
/// После готовности выставляет $DISPLAY — X11-приложения (Steam) смогут
/// подключаться к встроенному X-серверу.
pub fn spawn_xwayland(
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, AppState>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    // Паник-хук: пишем панику и бектрейс в panic.log, чтобы причину
    // краша было видно, даже если stderr не перенаправлен в файл.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("PANIC: {info}\n\nbacktrace:\n{bt}\n");
        eprintln!("{msg}");
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/home/user/sandboxWM/panic.log")
        {
            let _ = f.write_all(msg.as_bytes());
        }
    }));

    let (xwayland, client) = XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| {},
    )?;

    let lh = loop_handle.clone();
    loop_handle.insert_source(xwayland, move |event, _, state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            match X11Wm::start_wm(lh.clone(), x11_socket, client.clone()) {
                Ok(wm) => {
                    state.xwm = Some(wm);
                    state.xdisplay = Some(display_number);
                    unsafe { std::env::set_var("DISPLAY", format!(":{display_number}")) };
                    // Обновляем окружение systemd/dbus, чтобы порталы и
                    // активируемые сервисы тоже видели DISPLAY.
                    let _ = std::process::Command::new("dbus-update-activation-environment")
                        .arg("--systemd")
                        .arg("DISPLAY")
                        .spawn()
                        .map(|mut child| child.wait());
                    println!("=====> XWayland готов: DISPLAY=:{display_number}");
                }
                Err(e) => eprintln!("[xwayland] не удалось запустить XWM: {e}"),
            }
        }
        XWaylandEvent::Error => {
            eprintln!(
                "[xwayland] Xwayland завершился с ошибкой (X11-приложения не будут работать)"
            );
        }
    })?;

    Ok(())
}
