use smithay::{
    backend::session::libseat::LibSeatSession,
    desktop::{Space, Window},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_surface::WlSurface, wl_seat::WlSeat},
            Client, DisplayHandle,
        },
    },
    utils::{Clock, Monotonic, Serial, Rectangle, Size, Logical},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState,
        },
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        output::{OutputHandler, OutputManagerState},
        shm::{ShmHandler, ShmState},
    },
};

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub struct AppState {
    pub display_handle: DisplayHandle,
    pub clock: Clock<Monotonic>,

    // Smithay states
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,

    // WM states
    pub space: Space<Window>,
    pub seat: Seat<Self>,
    pub running: bool,

    // Tiling / resize state
    pub resize_window: Option<Window>,
    pub resize_start_ptr: Option<(f64, f64)>,
    pub resize_start_geo: Option<Rectangle<i32, Logical>>,
    pub tile_tree: Option<crate::tiling::TileNode>,

    // TTY session (None in winit/nested mode)
    pub session: Option<LibSeatSession>,
    pub session_paused: bool,

    // Cursor
    pub cursor_status: CursorImageStatus,
    pub pointer_location: smithay::utils::Point<f64, smithay::utils::Logical>,

    // Damage flag — the render loop only redraws when something actually changed
    // (window added/removed/focused/resized, surface committed, pointer moved).
    // Set to `true` at the relevant mutation points, reset after a successful frame.
    pub needs_render: bool,
}

impl AppState {
    pub fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let _ = seat.add_keyboard(Default::default(), 200, 25);
        let _ = seat.add_pointer();

        Self {
            display_handle,
            clock: Clock::new(),
            compositor_state,
            shm_state,
            xdg_shell_state,
            output_manager_state,
            seat_state,
            space: Space::default(),
            seat,
            running: true,
            resize_window: None,
            resize_start_ptr: None,
            resize_start_geo: None,
            tile_tree: None,
            session: None,
            session_paused: false,
            cursor_status: CursorImageStatus::default_named(),
            pointer_location: (0.0, 0.0).into(),
            needs_render: true,
        }
    }

    /// Finish an active tile resize and re-apply the saved layout.
    pub fn end_resize(&mut self) {
        if self.resize_window.is_none() {
            return;
        }
        self.resize_window = None;
        self.resize_start_ptr = None;
        self.resize_start_geo = None;
        self.recalculate_layout();
        self.needs_render = true;
    }

    /// Get the screen size from the first mapped output.
    pub fn output_size(&self) -> Size<i32, Logical> {
        self.space
            .outputs()
            .next()
            .and_then(|o| o.current_mode())
            .map(|m| (m.size.w, m.size.h).into())
            .unwrap_or_else(|| (1920, 1080).into())
    }

    /// Get current screen rectangle
    pub fn output_rect(&self) -> Rectangle<i32, Logical> {
        let size = self.output_size();
        Rectangle::new((0, 0).into(), size)
    }

    /// BSP tiling layout calculation.
    pub fn recalculate_layout(&mut self) {
        let screen = self.output_size();
        let gaps_in = 4i32; // between windows
        let gaps_out = 8i32; // from screen edges
        
        let screen_rect = Rectangle::new(
            (gaps_out, gaps_out).into(),
            (screen.w - gaps_out * 2, screen.h - gaps_out * 2).into()
        );

        if let Some(tree) = &self.tile_tree {
            let mut rects = Vec::new();
            tree.collect_rects(screen_rect, gaps_in, &mut rects);
            for (win, rect) in rects {
                self.apply_window_geometry(&win, rect);
            }
        }
    }

    fn apply_window_geometry(&mut self, window: &Window, rect: Rectangle<i32, Logical>) {
        // Re-map at the correct position
        self.space.map_element(window.clone(), rect.loc, false);

        // Ask the client to resize its surface — but only if the size actually
        // changed. Sending a configure with the same size on every relayout used
        // to trigger an O(N²) "configure storm": each client re-commits its
        // buffer in response, which re-runs the layout, which re-configures every
        // window again. Comparing against the already-acked `current_state`
        // breaks that feedback loop after a single round-trip.
        if let Some(toplevel) = window.toplevel() {
            let already = toplevel
                .current_state()
                .size
                .map(|s| s == rect.size)
                .unwrap_or(false);
            if !already {
                toplevel.with_pending_state(|state| {
                    state.size = Some(rect.size.into());
                });
                toplevel.send_configure();
            }
        }
    }

    /// Set keyboard focus to a specific window.
    pub fn focus_window(&mut self, window: &Window) {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();

        // Deactivate all windows first, then activate the target
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for w in &windows {
            let is_target = w == window;
            w.set_activated(is_target);
            if let Some(toplevel) = w.toplevel() {
                toplevel.with_pending_state(|state| {
                    if is_target {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                toplevel.send_configure();
            }
        }

        // Set keyboard focus to target window's surface
        if let Some(toplevel) = window.toplevel() {
            let wl_surface = toplevel.wl_surface().clone();
            if let Some(kbd) = self.seat.get_keyboard() {
                kbd.set_focus(self, Some(wl_surface), serial);
            }
        }

        self.space.raise_element(window, true);
        self.needs_render = true;
    }

    /// Move focus to the next or previous window in the stack.
    pub fn focus_next(&mut self, forward: bool) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        if windows.is_empty() {
            return;
        }

        let kbd = match self.seat.get_keyboard() {
            Some(k) => k,
            None => return,
        };
        let current_surface = kbd.current_focus();
        let current_idx = current_surface.and_then(|surf| {
            windows.iter().position(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == &surf)
                    .unwrap_or(false)
            })
        });

        let next_idx = match current_idx {
            Some(idx) => {
                if forward {
                    (idx + 1) % windows.len()
                } else {
                    if idx == 0 { windows.len() - 1 } else { idx - 1 }
                }
            }
            None => 0,
        };

        let target = windows[next_idx].clone();
        self.focus_window(&target);
    }

    /// Get currently focused window
    pub fn get_focused_window(&self) -> Option<Window> {
        let kbd = self.seat.get_keyboard()?;
        let surf = kbd.current_focus()?;
        self.space.elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == &surf).unwrap_or(false))
            .cloned()
    }
}

// 1. Compositor
impl CompositorHandler for AppState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Обновляем буферы рендера
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        // Обновляем bbox всех окон которые используют эту поверхность
        let mut relayout = false;
        for window in self.space.elements() {
            if window.wl_surface().as_ref().map(|s| s.as_ref()) == Some(surface) {
                window.on_commit();
                relayout = true;
                break;
            }
        }
        // Keep tiled geometry after client commits (prevents snap-back).
        if relayout && self.tile_tree.is_some() && self.resize_window.is_none() {
            self.recalculate_layout();
        }
        // A new buffer was committed, so the screen needs redrawing.
        self.needs_render = true;
    }
}

smithay::delegate_compositor!(AppState);

// 2. Buffer & SHM
impl BufferHandler for AppState {
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
}
impl ShmHandler for AppState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
smithay::delegate_shm!(AppState);

// 2.5 Output
impl OutputHandler for AppState {}
smithay::delegate_output!(AppState);

// 3. XDG Shell
impl XdgShellHandler for AppState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Сначала помещаем в (0,0) с заглушкой — размер пересчитается при layout
        surface.with_pending_state(|state| {
            state.size = Some((800, 600).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        // Пока кладём наверху (будет перемещён layout-ом)
        self.space.map_element(window.clone(), (0, 0), true);

        // Обновляем BSP дерево
        let focused = self.get_focused_window();
        if let Some(tree) = self.tile_tree.take() {
            let target = focused.unwrap_or_else(|| window.clone());
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.output_rect()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        // Пересчитываем тайловую раскладку для всех окон
        self.recalculate_layout();

        // Передаём клавиатурный фокус новому окну
        self.focus_window(&window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.space.unmap_elem(&window);
            
            if let Some(tree) = self.tile_tree.take() {
                self.tile_tree = tree.remove(&window);
            }
            
            // Пересчитываем тайловую раскладку
            self.recalculate_layout();
            // Фокус — последнее окно если есть (clone раньше borrow)
            let next_win = self.space.elements().next_back().cloned();
            if let Some(win) = next_win {
                self.focus_window(&win);
            }
            // Окно ушло — экран точно нужно перерисовать (даже если space пустой).
            self.needs_render = true;
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
}
smithay::delegate_xdg_shell!(AppState);

// 4. Seat / Input
impl SeatHandler for AppState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }
}
smithay::delegate_seat!(AppState);
