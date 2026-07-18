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
            decoration::{XdgDecorationState, XdgDecorationHandler},
        },
        output::{OutputHandler, OutputManagerState},
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

pub struct AppState {
    pub display_handle: DisplayHandle,
    pub clock: Clock<Monotonic>,

    // Smithay states
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub xdg_decoration_state: XdgDecorationState,

    // WM states
    pub space: Space<Window>,
    pub seat: Seat<Self>,
    pub running: bool,

    // Tiling / resize state
    pub resize_window: Option<Window>,
    pub resize_start_ptr: Option<(f64, f64)>,
    pub resize_edges: Option<(bool, bool)>, // (drag_left, drag_top)
    pub tile_tree: Option<crate::tiling::TileNode>,

    // ── Физический режим (Phase 1) ──────────────────────────────────────
    // Текущий режим раскладки. Tiling — жёсткий тайлинг (поведение до Phase 1,
    // безопасный откат). Physics — окна становятся динамическими телами rapier2d
    // на бесконечном холсте с гравитацией и полом.
    pub layout_mode: LayoutMode,
    // Физический мир; None в Tiling-режиме, инициализируется при переключении.
    pub physics: Option<crate::physics::WindowPhysics>,
    // Связь окна Smithay ↔ тело rapier. Window — клонируемый Rc-хендл
    // (Hash+Eq), поэтому работает как ключ HashMap.
    pub window_bodies: std::collections::HashMap<Window, rapier2d::prelude::RigidBodyHandle>,
    // Смещение камеры в мировых координатах. Камера реализуется через
    // map_output(&output, (-cam_x, -cam_y)) — Smithay сам конвертирует мир→экран.
    pub camera_offset: (f64, f64),
    pub target_camera_offset: (f64, f64),
    // Окно, которое сейчас таскают мышью в физическом режиме, и его тело.
    pub drag_body: Option<(Window, rapier2d::prelude::RigidBodyHandle)>,
    pub drag_last_ptr: Option<(f64, f64)>,
    // Таймстеп симуляции (совпадает с тем, что задан в physics.set_dt()).
    // Нужен в step_physics для расчёта скорости drag_to.
    pub physics_dt: f32,
    
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

    // Damage flag — the render loop only redraws when something actually changed
    // (window added/removed/focused/resized, surface committed, pointer moved).
    // Set to `true` at the relevant mutation points, reset after a successful frame.
    pub needs_render: bool,

    // Deferred layout flag — set when tile tree ratios change (resize, winit
    // window resize) so that recalculate_layout() runs exactly once per render
    // frame instead of on every mouse event (100-1000 Hz → ~60 Hz).
    pub layout_dirty: bool,

    // Throttle for live resizing to prevent terminal CPU spikes.
    pub last_resize_time: std::time::Instant,
}

impl AppState {
    pub fn new(display_handle: DisplayHandle) -> Self {
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

        Self {
            display_handle,
            clock: Clock::new(),
            compositor_state,
            shm_state,
            xdg_shell_state,
            output_manager_state,
            seat_state,
            xdg_decoration_state,
            space: Space::default(),
            seat,
            running: true,
            resize_window: None,
            resize_start_ptr: None,
            resize_edges: None,
            tile_tree: None,
            layout_mode: LayoutMode::default(),
            physics: None,
            window_bodies: std::collections::HashMap::new(),
            camera_offset: (0.0, 0.0),
            target_camera_offset: (0.0, 0.0),
            drag_body: None,
            drag_last_ptr: None,
            physics_dt: 1.0 / 60.0,
            pending_configures: std::collections::HashSet::new(),
            dmabuf_state,
            dmabuf_global: None,
            session: None,
            session_paused: false,
            cursor_status: CursorImageStatus::default_named(),
            pointer_location: (0.0, 0.0).into(),
            needs_render: true,
            layout_dirty: false,
            last_resize_time: std::time::Instant::now(),
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

    // ── Физический режим (Phase 1) ──────────────────────────────────────

    /// Переключает между Tiling и Physics. Вызывается по Super+G.
    pub fn toggle_physics(&mut self) {
        match self.layout_mode {
            LayoutMode::Tiling => self.enter_physics_mode(),
            LayoutMode::Physics => self.exit_physics_mode(),
        }
        self.needs_render = true;
    }

    /// Tiling → Physics. Создаёт физический мир и спавнит тело для каждого
    /// уже отрисованного окна по его текущей позиции/размеру в Space.
    fn enter_physics_mode(&mut self) {
        let mut phys = crate::physics::WindowPhysics::new();
        phys.set_dt(self.physics_dt);
        self.window_bodies.clear();

        // Снимаем снапшот геометрий до borrow phys.
        let geoms: Vec<(Window, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|w| {
                let geo = self.space.element_geometry(w)?;
                Some((w.clone(), geo))
            })
            .collect();

        for (win, geo) in geoms {
            // Центр окна в мировых координатах. Камера сейчас (0,0), поэтому
            // мировые координаты = экранные на момент переключения.
            let cx = geo.loc.x as f32 + geo.size.w as f32 * 0.5;
            let cy = geo.loc.y as f32 + geo.size.h as f32 * 0.5;
            let handle = phys.spawn_window(cx, cy, geo.size.w as f32, geo.size.h as f32);
            self.window_bodies.insert(win, handle);
        }

        self.physics = Some(phys);
        self.layout_mode = LayoutMode::Physics;
        self.drag_body = None;
        self.drag_last_ptr = None;
        println!("[physics] режим включён, {} окон", self.window_bodies.len());
    }

    /// Physics → Tiling. Убирает все тела и возвращается к жёсткому тайлингу.
    fn exit_physics_mode(&mut self) {
        self.physics = None;
        self.window_bodies.clear();
        self.drag_body = None;
        self.drag_last_ptr = None;
        self.camera_offset = (0.0, 0.0);
        self.target_camera_offset = (0.0, 0.0);
        self.layout_mode = LayoutMode::Tiling;
        // Пересчитываем тайлы — окна встанут на свои места.
        self.recalculate_layout();
        println!("[physics] режим выключен, возврат к тайлингу");
    }

    /// Один шаг физики + применение трансформ тел к окнам. Вызывается из
    /// рендер-цикла (~60 Hz) только в физическом режиме.
    pub fn step_physics(&mut self) {
        let Some(phys) = self.physics.as_mut() else {
            return;
        };
        // Если таскаем тело мышью — двигаем его к курсору через скорость
        // (drag_to), а не телепортом. Телепорт динамического тела каждый
        // кадр заставлял solver разрешать внезапное проникновение в пол/
        // соседние окна как жёсткий контакт — отсюда были рывки при drag,
        // даже когда CPU/RAM были в норме.
        let dt = self.physics_dt;
        if let Some((_, handle)) = self.drag_body {
            if let Some((x, y)) = self.drag_last_ptr {
                phys.drag_to(handle, x as f32, y as f32, dt);
            }
        }
        let step_dur = phys.step();
        
        // --- ДИАГНОСТИКА ЛАГОВ ---
        // Печатаем только если шаг занял больше 2мс или идет drag, чтобы не спамить в 60fps
        if step_dur.as_millis() > 2 || self.drag_body.is_some() {
            eprintln!("[DEBUG] phys.step() took {:?} | bodies: {}", step_dur, phys.body_count());
        }
        // Держим needs_render поднятым, пока хоть одно тело двигается — иначе
        // рендер-цикл уснёт и симуляция застынет. Когда всё улеглось, рендер
        // засыпает (это и нужно для энергосбережения на idle столе).
        let mut moving = phys.any_moving() || self.drag_body.is_some();
        
        // Плавная камера (Lerp)
        let cam_dx = self.target_camera_offset.0 - self.camera_offset.0;
        let cam_dy = self.target_camera_offset.1 - self.camera_offset.1;
        if cam_dx * cam_dx + cam_dy * cam_dy > 0.5 {
            self.camera_offset.0 += cam_dx * 0.15;
            self.camera_offset.1 += cam_dy * 0.15;
            moving = true; // продолжаем форсировать 60 FPS, пока камера едет
        } else {
            self.camera_offset = self.target_camera_offset;
        }

        self.apply_physics_layout();
        if moving {
            self.needs_render = true;
        }
    }

    /// Читает трансформы всех тел и применяет их к окнам через существующий
    /// apply_window_geometry (он делает map_element + backpressured configure).
    fn apply_physics_layout(&mut self) {
        // Собираем (окно, трансформ, размер) полностью, отпуская borrow phys,
        // и только потом мутируем self через apply_window_geometry.
        let transforms: Vec<(Window, (f32, f32), smithay::utils::Size<i32, Logical>)> = {
            let Some(phys) = self.physics.as_ref() else {
                return;
            };
            self.window_bodies
                .iter()
                .filter_map(|(win, handle)| {
                    let (cx, cy, _angle) = phys.body_transform(*handle)?;
                    let size = self
                        .space
                        .element_geometry(win)
                        .map(|g| g.size)
                        .unwrap_or_else(|| (100, 100).into());
                    Some((win.clone(), (cx, cy), size))
                })
                .collect()
        };
        for (win, (cx, cy), size) in transforms {
            // Позиция тела = центр; переводим в top-left для Space.
            let loc = (
                (cx - size.w as f32 * 0.5).round() as i32,
                (cy - size.h as f32 * 0.5).round() as i32,
            ).into();
            let rect = Rectangle::new(loc, size);
            self.apply_window_geometry(&win, rect);
        }
    }

    /// Смещает камеру на `(dx, dy)` в мировых координатах. Фактическое
    /// смещение вида применяется в рендер-цикле через map_output.
    pub fn move_camera(&mut self, dx: f64, dy: f64) {
        self.target_camera_offset.0 += dx;
        self.target_camera_offset.1 += dy;
        self.needs_render = true;
    }

    /// Спавнит тело для нового окна в физическом режиме. Окно появляется
    /// сверху видимой области и падает на пол/на другие окна.
    pub fn physics_spawn_window(&mut self, win: &Window) {
        // Вычисляем координаты до borrow phys, чтобы не конфликтовать с
        //borrow self.physics и self.output_size()/window_bodies.
        let spawn_x = self.camera_offset.0 as f32
            + self.output_size().w as f32 * 0.5
            + (self.window_bodies.len() as f32 % 8.0 - 4.0) * 40.0;
        let spawn_y = self.camera_offset.1 as f32 - 100.0; // чуть выше верхнего края
        let geo_size = win.geometry().size;
        let w = geo_size.w as f32;
        let h = geo_size.h as f32;
        let Some(phys) = self.physics.as_mut() else {
            return;
        };
        let handle = phys.spawn_window(spawn_x, spawn_y, w, h);
        self.window_bodies.insert(win.clone(), handle);
    }

    /// Убирает тело окна при его закрытии в физическом режиме.
    pub fn physics_remove_window(&mut self, win: &Window) {
        if let Some(handle) = self.window_bodies.remove(win) {
            if let Some(phys) = self.physics.as_mut() {
                phys.remove_window(handle);
            }
        }
    }

    /// Находит тело окна под курсором (мировые координаты) для drag. Возвращает
    /// окно + хендл тела, если курсор попадает в bounding-box тела.
    pub fn physics_pick(&self, world_x: f64, world_y: f64) -> Option<(Window, rapier2d::prelude::RigidBodyHandle)> {
        let phys = self.physics.as_ref()?;
        for (win, handle) in self.window_bodies.iter() {
            if let Some((cx, cy, _)) = phys.body_transform(*handle) {
                let size = self
                    .space
                    .element_geometry(win)
                    .map(|g| g.size)
                    .unwrap_or_else(|| (100, 100).into());
                let hw = size.w as f64 * 0.5;
                let hh = size.h as f64 * 0.5;
                if (world_x - cx as f64).abs() <= hw && (world_y - cy as f64).abs() <= hh {
                    return Some((win.clone(), *handle));
                }
            }
        }
        None
    }

    /// Начинает/обновляет/заканчивает drag тела в физическом режиме.
    pub fn physics_drag_begin(&mut self, world_x: f64, world_y: f64) -> bool {
        if self.layout_mode != LayoutMode::Physics {
            return false;
        }
        if let Some(picked) = self.physics_pick(world_x, world_y) {
            self.drag_body = Some(picked);
            self.drag_last_ptr = Some((world_x, world_y));
            // Будим тело, чтобы оно не спало во время drag.
            if let Some(phys) = self.physics.as_mut() {
                if let Some((_, h)) = self.drag_body {
                    phys.set_window_velocity(h, 0.0, 0.0);
                }
            }
            self.needs_render = true;
            true
        } else {
            false
        }
    }

    pub fn physics_drag_update(&mut self, world_x: f64, world_y: f64) {
        if self.drag_body.is_some() {
            self.drag_last_ptr = Some((world_x, world_y));
            self.needs_render = true;
        }
    }

    pub fn physics_drag_end(&mut self) {
        self.drag_body = None;
        self.drag_last_ptr = None;
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
        let current_buffer_geo = window.geometry();
        let mut loc = rect.loc;

        if let Some(old_geo) = self.space.element_geometry(window) {
            let old_left = old_geo.loc.x;
            let old_right = old_geo.loc.x + old_geo.size.w;
            let target_right = rect.loc.x + rect.size.w;
            
            let old_top = old_geo.loc.y;
            let old_bottom = old_geo.loc.y + old_geo.size.h;
            let target_bottom = rect.loc.y + rect.size.h;
            
            // If the right edge is conceptually fixed, anchor visual location to the right
            if (target_right - old_right).abs() < 5 && (rect.loc.x - old_left).abs() > 0 {
                loc.x = target_right - current_buffer_geo.size.w;
            }
            
            // If the bottom edge is conceptually fixed, anchor visual location to the bottom
            if (target_bottom - old_bottom).abs() < 5 && (rect.loc.y - old_top).abs() > 0 {
                loc.y = target_bottom - current_buffer_geo.size.h;
            }
        }

        self.space.map_element(window.clone(), loc, false);

        // Ask the client to resize its surface — but only if the size actually
        // changed. Sending a configure with the same size on every relayout used
        // to trigger an O(N²) "configure storm": each client re-commits its
        // buffer in response, which re-runs the layout, which re-configures every
        // window again. Comparing against the already-acked `current_state`
        // breaks that feedback loop after a single round-trip.
        if let Some(toplevel) = window.toplevel() {
            let mut changed = false;
            toplevel.with_pending_state(|state| {
                if state.size != Some(rect.size.into()) {
                    state.size = Some(rect.size.into());
                    changed = true;
                }
            });
            if changed {
                // Backpressure: only send a new configure if the client has
                // already processed and committed the previous one. This prevents
                // us from spamming 60 configures/sec to a client that can only
                // render at 15 FPS, which would cause an infinite queue buildup.
                if !self.pending_configures.contains(window) {
                    toplevel.send_configure();
                    self.pending_configures.insert(window.clone());
                }
            }
        }
    }

    /// Set keyboard focus to a specific window.
    pub fn focus_window(&mut self, window: &Window) {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();

        // Deactivate all windows first, then activate the target.
        // Dirty-check: only send a configure to windows whose activated state
        // actually changed. Previously every focus switch sent a configure to
        // ALL N windows, triggering an O(N) burst of client re-commits.
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
                if changed {
                    toplevel.send_configure();
                }
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
        if let Some(window) = found {
            window.on_commit();
            // Client has committed a buffer, meaning it has processed the last
            // configure event. We can send new configure events to it now.
            self.pending_configures.remove(&window);
            
            // Если мы в физическом режиме, но тело ещё не создано — самое время это сделать!
            // Но только когда клиент уже закоммитил реальный буфер (размер > placeholder 1x1).
            // Первый commit часто приходит с нулевой/заглушечной геометрией.
            if self.layout_mode == LayoutMode::Physics && !self.window_bodies.contains_key(&window) {
                let geo = window.geometry();
                if geo.size.w > 1 && geo.size.h > 1 {
                    self.physics_spawn_window(&window);
                }
            }

            // Синхронизируем размер коллайдера с реальной геометрией окна.
            // Клиент может менять размер после первого commit — коллайдер должен следовать.
            if self.layout_mode == LayoutMode::Physics {
                if let Some(&handle) = self.window_bodies.get(&window) {
                    let geo = window.geometry();
                    if geo.size.w > 1 && geo.size.h > 1 {
                        if let Some(phys) = self.physics.as_mut() {
                            phys.update_collider_size(handle, geo.size.w as f32, geo.size.h as f32);
                        }
                    }
                }
            }

            // During active resize: do NOT call apply_window_geometry or
            // recalculate_layout. Any such call can send configure →
            // client draws → commit → apply_window_geometry → configure →
            // an O(N) amplification loop per commit. The layout will be
            // applied once per frame via the layout_dirty flag in the
            // render loop.
        }

        // Новый буфер — экран нужно перерисовать.
        self.needs_render = true;
    }

}

smithay::delegate_compositor!(AppState);

// 2. Buffer & SHM
impl BufferHandler for AppState {
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
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
        // Пока кладём наверху (будет перемещён layout-ом / физикой)
        self.space.map_element(window.clone(), (0, 0), true);

        // Обновляем BSP дерево ВСЕГДА, чтобы при возврате из физического режима
        // окно корректно стало в тайл.
        let focused = self.get_focused_window();
        if let Some(tree) = self.tile_tree.take() {
            let target = focused.unwrap_or_else(|| window.clone());
            self.tile_tree = Some(tree.insert(&target, window.clone(), self.output_rect()));
        } else {
            self.tile_tree = Some(crate::tiling::TileNode::Leaf(window.clone()));
        }

        if self.layout_mode == LayoutMode::Physics {
            // Тело заспавнится в commit() когда клиент впервые отрисует окно
        } else {
            // Пересчитываем тайловую раскладку для всех окон
            self.recalculate_layout();
        }

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

            if self.layout_mode == LayoutMode::Physics {
                self.physics_remove_window(&window);
                // Если таскали именно это окно — сбрасываем drag.
                if self.drag_body.as_ref().map(|(w, _)| w == &window).unwrap_or(false) {
                    self.physics_drag_end();
                }
            } else if let Some(tree) = self.tile_tree.take() {
                self.tile_tree = tree.remove(&window);
                // Пересчитываем тайловую раскладку
                self.recalculate_layout();
            }

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
smithay::delegate_xdg_decoration!(AppState);

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
smithay::delegate_dmabuf!(AppState);
