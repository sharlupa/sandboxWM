use smithay::reexports::wayland_server::Resource;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::{
    backend::session::libseat::LibSeatSession,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space,
        Window, find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, Focus, PointerHandle},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{
            PointerConstraintsHandler, PointerConstraintsState, with_pointer_constraint,
        },
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
        shm::{ShmHandler, ShmState},
    },
};

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

/// Режим раскладки окон. Переключается горячей клавишей Super+G.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Жёсткий тайлинг (BSP/Dwindle) — поведение до Phase 1.
    Tiling,
    /// Физический режим: окна = динамические тела rapier2d на бесконечном
    /// холсте с гравитацией и полом.
    Physics,
}

impl Default for LayoutMode {
    fn default() -> Self {
        LayoutMode::Tiling
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Отступы тайлинга: между окнами и от краёв экрана.
pub const GAPS_IN: i32 = 4;
pub const GAPS_OUT: i32 = 8;

pub struct AppState {
    pub display_handle: DisplayHandle,
    pub clock: Clock<Monotonic>,

    /// Загруженный при старте конфиг (см. `config.rs`).
    pub config: crate::config::Config,

    // Smithay states
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub popup_manager: PopupManager,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub xdg_decoration_state: XdgDecorationState,
    pub screencopy_state: crate::screencopy::ScreencopyState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub wlr_output_manager_state: crate::output_manager::OutputManagerState,
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// wlr-layer-shell: панели (waybar), фоны, лаунчеры, уведомления.
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// Относительное движение мыши и захват указателя — для игр.
    pub relative_pointer_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    /// ext-workspace-v1: рабочие столы для панелей.
    pub ext_workspace_state: crate::ext_workspace::ExtWorkspaceState,

    // XWayland: shell-state, оконный менеджер X11 и номер X-дисплея
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// Новое X11-окно попадает в X-стек Smithay только после MapNotify —
    /// порядок нужно повторить на его первом коммите.
    pub x11_stacking_dirty: bool,
    pub xdisplay: Option<u32>,

    // WM states
    pub space: Space<Window>,
    pub seat: Seat<Self>,
    pub running: bool,

    // Tiling / resize state
    pub resize_window: Option<Window>,
    pub resize_start_ptr: Option<(f64, f64)>,
    pub resize_edges: Option<(bool, bool)>, // (drag_left, drag_top)
    pub tile_tree: Option<crate::tiling::TileNode>,

    // ── Рабочие столы (см. workspace.rs) ────────────────────────────────
    /// Данные неактивных столов; слот активного пуст — его данные в полях ниже.
    pub workspaces: Vec<crate::workspace::Workspace>,
    pub active_workspace: usize,
    /// Полноэкранное окно активного стола (см. fullscreen.rs).
    pub fullscreen: Option<crate::workspace::Fullscreen>,

    // ── Физический режим (см. sandbox.rs) ───────────────────────────────
    pub layout_mode: LayoutMode,
    /// Физический мир. Создаётся при первом входе в физику и живёт дальше:
    /// в тайлинге тела окон выключены, линии и связи сохраняются.
    pub physics: Option<crate::physics::WindowPhysics>,
    /// Куски окон (неразрезанное окно — один кусок).
    pub pieces: Vec<crate::sandbox::Piece>,
    /// Камера: экран = (мир − offset) · zoom.
    pub camera_offset: (f64, f64),
    pub target_camera_offset: (f64, f64),
    pub camera_zoom: f64,
    /// Камера физики на время тайлинга (offset x, y, zoom).
    pub saved_camera: (f64, f64, f64),
    // Панорамирование камеры удержанием средней кнопки мыши.
    pub camera_pan_active: bool,
    pub camera_pan_last_ptr: Option<(f64, f64)>,
    /// Super+ЛКМ тащит окно (механика — в WindowPhysics).
    pub dragging: bool,
    /// Удержание Super+A/D: -1 влево, +1 вправо, 0 — не крутим.
    pub spin_dir: f32,
    pub spin_piece: Option<u32>,
    pub tool: crate::sandbox::Tool,
    pub last_material: Option<crate::physics::Material>,
    pub transition: Option<crate::sandbox::Transition>,
    /// Размеры окон в физике на время тайлинга.
    pub physics_sizes: std::collections::HashMap<Window, Size<i32, Logical>>,
    pub last_physics_tick: Option<std::time::Instant>,
    /// Мягкие тела кусков (концепт §5): id куска → сетка.
    pub soft: std::collections::HashMap<u32, crate::sandbox::SoftPiece>,
    /// Захват края мягкого окна (Super+Shift+ЛКМ).
    pub soft_grab: Option<(u32, crate::softbody::SoftGrab)>,
    /// Снимки окон для отрисовки деформированной сеткой.
    pub snapshots: crate::mesh_render::SnapshotCache,
    /// Окно под указателем в момент нажатия кнопки: пока кнопка зажата,
    /// координаты считаются относительно него (выделение текста за краем).
    pub pointer_grab_target: Option<crate::sandbox::PointerTarget>,
    /// Кнопки, нажатия которых отданы клиентам (их отпускание — тоже клиентам).
    pub forwarded_buttons: std::collections::HashSet<u32>,
    /// Нажатие ушло поверхности слоя (панели): до отпускания указатель — ей.
    pub pointer_on_layer: bool,
    pub render_cache: crate::render::RenderCache,

    // ── Сессия (концепт §3) ─────────────────────────────────────────────
    pub session_loaded: bool,
    pub restored_floor_y: Option<f32>,
    pub autosave: Option<crate::session::Autosave>,
    pub restorer: Option<crate::session::SessionRestorer>,
    pub pending_joints: Vec<crate::session::SavedJoint>,
    /// Восстановленные окна сессии: индекс → (рабочий стол, куски).
    pub restored_windows: std::collections::HashMap<usize, (usize, Vec<u32>)>,
    /// Рабочий стол каждого сохранённого окна (индекс — как в restorer).
    pub restore_workspaces: Vec<usize>,
    /// Окна, для которых уже пробовали найти место в сессии.
    pub restore_tried: std::collections::HashSet<Window>,
    /// Команды из сессии, ждущие готовности XWayland.
    pub pending_relaunch: Vec<(Vec<String>, Option<String>)>,
    pub relaunch_deadline: Option<std::time::Instant>,

    // Tracks windows that have received a configure event but haven't committed
    // a new buffer yet. This prevents us from spamming configure events faster
    // than the client can process them (backpressure).
    pub pending_configures: std::collections::HashSet<Window>,

    // DMA-BUF global state for zero-copy buffer sharing.
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    pub dmabuf_global: Option<smithay::wayland::dmabuf::DmabufGlobal>,

    // TTY session (None in winit/nested mode)
    pub session: Option<LibSeatSession>,
    pub session_paused: bool,

    // Cursor
    pub cursor_status: CursorImageStatus,
    pub pointer_location: smithay::utils::Point<f64, smithay::utils::Logical>,
    /// Абсолютная позиция указателя хоста/планшета: из её изменений
    /// считается относительное движение (под захватом наш курсор стоит).
    pub host_pointer_location: Option<smithay::utils::Point<f64, smithay::utils::Logical>>,

    // Damage flag — the render loop only redraws when something actually changed
    // (window added/removed/focused/resized, surface committed, pointer moved).
    // Set to `true` at the relevant mutation points, reset after a successful frame.
    pub needs_render: bool,

    // Deferred layout flag — set when tile tree ratios change (resize, winit
    // window resize) so that recalculate_layout() runs exactly once per render
    // frame instead of on every mouse event (100-1000 Hz → ~60 Hz).
    pub layout_dirty: bool,
}

impl AppState {
    pub fn new(display_handle: DisplayHandle) -> Self {
        let config = crate::config::Config::load();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let _ = seat.add_keyboard(Default::default(), 200, 25);
        let _ = seat.add_pointer();

        let dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();
        let screencopy_state = crate::screencopy::ScreencopyState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let wlr_output_manager_state =
            crate::output_manager::OutputManagerState::new::<Self>(&display_handle);
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&display_handle);
        // wp_viewporter: клиенты (видео, браузеры) сами кадрируют/масштабируют
        // буферы; рендер-элементы Smithay учитывают viewport автоматически.
        let viewporter_state =
            smithay::wayland::viewporter::ViewporterState::new::<Self>(&display_handle);
        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let ext_workspace_state = crate::ext_workspace::ExtWorkspaceState::new(&display_handle);
        let workspaces = (0..config.workspaces.count)
            .map(|_| crate::workspace::Workspace::default())
            .collect();

        Self {
            display_handle,
            clock: Clock::new(),
            config,
            compositor_state,
            shm_state,
            xdg_shell_state,
            popup_manager: PopupManager::default(),
            output_manager_state,
            wlr_output_manager_state,
            viewporter_state,
            layer_shell_state,
            relative_pointer_state,
            pointer_constraints_state,
            ext_workspace_state,
            xwayland_shell_state,
            xwm: None,
            x11_stacking_dirty: false,
            xdisplay: None,
            seat_state,
            xdg_decoration_state,
            screencopy_state,
            data_device_state,
            primary_selection_state,
            space: Space::default(),
            seat,
            running: true,
            resize_window: None,
            resize_start_ptr: None,
            resize_edges: None,
            tile_tree: None,
            workspaces,
            active_workspace: 0,
            fullscreen: None,
            layout_mode: LayoutMode::default(),
            physics: None,
            pieces: Vec::new(),
            camera_offset: (0.0, 0.0),
            target_camera_offset: (0.0, 0.0),
            camera_zoom: 1.0,
            saved_camera: (0.0, 0.0, 1.0),
            camera_pan_active: false,
            camera_pan_last_ptr: None,
            dragging: false,
            spin_dir: 0.0,
            spin_piece: None,
            tool: crate::sandbox::Tool::None,
            last_material: None,
            transition: None,
            physics_sizes: Default::default(),
            last_physics_tick: None,
            pointer_grab_target: None,
            forwarded_buttons: Default::default(),
            pointer_on_layer: false,
            soft: Default::default(),
            soft_grab: None,
            snapshots: Default::default(),
            render_cache: Default::default(),
            session_loaded: false,
            restored_floor_y: None,
            autosave: None,
            restorer: None,
            pending_joints: Vec::new(),
            restored_windows: Default::default(),
            restore_workspaces: Vec::new(),
            restore_tried: Default::default(),
            pending_relaunch: Vec::new(),
            relaunch_deadline: None,
            pending_configures: std::collections::HashSet::new(),
            dmabuf_state,
            dmabuf_global: None,
            session: None,
            session_paused: false,
            cursor_status: CursorImageStatus::default_named(),
            pointer_location: (0.0, 0.0).into(),
            host_pointer_location: None,
            needs_render: true,
            layout_dirty: false,
        }
    }

    pub fn end_resize(&mut self) {
        if self.resize_window.is_none() {
            return;
        }
        self.resize_window = None;
        self.resize_start_ptr = None;
        self.resize_edges = None;
        self.recalculate_layout();
        self.needs_render = true;
    }

    /// Общая уборка окна (Wayland toplevel или X11): Space, тайлы, физика,
    /// фокус. Окно убирается из BSP-дерева в любом режиме — раньше в физике
    /// оно оставалось там, и после возврата в тайлинг закрытое окно занимало
    /// пустую ячейку (и даже заново попадало в Space).
    pub fn remove_window_common(&mut self, window: &Window) {
        // Окно со скрытого рабочего стола: убираем из его данных.
        if self.remove_hidden_window(window) {
            return;
        }
        let was_focused = self.get_focused_window().as_ref() == Some(window);
        if self.is_fullscreen(window) {
            self.fullscreen = None;
        }
        self.space.unmap_elem(window);
        self.pending_configures.remove(window);
        self.physics_remove_window(window);
        if self.resize_window.as_ref() == Some(window) {
            self.resize_window = None;
            self.resize_start_ptr = None;
            self.resize_edges = None;
        }
        if let Some(tree) = self.tile_tree.take() {
            self.tile_tree = tree.remove(window);
        }
        if self.layout_mode == LayoutMode::Tiling {
            self.recalculate_layout();
        }

        // Фокус у панели/лаунчера не отнимаем.
        let layer_focused = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|f| f.surface())
            .is_some_and(|f| self.any_window_for_surface(&f).is_none());
        if was_focused || (self.get_focused_window().is_none() && !layer_focused) {
            let next_win = self.fullscreen_window().or_else(|| {
                self.space
                    .elements()
                    .rev()
                    .find(|w| !w.x11_surface().map_or(false, |x| x.is_override_redirect()))
                    .cloned()
            });
            if let Some(win) = next_win {
                self.focus_window(&win);
            }
        }
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

    /// Цвет фона из `[rendering] background`.
    pub fn background_color(&self) -> [f32; 4] {
        crate::config::parse_color(&self.config.rendering.background)
    }

    /// Область тайлинга: экран без панелей (layer-shell) и внешних отступов.
    pub fn tiling_area(&self) -> Rectangle<i32, Logical> {
        let usable = self.usable_area();
        Rectangle::new(
            (usable.loc.x + GAPS_OUT, usable.loc.y + GAPS_OUT).into(),
            (
                (usable.size.w - GAPS_OUT * 2).max(1),
                (usable.size.h - GAPS_OUT * 2).max(1),
            )
                .into(),
        )
    }

    /// Тайловый прямоугольник окна (как его расставит `recalculate_layout`).
    pub fn tile_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let cell = self
            .tile_tree
            .as_ref()?
            .area_for(window, self.tiling_area())?;
        Some(crate::tiling::shrink(cell, GAPS_IN))
    }

    /// BSP tiling layout calculation.
    pub fn recalculate_layout(&mut self) {
        if self.layout_mode != LayoutMode::Tiling {
            return;
        }
        let Some(tree) = &self.tile_tree else {
            return;
        };
        let mut rects = Vec::new();
        tree.collect_rects(self.tiling_area(), GAPS_IN, &mut rects);
        // map_element поднимает окно наверх стека — сохраняем порядок, чтобы
        // перерасчёт не прятал X11-меню под окна и не терял поднятое окно.
        let order: Vec<Window> = self.space.elements().cloned().collect();
        let mut moved = false;
        for (win, rect) in rects {
            // Полноэкранное окно живёт вне тайлов.
            if self.is_fullscreen(&win) {
                continue;
            }
            moved |= self.apply_window_geometry(&win, rect);
        }
        if moved {
            for w in &order {
                self.space.raise_element(w, false);
            }
        }
        self.place_fullscreen_window();
    }

    /// Где поставить окно в тайле `rect`. Пока клиент не перерисовался в
    /// новый размер, окно прижато к неподвижному краю тайла (ресайз за
    /// левый/верхний край без рывков); как только размер совпал — ровно в
    /// тайл. Раньше прижатое положение так и оставалось после коммита: окно,
    /// занявшее место закрытого соседа слева, уезжало за правый край экрана.
    fn tile_location(&self, window: &Window, rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
        let current = window.geometry().size;
        let mut loc = rect.loc;
        if current == rect.size {
            return loc;
        }
        if let Some(old_geo) = self.space.element_geometry(window) {
            let old_right = old_geo.loc.x + old_geo.size.w;
            let old_bottom = old_geo.loc.y + old_geo.size.h;
            let target_right = rect.loc.x + rect.size.w;
            let target_bottom = rect.loc.y + rect.size.h;
            if (target_right - old_right).abs() < 5 && rect.loc.x != old_geo.loc.x {
                loc.x = target_right - current.w;
            }
            if (target_bottom - old_bottom).abs() < 5 && rect.loc.y != old_geo.loc.y {
                loc.y = target_bottom - current.h;
            }
        }
        loc
    }

    /// Коммит окна в тайлинге: после смены размера поставить его ровно в
    /// свой тайл (см. [`Self::tile_location`]).
    fn reposition_tiled_window(&mut self, window: &Window) {
        if self.layout_mode != LayoutMode::Tiling
            || self.is_fullscreen(window)
            || window
                .x11_surface()
                .is_some_and(|x| x.is_override_redirect())
        {
            return;
        }
        let Some(rect) = self.tile_rect(window) else {
            return;
        };
        let loc = self.tile_location(window, rect);
        if self.space.element_location(window) == Some(loc) {
            return;
        }
        let order: Vec<Window> = self.space.elements().cloned().collect();
        self.space.map_element(window.clone(), loc, false);
        for w in &order {
            self.space.raise_element(w, false);
        }
        if let Some(x11) = window.x11_surface() {
            let target = Rectangle::new(loc, rect.size);
            if x11.geometry() != target {
                let _ = x11.configure(target);
            }
        }
        self.needs_render = true;
    }

    /// Ставит окно в прямоугольник тайла. Возвращает true, если окно
    /// перемещалось в Space (порядок стека нужно восстановить).
    fn apply_window_geometry(&mut self, window: &Window, rect: Rectangle<i32, Logical>) -> bool {
        if !self.space.elements().any(|w| w == window) {
            // Окно уже закрыто/размаплено — не возвращаем его в Space.
            return false;
        }
        let loc = self.tile_location(window, rect);

        let moved = self.space.element_location(window) != Some(loc);
        if moved {
            self.space.map_element(window.clone(), loc, false);
        }

        // Ask the client to resize its surface — but only if the size actually
        // changed. Sending a configure with the same size on every relayout used
        // to trigger an O(N²) "configure storm".
        if let Some(toplevel) = window.toplevel() {
            let mut changed = false;
            toplevel.with_pending_state(|state| {
                if state.size != Some(rect.size) {
                    state.size = Some(rect.size);
                    changed = true;
                }
            });
            // Backpressure: only send a new configure if the client has
            // already processed and committed the previous one.
            if changed && !self.pending_configures.contains(window) {
                toplevel.send_configure();
                self.pending_configures.insert(window.clone());
            }
        }

        // X11-окна (XWayland): размер/позиция задаются напрямую через configure.
        if let Some(x11) = window.x11_surface() {
            let target = Rectangle::new(loc, rect.size);
            if x11.geometry() != target {
                let _ = x11.configure(target);
            }
        }
        moved
    }

    /// Synchronize Smithay's visual Space order with XWayland's internal
    /// X11 stacking order. Steam/CEF uses many override-redirect popup windows;
    /// if the two stacks diverge, a popup may be visible on top while X11 sends
    /// the click to the main window underneath it.
    ///
    /// X11-окна скрытых рабочих столов в X-сервере остаются отображёнными —
    /// они уходят в самый низ стека. Иначе XWayland, выбирающий окно для
    /// клика по X-стеку, отдавал клик окну с другого стола (или окну под
    /// полноэкранной игрой).
    ///
    /// Порядок для `update_stacking_order_downwards` — СВЕРХУ ВНИЗ: так
    /// работает его код (документация Smithay утверждает обратное). Раньше
    /// сюда шёл порядок Space снизу вверх, и X-стек получался перевёрнутым:
    /// окно в фокусе (и полноэкранная игра) оказывалось в X внизу, а клики
    /// уходили окнам, лежащим под ним.
    pub fn sync_x11_stacking(&mut self) {
        let visible = self
            .space
            .elements()
            .rev()
            .filter_map(|w| w.x11_surface().cloned());
        let hidden = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.windows.iter())
            .filter_map(|w| w.x11_surface().cloned());
        let order: Vec<smithay::xwayland::X11Surface> = visible.chain(hidden).collect();
        if let Some(xwm) = self.xwm.as_mut() {
            let _ = xwm.update_stacking_order_downwards(order.iter());
        }
    }

    /// Set keyboard focus to a specific normal toplevel window.
    pub fn focus_window(&mut self, window: &Window) {
        // X11 override-redirect windows (Steam/CEF menus, tooltips and
        // notifications) manage their own focus/grabs. Treating them as normal
        // toplevels generates FocusOut before ButtonPress and makes clicks pass
        // through the popup.
        if window
            .x11_surface()
            .map_or(false, |x| x.is_override_redirect())
        {
            return;
        }
        // Фокус ушёл на другое окно (Super+стрелки, перенос) — полноэкранное
        // окно возвращается в обычный размер, иначе фокус был бы невидим.
        if let Some(fs) = self.fullscreen_window() {
            if &fs != window {
                self.set_fullscreen(&fs, false);
            }
        }

        let already_focused = self
            .get_focused_window()
            .as_ref()
            .map_or(false, |focused| focused == window);

        // Raise visually first, then mirror the complete order to X11. Raising
        // just the target with X11Wm::raise_window loses CEF popup ordering.
        self.space.raise_element(window, true);
        if already_focused {
            self.sync_x11_stacking();
            self.needs_render = true;
            return;
        }

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for w in &windows {
            let is_target = w == window;
            if let Some(toplevel) = w.toplevel() {
                let mut changed = false;
                toplevel.with_pending_state(|state| {
                    let already = state.states.contains(xdg_toplevel::State::Activated);
                    if is_target && !already {
                        state.states.set(xdg_toplevel::State::Activated);
                        changed = true;
                    } else if !is_target && already {
                        state.states.unset(xdg_toplevel::State::Activated);
                        changed = true;
                    }
                });
                w.set_activated(is_target);
                if changed && toplevel.is_initial_configure_sent() {
                    toplevel.send_configure();
                }
            } else if let Some(x11) = w.x11_surface() {
                // Never deactivate OR popups. Steam's locally-active focus
                // model closes or defocuses them before the pending click.
                if x11.is_override_redirect() {
                    continue;
                }
                let _ = x11.set_activated(is_target);
                w.set_activated(is_target);
            }
        }

        // X11-окну фокус ставится и в X-сервере (SetInputFocus/WM_TAKE_FOCUS
        // по его ICCCM-модели), см. focus.rs; Wayland-окну — его поверхности.
        if let Some(target) = crate::focus::KeyboardFocusTarget::for_window(window) {
            if let Some(kbd) = self.seat.get_keyboard() {
                kbd.set_focus(self, Some(target), serial);
            }
        }

        self.sync_x11_stacking();
        self.needs_render = true;
    }

    /// Move focus to the next or previous window in the stack.
    pub fn focus_next(&mut self, forward: bool) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !w.x11_surface().map_or(false, |x| x.is_override_redirect()))
            .cloned()
            .collect();
        if windows.is_empty() {
            return;
        }
        // Порядок по дереву тайлов стабилен (стек Space меняется при фокусе).
        let ordered: Vec<Window> = match &self.tile_tree {
            Some(tree) => {
                let mut v: Vec<Window> = tree
                    .windows()
                    .into_iter()
                    .filter(|w| windows.contains(w))
                    .collect();
                let rest: Vec<Window> =
                    windows.iter().filter(|w| !v.contains(w)).cloned().collect();
                v.extend(rest);
                v
            }
            None => windows,
        };
        let current = self.get_focused_window();
        let next_idx = match current.and_then(|c| ordered.iter().position(|w| *w == c)) {
            Some(idx) if forward => (idx + 1) % ordered.len(),
            Some(idx) => (idx + ordered.len() - 1) % ordered.len(),
            None => 0,
        };
        let target = ordered[next_idx].clone();
        self.focus_window(&target);
    }

    /// Фокус на ближайшее окно в направлении `(dx, dy)` (тайлинг, Super+стрелки).
    /// Если в ту сторону окон нет — по кругу, как раньше.
    pub fn focus_direction(&mut self, dx: f64, dy: f64) {
        let Some(current) = self.get_focused_window() else {
            self.focus_next(true);
            return;
        };
        let Some(cur) = self.space.element_geometry(&current) else {
            return;
        };
        let center = |r: Rectangle<i32, Logical>| {
            (
                r.loc.x as f64 + r.size.w as f64 * 0.5,
                r.loc.y as f64 + r.size.h as f64 * 0.5,
            )
        };
        let (cx, cy) = center(cur);
        let best = self
            .space
            .elements()
            .filter(|w| **w != current)
            .filter(|w| !w.x11_surface().map_or(false, |x| x.is_override_redirect()))
            .filter_map(|w| {
                let (x, y) = center(self.space.element_geometry(w)?);
                let along = (x - cx) * dx + (y - cy) * dy;
                let across = ((x - cx) * dy - (y - cy) * dx).abs();
                // Окна «впереди»; боковое смещение штрафуется сильнее.
                (along > 1.0).then(|| (w.clone(), along + across * 2.0))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(w, _)| w);
        match best {
            Some(w) => self.focus_window(&w),
            None => self.focus_next(dx + dy > 0.0),
        }
    }

    /// Get currently focused window
    pub fn get_focused_window(&self) -> Option<Window> {
        let kbd = self.seat.get_keyboard()?;
        let focus = kbd.current_focus()?;
        self.space.elements().find(|w| focus.is_window(w)).cloned()
    }

    /// Окно, которому принадлежит wl_surface (toplevel или X11).
    fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }

    /// Подгоняет попап под экран (в тайлинге) или просто берёт геометрию
    /// позиционера (в физике окна повёрнуты — «экран» в координатах окна
    /// не определён).
    fn position_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let target = (|| {
            if self.layout_mode != LayoutMode::Tiling {
                return None;
            }
            let root = find_popup_root_surface(&kind).ok()?;
            let window = self.window_for_surface(&root)?;
            let window_geo = self.space.element_geometry(&window)?;
            let output = self.space.outputs().next()?;
            let mut target = self.space.output_geometry(output)?;
            target.loc -= get_popup_toplevel_coords(&kind);
            target.loc -= window_geo.loc;
            Some(target)
        })();
        popup.with_pending_state(|state| {
            state.geometry = match target {
                Some(t) => state.positioner.get_unconstrained_geometry(t),
                None => state.positioner.get_geometry(),
            };
        });
    }
}

// 1. Compositor
impl CompositorHandler for AppState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Обычные Wayland-клиенты несут ClientState, а клиент XWayland —
        // XWaylandClientData (его создаёт smithay при XWayland::spawn).
        // Раньше unwrap() здесь ронял композитор (SIGABRT в global_bind),
        // как только Xwayland подключался к композитору.
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Поверхность курсора: wl_surface.offset сдвигает hotspot.
        if let CursorImageStatus::Surface(cursor) = &self.cursor_status {
            if cursor == surface {
                crate::cursor::apply_hotspot_delta(surface);
                self.needs_render = true;
            }
        }
        self.popup_manager.commit(surface);
        self.popup_manager.cleanup();
        // Обновляем буферы рендера
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Панель/фон/лаунчер (layer-shell).
        if self.layer_commit(surface) {
            return;
        }

        // Найдём окно, которому принадлежит эта поверхность, и обновим его bbox.
        // ВАЖНО: recalculate_layout() здесь НЕ вызываем — это создаёт петлю
        // commit → configure → commit → ... (~30–40% CPU на простое рабочем столе,
        // т.к. мигание курсора терминала триггерит commit). Тайловые позиции
        // выставляются в new_toplevel / toplevel_destroyed / handle_pointer_motion.
        let mut found = None;
        for window in self.space.elements() {
            if window.wl_surface().as_ref().map(|s| s.as_ref()) == Some(surface) {
                found = Some(window.clone());
                break;
            }
        }
        if found.is_none() {
            // Окно скрытого рабочего стола: на экране его нет, но ответ на
            // configure засчитываем, иначе после возврата ему не уйдёт новый.
            if let Some(window) =
                self.find_hidden_window(|w| w.wl_surface().as_deref() == Some(surface))
            {
                window.on_commit();
                self.pending_configures.remove(&window);
                return;
            }
        }
        if let Some(window) = found {
            window.on_commit();
            // Client has committed a buffer, meaning it has processed the last
            // configure event. We can send new configure events to it now.
            self.pending_configures.remove(&window);
            if self.x11_stacking_dirty && window.x11_surface().is_some() {
                self.x11_stacking_dirty = false;
                self.sync_x11_stacking();
            }
            // Размер, отложенный backpressure-ом (например, сосед закрылся,
            // пока клиент ещё не ответил на прошлый configure), уходит сейчас:
            // иначе окно так и оставалось старого размера.
            if let Some(toplevel) = window.toplevel() {
                if toplevel.is_initial_configure_sent() && toplevel.has_pending_changes() {
                    toplevel.send_configure();
                    self.pending_configures.insert(window.clone());
                }
            }
            // Окно перерисовалось в размер тайла — ставим его ровно в тайл.
            self.reposition_tiled_window(&window);

            let is_or = window
                .x11_surface()
                .map_or(false, |x| x.is_override_redirect());
            let size = window.geometry().size;
            if !is_or && size.w > 1 && size.h > 1 {
                if self.pieces_of(&window).next().is_none() {
                    // Первый настоящий буфер: в физике окно получает тело
                    // (или сохранённое место из сессии); в тайлинге —
                    // только сохранённое место на будущее.
                    if self.layout_mode == LayoutMode::Physics && self.transition.is_none() {
                        self.physics_spawn_window(&window);
                    } else if self.restorer.is_some() {
                        self.restore_window_for_later(&window);
                    }
                } else {
                    // Клиент сменил размер — коллайдер следует за окном.
                    self.physics_window_resized(&window);
                }
            }

            // During active resize: do NOT call apply_window_geometry or
            // recalculate_layout here (commit → configure → commit loop).
        }

        // Новый буфер — экран нужно перерисовать.
        self.needs_render = true;
    }
}

smithay::delegate_compositor!(AppState);

// 2. Buffer & SHM
impl BufferHandler for AppState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl smithay::wayland::dmabuf::DmabufHandler for AppState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        // We accept all dmabufs. The actual GPU import is done lazily by the GLES
        // renderer's EGL buffer reader (set up via bind_wl_display) when the
        // client attaches this buffer to a surface and commits it — so there is
        // no renderer to test against here.
        let _ = notifier.successful::<AppState>();
    }
}

impl ShmHandler for AppState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
smithay::delegate_shm!(AppState);

// 2.5 Output
impl OutputHandler for AppState {
    fn output_bound(
        &mut self,
        _output: smithay::output::Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        self.ext_workspace_output_bound(&wl_output);
    }
}
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
        // Пока кладём наверху (будет перемещён layout-ом / физикой)
        self.space.map_element(window.clone(), (0, 0), true);

        // Обновляем BSP дерево ВСЕГДА, чтобы при возврате из физического режима
        // окно корректно стало в тайл.
        let focused = self.get_focused_window();
        if let Some(tree) = self.tile_tree.take() {
            let target = focused.unwrap_or_else(|| window.clone());
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.tiling_area()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        // В физике тело появится в commit(), когда клиент впервые отрисует окно.
        if self.layout_mode == LayoutMode::Tiling {
            self.recalculate_layout();
        }

        // Передаём клавиатурный фокус новому окну. Поверх полноэкранного
        // (игра) новое окно не выскакивает: остаётся под ним без фокуса.
        if self.fullscreen_window().is_some() {
            self.place_fullscreen_window();
        } else {
            self.focus_window(&window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        if let Some(window) = window {
            self.remove_window_common(&window);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        match window {
            Some(window) => self.set_fullscreen(&window, true),
            None => {
                // До map: просто подтверждаем состояние (окно встанет в тайл).
                surface.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                });
                if surface.is_initial_configure_sent() {
                    surface.send_configure();
                }
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let is_it = |w: &Window| w.toplevel().map(|t| t == &surface).unwrap_or(false);
        let window = self
            .space
            .elements()
            .find(|w| is_it(w))
            .cloned()
            .or_else(|| self.find_hidden_window(is_it));
        if let Some(window) = window {
            self.set_fullscreen(&window, false);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Раньше геометрия попапа не задавалась вовсе: меню GTK/Qt получали
        // configure с нулевым прямоугольником и появлялись в углу окна.
        self.position_popup(&surface);
        let _ = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()));
        let _ = surface.send_configure();
        self.needs_render = true;
    }

    /// Захват для меню: клавиатура и указатель уходят попапу, клик мимо
    /// закрывает цепочку меню (раньше grab игнорировался — меню не
    /// закрывались по клику снаружи).
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Some(root) = find_popup_root_surface(&kind)
            .ok()
            .and_then(|root| self.window_for_surface(&root))
            .and_then(|w| w.wl_surface().map(|s| s.into_owned()))
        else {
            return;
        };
        let root = crate::focus::KeyboardFocusTarget::Wayland(root);
        let Ok(mut grab) = self.popup_manager.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        self.position_popup(&surface);
        surface.send_repositioned(token);
        let _ = surface.send_configure();
        self.needs_render = true;
    }
}
smithay::delegate_xdg_shell!(AppState);

// XDG Decoration — сообщаем клиентам использовать серверные декорации (без CSD)
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

impl XdgDecorationHandler for AppState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

impl AsMut<crate::screencopy::ScreencopyState> for AppState {
    fn as_mut(&mut self) -> &mut crate::screencopy::ScreencopyState {
        &mut self.screencopy_state
    }
}

impl AsMut<crate::output_manager::OutputManagerState> for AppState {
    fn as_mut(&mut self) -> &mut crate::output_manager::OutputManagerState {
        &mut self.wlr_output_manager_state
    }
}

impl crate::output_manager::OutputManagerHandler for AppState {
    fn outputs(
        &self,
    ) -> Vec<(
        smithay::output::Output,
        Option<smithay::utils::Point<i32, smithay::utils::Logical>>,
    )> {
        self.space
            .outputs()
            .map(|o| {
                (
                    o.clone(),
                    Some(
                        self.space
                            .output_geometry(o)
                            .unwrap_or(smithay::utils::Rectangle::from_size((0, 0).into()))
                            .loc,
                    ),
                )
            })
            .collect()
    }
}

smithay::delegate_xdg_decoration!(AppState);
smithay::delegate_viewporter!(AppState);
crate::delegate_screencopy!(AppState);
crate::delegate_output_manager!(AppState);

// 4. Seat / Input
impl SeatHandler for AppState {
    type KeyboardFocus = crate::focus::KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(
        &mut self,
        seat: &Seat<Self>,
        focused: Option<&crate::focus::KeyboardFocusTarget>,
    ) {
        let focused = focused.and_then(|f| f.surface());
        let focused = focused.as_ref();
        // Буфер обмена привязан к клиенту с клавиатурным фокусом: без этого
        // клиент не получает wl_data_offer и копирование/вставка не работают.
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
        // Клавиатура ушла с игры — захват мыши снимается, иначе
        // заблокированный курсор так и не покинул бы её окно.
        if let Some(pointer) = seat.get_pointer() {
            if let Some(surface) = pointer.current_focus() {
                if focused != Some(&surface) {
                    with_pointer_constraint(&surface, &pointer, |constraint| {
                        if let Some(c) = constraint {
                            if c.is_active() {
                                c.deactivate();
                            }
                        }
                    });
                }
            }
        }
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.needs_render = true;
    }
}
smithay::delegate_seat!(AppState);
smithay::delegate_dmabuf!(AppState);

// 5. Буфер обмена: wl_data_device_manager + primary selection
impl SelectionHandler for AppState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for AppState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for AppState {}
impl ServerDndGrabHandler for AppState {}

impl PrimarySelectionHandler for AppState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

smithay::delegate_data_device!(AppState);
smithay::delegate_primary_selection!(AppState);

// 6. Игры: относительное движение мыши и захват указателя (FPS-камера).
impl PointerConstraintsHandler for AppState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Полноэкранная игра закрывает весь экран: указатель переводим на неё
        // сразу, не дожидаясь движения мыши. Иначе захват ждал бы движения,
        // а до тех пор XWayland не мог эмулировать возврат курсора в центр —
        // игра видела смещение к нашему курсору и камеру «тянуло» в одну точку.
        let fullscreen = self
            .fullscreen_window()
            .is_some_and(|w| w.wl_surface().as_deref() == Some(surface));
        if fullscreen && pointer.current_focus().as_ref() != Some(surface) {
            crate::input::refresh_pointer_focus(self);
        }
        // Иначе захват включается, только когда указатель уже над
        // поверхностью, или при входе на неё (см. input.rs).
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        });
        if !active {
            return;
        }
        if let Some(origin) = self.surface_screen_origin(surface) {
            let pos = origin + location;
            pointer.set_location(pos);
            self.pointer_location = pos;
            self.needs_render = true;
        }
    }
}
smithay::delegate_pointer_constraints!(AppState);
smithay::delegate_relative_pointer!(AppState);

impl AppState {
    /// Где на экране начало поверхности окна (тайлинг или полноэкранное
    /// окно; в физике окна повёрнуты/масштабированы — `None`).
    pub fn surface_screen_origin(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let window = self.window_for_surface(surface)?;
        let geo_loc = window.geometry().loc;
        if self.is_fullscreen(&window) {
            return Some((Point::<i32, Logical>::default() - geo_loc).to_f64());
        }
        if self.layout_mode != LayoutMode::Tiling {
            return None;
        }
        let loc = self.space.element_location(&window)?;
        Some((loc - geo_loc).to_f64())
    }
}
