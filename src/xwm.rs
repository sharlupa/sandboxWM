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
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.tiling_area()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        // В физике тело появится в commit(), когда клиент отрисует буфер.
        if self.layout_mode == LayoutMode::Tiling {
            self.recalculate_layout();
        }

        // Поверх полноэкранного окна новое не выскакивает (см. new_toplevel).
        if self.fullscreen_window().is_some() {
            self.place_fullscreen_window();
        } else {
            self.focus_window(&window);
        }
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
        let window = self.x11_window(&surface);
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
        let mapped = self.x11_window(&surface).is_some();
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

    /// Игры (Wine/Proton) просят _NET_WM_STATE_FULLSCREEN.
    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        match self.x11_window(&surface) {
            Some(window) => self.set_fullscreen(&window, true),
            None => {
                let _ = surface.set_fullscreen(true);
            }
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        match self.x11_window(&surface) {
            Some(window) => self.set_fullscreen(&window, false),
            None => {
                let _ = surface.set_fullscreen(false);
            }
        }
    }
}

impl AppState {
    /// Окно X11-поверхности на любом рабочем столе.
    fn x11_window(&self, surface: &X11Surface) -> Option<Window> {
        let is_it = |w: &Window| w.x11_surface() == Some(surface);
        self.space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it))
    }
}

/// Спавнит Xwayland и вешает обработчик готовности на event loop.
/// После готовности выставляет $DISPLAY — X11-приложения (Steam) смогут
/// подключаться к встроенному X-серверу.
pub fn spawn_xwayland(
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, AppState>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
) -> Result<(), Box<dyn std::error::Error>> {
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
