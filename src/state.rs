//! Состояние композитора: [`AppState`] со всеми глобалами Smithay и
//! данными WM, и общая уборка окон.
//!
//! Обработчики протоколов — в [`crate::handlers`], ввод — в [`crate::input`],
//! раскладка — в [`crate::layout`], фокус — в [`crate::focus`].

use smithay::{
    backend::session::libseat::LibSeatSession,
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    reexports::calloop::LoopHandle,
    reexports::wayland_server::{
        DisplayHandle,
        backend::{ClientData, ClientId, DisconnectReason, GlobalId},
        protocol::wl_surface::WlSurface,
    },
    utils::{Clock, Logical, Monotonic, Rectangle, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        drm_syncobj::DrmSyncobjState,
        output::OutputManagerState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
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

pub struct AppState {
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, AppState>,
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
    /// Относительное движение мыши и захват указателя — для игр (см.
    /// `input::constraints`). `None`, если `[input] pointer_lock = false`.
    pub relative_pointer_state: Option<RelativePointerManagerState>,
    pub pointer_constraints_global: Option<GlobalId>,
    /// ext-workspace-v1: рабочие столы для панелей.
    pub ext_workspace_state: crate::ext_workspace::ExtWorkspaceState,
    /// linux-drm-syncobj-v1 (явная синхронизация, см. `handlers::sync`).
    /// Включает бэкенд, когда знает DRM-устройство.
    pub syncobj_state: Option<DrmSyncobjState>,

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
    /// Поверхности с новым захватом указателя, ждущие включения (см.
    /// PointerConstraintsHandler::new_constraint).
    pub pending_constraints: Vec<WlSurface>,
    /// Фокус, отданный Smithay последним `motion`: поверхность и её начало
    /// (см. `input::pointer::send_motion`).
    pub pointer_focus: crate::input::pointer::PointerFocus,
    /// Фокус указателя пересчитать после обработки клиентов (под курсором
    /// появилось или пропало окно) — см. `queue_pointer_refresh`.
    pub pointer_refresh_pending: bool,
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
    pub fn new(display_handle: DisplayHandle, loop_handle: LoopHandle<'static, AppState>) -> Self {
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
        // Без этих глобалов XWayland работает с абсолютным курсором (как до
        // рабочих столов) — см. `[input] pointer_lock`.
        let (relative_pointer_state, pointer_constraints_global) = if config.input.pointer_lock {
            (
                Some(RelativePointerManagerState::new::<Self>(&display_handle)),
                Some(crate::input::constraints::create_global(
                    &display_handle,
                    config.input.xwayland_pointer_lock,
                )),
            )
        } else {
            (None, None)
        };
        let ext_workspace_state = crate::ext_workspace::ExtWorkspaceState::new(&display_handle);
        let workspaces = (0..config.workspaces.count)
            .map(|_| crate::workspace::Workspace::default())
            .collect();

        Self {
            display_handle,
            loop_handle,
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
            pointer_constraints_global,
            ext_workspace_state,
            syncobj_state: None,
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
            pending_constraints: Vec::new(),
            pointer_focus: None,
            pointer_refresh_pending: false,
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
        // Под курсором теперь другое окно.
        self.queue_pointer_refresh();
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

    /// Окно, которому принадлежит wl_surface (toplevel или X11).
    pub(crate) fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }
}
