//! Физическая «песочница» поверх [`AppState`]: куски окон, инструменты,
//! режимы гравитации, плавные переходы тайлинг ↔ физика, обратное
//! проецирование курсора, оверлей и сессия.
//!
//! Координаты:
//! - **экран** — физические пиксели выхода (scale 1);
//! - **мир** — бесконечный холст rapier: `экран = (мир − camera_offset) · zoom`;
//! - **координаты окна** — относительно верхнего левого угла геометрии окна
//!   (без CSD-тени), как у плоского буфера клиента. Кусок окна — прямоугольник
//!   `rect` в этих координатах, его центр совпадает с центром тела в мире.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rapier2d::math::Vector;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::seat::WaylandFocus;

use crate::overlay::{OverlayGeometry, to_screen};
use crate::physics::{Anchor, LinkKind, Material, WindowPhysics};
use crate::render::{Affine, PieceDraw};
use crate::state::{AppState, LayoutMode};

/// Кусок окна в физическом режиме (неразрезанное окно — один кусок).
#[derive(Debug, Clone)]
pub struct Piece {
    pub id: u32,
    pub window: Window,
    /// Обрезка в координатах окна.
    pub rect: Rectangle<f64, Logical>,
}

impl Piece {
    fn center(&self) -> (f64, f64) {
        (
            self.rect.loc.x + self.rect.size.w * 0.5,
            self.rect.loc.y + self.rect.size.h * 0.5,
        )
    }
}

/// Мягкое тело куска (концепт §5): сетка + параметры + прошлая скорость
/// тела для инерционных псевдосил (удар об пол сплющивает окно).
pub struct SoftPiece {
    pub mesh: crate::softbody::SoftMesh,
    pub params: crate::softbody::SoftParams,
    prev_vel: Option<Vector>,
    rest_generation: u64,
}

/// Активный инструмент физического режима.
#[derive(Debug, Clone)]
pub enum Tool {
    None,
    /// Рисование линий (концепт §2). `stroke` — мировые точки текущего штриха.
    Draw {
        material: Material,
        stroke: Vec<Vector>,
        drawing: bool,
        erasing: bool,
    },
    /// Нож (концепт §7): отрезок в экранных координатах.
    Knife {
        start: Option<(f64, f64)>,
        end: (f64, f64),
    },
    /// Связь двух окон (концепт §4): первая точка крепления, если выбрана.
    Link {
        kind: LinkKind,
        first: Option<Anchor>,
    },
}

/// Конец связи при пересоздании кусков: точка в координатах окна или
/// якорь на другом (не пересоздаваемом) куске.
#[derive(Debug, Clone, Copy)]
enum LinkEnd {
    Window(f64, f64),
    Anchor(Anchor),
}

/// Куда сейчас «смотрит» указатель в физическом режиме.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerTarget {
    pub window: Window,
    /// Кусок окна; `None` — X11 override-redirect окно (меню/тултип).
    pub piece: Option<u32>,
}

/// Экранная поза центра куска.
#[derive(Debug, Clone, Copy)]
struct ScreenPose {
    cx: f64,
    cy: f64,
    angle: f64,
    scale: f64,
}

impl ScreenPose {
    fn lerp(&self, o: &ScreenPose, t: f64) -> ScreenPose {
        ScreenPose {
            cx: self.cx + (o.cx - self.cx) * t,
            cy: self.cy + (o.cy - self.cy) * t,
            angle: self.angle + (o.angle - self.angle) * t,
            scale: self.scale + (o.scale - self.scale) * t,
        }
    }
}

struct TransitionItem {
    window: Window,
    key: u32,
    clip: Rectangle<f64, Logical>,
    popups: bool,
    from: ScreenPose,
    to: ScreenPose,
}

/// Плавный переход тайлинг ↔ физика (концепт §1, режим 2).
pub struct Transition {
    start: Instant,
    duration: f64,
    to_tiling: bool,
    items: Vec<TransitionItem>,
    cam_from: (f64, f64, f64),
    cam_to: (f64, f64, f64),
}

impl Transition {
    fn progress(&self, now: Instant) -> f64 {
        if self.duration <= 0.0 {
            return 1.0;
        }
        (now.duration_since(self.start).as_secs_f64() / self.duration).clamp(0.0, 1.0)
    }
}

/// Плавная кривая 0..1 (ease-in-out).
fn ease(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// Угол в (−π, π].
fn wrap_angle(a: f64) -> f64 {
    let tau = std::f64::consts::TAU;
    let mut a = a % tau;
    if a > std::f64::consts::PI {
        a -= tau;
    } else if a <= -std::f64::consts::PI {
        a += tau;
    }
    a
}

fn v2(x: f64, y: f64) -> Vector {
    Vector::new(x as f32, y as f32)
}

/// Пересечение отрезка `p0→p1` с прямоугольником (Liang–Barsky):
/// параметры входа/выхода `t ∈ [0,1]`.
fn clip_segment(p0: (f64, f64), p1: (f64, f64), r: Rectangle<f64, Logical>) -> Option<(f64, f64)> {
    let dx = p1.0 - p0.0;
    let dy = p1.1 - p0.1;
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    let checks = [
        (-dx, p0.0 - r.loc.x),
        (dx, r.loc.x + r.size.w - p0.0),
        (-dy, p0.1 - r.loc.y),
        (dy, r.loc.y + r.size.h - p0.1),
    ];
    for (p, q) in checks {
        if p.abs() < 1e-12 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
            if t0 > t1 {
                return None;
            }
        }
    }
    Some((t0, t1))
}

impl AppState {
    // ── Мир и камера ───────────────────────────────────────────────

    pub fn screen_to_world(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x / self.camera_zoom + self.camera_offset.0,
            y / self.camera_zoom + self.camera_offset.1,
        )
    }

    /// Создаёт физический мир при первой необходимости. Пол — на нижнем
    /// краю экрана (или из конфига/сессии).
    pub fn ensure_physics(&mut self) -> &mut WindowPhysics {
        if self.physics.is_none() {
            let floor = self
                .restored_floor_y
                .or(self.config.physics.floor_y)
                .unwrap_or_else(|| self.output_size().h as f32);
            self.physics = Some(WindowPhysics::new(&self.config, floor));
        }
        self.physics.as_mut().expect("physics just created")
    }

    pub fn zoom_camera(&mut self, factor: f64) {
        let (sx, sy) = (self.pointer_location.x, self.pointer_location.y);
        self.zoom_camera_at(factor, sx, sy);
    }

    /// Зум вокруг экранной точки: мировая точка под ней остаётся на месте.
    pub fn zoom_camera_at(&mut self, factor: f64, sx: f64, sy: f64) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        let (wx, wy) = self.screen_to_world(sx, sy);
        let zoom = (self.camera_zoom * factor)
            .clamp(self.config.physics.zoom_min, self.config.physics.zoom_max);
        self.camera_zoom = zoom;
        self.camera_offset = (wx - sx / zoom, wy - sy / zoom);
        self.target_camera_offset = self.camera_offset;
        self.camera_changed();
    }

    pub fn reset_camera(&mut self) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        self.camera_offset = (0.0, 0.0);
        self.target_camera_offset = (0.0, 0.0);
        self.camera_zoom = 1.0;
        self.camera_changed();
    }

    pub fn move_camera(&mut self, dx: f64, dy: f64) {
        self.target_camera_offset.0 += dx / self.camera_zoom;
        self.target_camera_offset.1 += dy / self.camera_zoom;
        self.needs_render = true;
    }

    fn camera_changed(&mut self) {
        self.sync_space_positions();
        self.needs_render = true;
    }

    pub fn physics_camera_pan_begin(&mut self, screen_x: f64, screen_y: f64) -> bool {
        if self.layout_mode != LayoutMode::Physics {
            return false;
        }
        self.camera_pan_active = true;
        self.camera_pan_last_ptr = Some((screen_x, screen_y));
        self.target_camera_offset = self.camera_offset;
        self.needs_render = true;
        true
    }

    /// Сдвигает камеру как схваченный холст; true — событие съедено.
    pub fn physics_camera_pan_update(&mut self, screen_x: f64, screen_y: f64) -> bool {
        if !self.camera_pan_active {
            return false;
        }
        if let Some((last_x, last_y)) = self.camera_pan_last_ptr {
            self.camera_offset.0 -= (screen_x - last_x) / self.camera_zoom;
            self.camera_offset.1 -= (screen_y - last_y) / self.camera_zoom;
            self.target_camera_offset = self.camera_offset;
        }
        self.camera_pan_last_ptr = Some((screen_x, screen_y));
        self.camera_changed();
        true
    }

    pub fn physics_camera_pan_end(&mut self) {
        self.camera_pan_active = false;
        self.camera_pan_last_ptr = None;
    }

    // ── Куски ──────────────────────────────────────────────────────

    pub fn pieces_of(&self, window: &Window) -> impl Iterator<Item = &Piece> + '_ {
        let window = window.clone();
        self.pieces.iter().filter(move |p| p.window == window)
    }

    fn piece_by_id(&self, id: u32) -> Option<&Piece> {
        self.pieces.iter().find(|p| p.id == id)
    }

    /// Основной кусок окна (самый большой) — с ним рисуются попапы.
    fn main_piece(&self, window: &Window) -> Option<&Piece> {
        self.pieces_of(window).max_by(|a, b| {
            (a.rect.size.w * a.rect.size.h).total_cmp(&(b.rect.size.w * b.rect.size.h))
        })
    }

    /// Координаты окна → экран для куска.
    fn piece_xf(&self, piece: &Piece) -> Option<Affine> {
        let phys = self.physics.as_ref()?;
        let (c, angle) = phys.piece_transform(piece.id)?;
        let s = to_screen(c, self.camera_offset, self.camera_zoom);
        Some(Affine::piece(
            (s[0] as f64, s[1] as f64),
            angle as f64,
            self.camera_zoom,
            piece.center(),
        ))
    }

    fn piece_screen_pose(&self, piece: &Piece) -> Option<ScreenPose> {
        let phys = self.physics.as_ref()?;
        let (c, angle) = phys.piece_transform(piece.id)?;
        Some(ScreenPose {
            cx: (c.x as f64 - self.camera_offset.0) * self.camera_zoom,
            cy: (c.y as f64 - self.camera_offset.1) * self.camera_zoom,
            angle: angle as f64,
            scale: self.camera_zoom,
        })
    }

    fn is_override_redirect(window: &Window) -> bool {
        window
            .x11_surface()
            .map_or(false, |x| x.is_override_redirect())
    }

    /// Свойства окна из конфига по app_id / X11-классу.
    fn window_override(&self, window: &Window) -> crate::config::WindowOverride {
        self.config.window_override(&window_app_id(window))
    }

    /// Создаёт тело для окна, которое впервые отрисовалось в физике.
    pub fn physics_spawn_window(&mut self, window: &Window) {
        if Self::is_override_redirect(window) || self.pieces_of(window).next().is_some() {
            return;
        }
        let size = window.geometry().size;
        if size.w <= 1 || size.h <= 1 {
            return;
        }
        if self.try_restore_window(window) {
            return;
        }
        let screen = self.output_size();
        let jitter = ((self.pieces.len() % 8) as f64 - 4.0) * 40.0;
        let x = self.camera_offset.0 + screen.w as f64 / (2.0 * self.camera_zoom) + jitter;
        // Чуть выше видимой области — окно падает в кадр.
        let y = self.camera_offset.1 - size.h as f64 * 0.5 - 40.0 / self.camera_zoom;
        let rect = Rectangle::new((0.0, 0.0).into(), (size.w as f64, size.h as f64).into());
        let props = self.window_override(window);
        let id = self.spawn_piece(window, rect, v2(x, y), 0.0, props.weight.unwrap_or(1.0));
        if props.floating.unwrap_or(false) {
            self.set_window_floating(window, true);
        }
        if props.softness.unwrap_or(0.0) > 0.0 {
            let params = self.soft_params(window);
            self.make_soft(id, params);
        }
        self.sync_space_positions();
    }

    fn spawn_piece(
        &mut self,
        window: &Window,
        rect: Rectangle<f64, Logical>,
        center: Vector,
        angle: f32,
        weight: f32,
    ) -> u32 {
        // X11 не умеет повёрнутые окна: глобальные координаты кликов
        // (Steam/CEF) съезжают. Блокируем вращение.
        let lock = window.x11_surface().is_some();
        let id = self.ensure_physics().spawn_piece(
            center,
            angle,
            rect.size.w as f32,
            rect.size.h as f32,
            weight,
            lock,
        );
        self.pieces.push(Piece {
            id,
            window: window.clone(),
            rect,
        });
        id
    }

    /// Убирает все куски окна (закрытие окна).
    pub fn physics_remove_window(&mut self, window: &Window) {
        let ids: Vec<u32> = self.pieces_of(window).map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_ref() {
            if phys.drag_piece().is_some_and(|p| ids.contains(&p)) {
                // Тащили закрывшееся окно — ховер клиентам больше не глушим.
                self.dragging = false;
            }
        }
        self.restore_tried.remove(window);
        for id in &ids {
            self.soft.remove(id);
            if self.soft_grab.as_ref().map_or(false, |(p, _)| p == id) {
                self.soft_grab = None;
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.remove_piece(*id);
            }
            if self.spin_piece == Some(*id) {
                self.spin_piece = None;
                self.spin_dir = 0.0;
            }
        }
        self.pieces.retain(|p| &p.window != window);
        self.snapshots.remove(window);
        if self
            .pointer_grab_target
            .as_ref()
            .map_or(false, |t| &t.window == window)
        {
            self.pointer_grab_target = None;
        }
        if let Tool::Link { first: Some(a), .. } = &self.tool {
            if ids.contains(&a.piece) {
                self.tool = Tool::None;
            }
        }
        if let Some(t) = self.transition.as_mut() {
            t.items.retain(|i| &i.window != window);
        }
    }

    /// Клиент поменял размер окна: куски следуют за ним. Неразрезанный
    /// кусок — это всё окно; разрезанные масштабируются пропорционально.
    pub fn physics_window_resized(&mut self, window: &Window) {
        // В тайлинге клиент рисует тайловый размер — выключенные коллайдеры
        // его не берут: при возврате в физику окну вернут прежний размер, а
        // иначе тело включалось бы уже раздутым (в полу, куски — внахлёст).
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        let size = window.geometry().size;
        if size.w <= 1 || size.h <= 1 {
            return;
        }
        let pieces: Vec<(u32, Rectangle<f64, Logical>)> =
            self.pieces_of(window).map(|p| (p.id, p.rect)).collect();
        if pieces.is_empty() {
            return;
        }
        let bounds = pieces
            .iter()
            .map(|(_, r)| *r)
            .reduce(|a, b| a.merge(b))
            .unwrap_or_default();
        let (w, h) = (size.w as f64, size.h as f64);
        if bounds.loc == Point::from((0.0, 0.0)) && bounds.size == (w, h).into() {
            return;
        }
        let sx = w / bounds.size.w.max(1.0);
        let sy = h / bounds.size.h.max(1.0);
        for (id, r) in pieces {
            let rect = if self.pieces_of(window).count() == 1 {
                Rectangle::new((0.0, 0.0).into(), (w, h).into())
            } else {
                Rectangle::new(
                    ((r.loc.x - bounds.loc.x) * sx, (r.loc.y - bounds.loc.y) * sy).into(),
                    (r.size.w * sx, r.size.h * sy).into(),
                )
            };
            if let Some(p) = self.pieces.iter_mut().find(|p| p.id == id) {
                p.rect = rect;
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_size(id, rect.size.w as f32, rect.size.h as f32);
            }
            if let Some(soft) = self.soft.get_mut(&id) {
                soft.mesh.resize(rect.size.w as f32, rect.size.h as f32);
            }
        }
    }

    // ── Режимы ─────────────────────────────────────────────────────

    /// Super+G: тайлинг ↔ физика (оба направления — плавно).
    pub fn toggle_physics(&mut self) {
        match self.layout_mode {
            LayoutMode::Tiling => self.enter_physics_mode(),
            LayoutMode::Physics => self.begin_tiling_transition(),
        }
        self.needs_render = true;
    }

    /// Режим 1: обычная гравитация.
    pub fn gravity_normal(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        if let Some(phys) = self.physics.as_mut() {
            phys.set_zero_gravity(false);
        }
        self.needs_render = true;
    }

    /// Режим 2: гравитация выключена — окна плавно встают в тайлинг.
    pub fn gravity_tiling(&mut self) {
        if self.layout_mode == LayoutMode::Physics {
            self.begin_tiling_transition();
        }
    }

    /// Режим 4: невесомость для всех (переключатель).
    pub fn toggle_zero_gravity(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let phys = self.ensure_physics();
        let zero = !phys.zero_gravity();
        phys.set_zero_gravity(zero);
        println!(
            "[physics] невесомость: {}",
            if zero { "вкл" } else { "выкл" }
        );
        self.needs_render = true;
    }

    /// Режим 3: невесомость только для активного окна (переключатель).
    pub fn toggle_float_focused(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let floating = !self.window_floating(&window);
        self.set_window_floating(&window, floating);
        println!(
            "[physics] невесомость окна {:?}: {}",
            window_app_id(&window),
            if floating { "вкл" } else { "выкл" }
        );
        self.needs_render = true;
    }

    fn window_floating(&self, window: &Window) -> bool {
        let Some(phys) = self.physics.as_ref() else {
            return false;
        };
        self.pieces_of(window).any(|p| phys.is_floating(p.id))
    }

    fn set_window_floating(&mut self, window: &Window, floating: bool) {
        let ids: Vec<u32> = self.pieces_of(window).map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.set_floating(id, floating);
            }
        }
    }

    /// Tiling → Physics. Окна, у которых уже есть тела (были в физике до
    /// тайлинга), плавно возвращаются на свои места; новые — получают тела
    /// в текущих тайловых позициях.
    pub fn enter_physics_mode(&mut self) {
        if self.layout_mode == LayoutMode::Physics {
            return;
        }
        self.end_resize();
        self.ensure_physics();
        let geoms: Vec<(Window, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter(|w| !Self::is_override_redirect(w))
            .filter_map(|w| Some((w.clone(), self.space.element_geometry(w)?)))
            .collect();

        let cam_to = self.saved_camera;
        let mut items = Vec::new();
        for (window, geo) in geoms {
            let existing: Vec<Piece> = self.pieces_of(&window).cloned().collect();
            if existing.is_empty() {
                let rect = Rectangle::new(
                    (0.0, 0.0).into(),
                    (geo.size.w as f64, geo.size.h as f64).into(),
                );
                // Мир под сохранённой камерой: экранная тайловая позиция → мир.
                let cx = (geo.loc.x as f64 + geo.size.w as f64 * 0.5) / cam_to.2 + cam_to.0;
                let cy = (geo.loc.y as f64 + geo.size.h as f64 * 0.5) / cam_to.2 + cam_to.1;
                let props = self.window_override(&window);
                self.spawn_piece(&window, rect, v2(cx, cy), 0.0, props.weight.unwrap_or(1.0));
                if props.floating.unwrap_or(false) {
                    self.set_window_floating(&window, true);
                }
                continue;
            }
            let (ww, wh) = (
                window.geometry().size.w.max(1) as f64,
                window.geometry().size.h.max(1) as f64,
            );
            let main = self.main_piece(&window).map(|p| p.id);
            for piece in existing {
                let Some((c, angle)) = self
                    .physics
                    .as_ref()
                    .and_then(|p| p.piece_transform(piece.id))
                else {
                    continue;
                };
                let (pcx, pcy) = piece.center();
                let from = ScreenPose {
                    cx: geo.loc.x as f64 + pcx * geo.size.w as f64 / ww,
                    cy: geo.loc.y as f64 + pcy * geo.size.h as f64 / wh,
                    angle: 0.0,
                    scale: 1.0,
                };
                let angle = angle as f64;
                let to = ScreenPose {
                    cx: (c.x as f64 - cam_to.0) * cam_to.2,
                    cy: (c.y as f64 - cam_to.1) * cam_to.2,
                    angle,
                    scale: cam_to.2,
                };
                items.push(TransitionItem {
                    window: window.clone(),
                    key: piece.id,
                    clip: piece.rect,
                    popups: Some(piece.id) == main,
                    from: ScreenPose {
                        angle: angle - wrap_angle(angle),
                        ..from
                    },
                    to,
                });
            }
        }

        self.layout_mode = LayoutMode::Physics;
        self.tool = Tool::None;
        self.last_physics_tick = None;
        if items.is_empty() {
            self.camera_offset = (cam_to.0, cam_to.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = cam_to.2;
            self.set_pieces_enabled(true);
            self.restore_physics_sizes();
        } else {
            self.transition = Some(Transition {
                start: Instant::now(),
                duration: self.config.gravity_modes.freeze_transition_secs.max(0.0) as f64,
                to_tiling: false,
                items,
                cam_from: (0.0, 0.0, 1.0),
                cam_to,
            });
        }
        self.sync_space_positions();
        self.needs_render = true;
        println!(
            "[physics] режим включён, {} окон",
            self.space.elements().count()
        );
    }

    /// Physics → Tiling: окна плавно выстраиваются по сетке.
    fn begin_tiling_transition(&mut self) {
        if self.transition.is_some() {
            return;
        }
        self.cancel_interactions();
        self.saved_camera = (self.camera_offset.0, self.camera_offset.1, self.camera_zoom);
        // Тайлинг поменяет размеры окон — запоминаем физические, чтобы
        // вернуть их при возврате в физику.
        self.physics_sizes = self
            .pieces
            .iter()
            .map(|p| (p.window.clone(), p.window.geometry().size))
            .collect();
        let tiles = self.tile_rects();
        let mut items = Vec::new();
        for piece in self.pieces.clone() {
            let Some(from) = self.piece_screen_pose(&piece) else {
                continue;
            };
            let Some(tile) = tiles.get(&piece.window) else {
                continue;
            };
            let size = piece.window.geometry().size;
            let (ww, wh) = (size.w.max(1) as f64, size.h.max(1) as f64);
            let (pcx, pcy) = piece.center();
            let to = ScreenPose {
                cx: tile.loc.x as f64 + pcx * tile.size.w as f64 / ww,
                cy: tile.loc.y as f64 + pcy * tile.size.h as f64 / wh,
                // Кратчайший доворот до 0.
                angle: from.angle - wrap_angle(from.angle),
                scale: 1.0,
            };
            let popups = self.main_piece(&piece.window).map(|p| p.id) == Some(piece.id);
            items.push(TransitionItem {
                window: piece.window.clone(),
                key: piece.id,
                clip: piece.rect,
                popups,
                from,
                to,
            });
        }
        self.transition = Some(Transition {
            start: Instant::now(),
            duration: self.config.gravity_modes.freeze_transition_secs.max(0.0) as f64,
            to_tiling: true,
            items,
            cam_from: self.saved_camera,
            cam_to: (0.0, 0.0, 1.0),
        });
        self.needs_render = true;
    }

    /// Тайловые прямоугольники окон (как их расставит `recalculate_layout`).
    fn tile_rects(&self) -> HashMap<Window, Rectangle<i32, Logical>> {
        let mut rects = Vec::new();
        if let Some(tree) = &self.tile_tree {
            tree.collect_rects(self.tiling_area(), crate::state::GAPS_IN, &mut rects);
        }
        rects.into_iter().collect()
    }

    fn finish_transition(&mut self) {
        let Some(t) = self.transition.take() else {
            return;
        };
        if t.to_tiling {
            self.set_pieces_enabled(false);
            self.camera_offset = (0.0, 0.0);
            self.target_camera_offset = (0.0, 0.0);
            self.camera_zoom = 1.0;
            self.layout_mode = LayoutMode::Tiling;
            // Инструмент, включённый во время анимации, в тайлинге не нужен.
            self.tool = Tool::None;
            self.recalculate_layout();
            println!("[physics] режим выключен, возврат к тайлингу");
        } else {
            self.camera_offset = (t.cam_to.0, t.cam_to.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = t.cam_to.2;
            self.set_pieces_enabled(true);
            self.last_physics_tick = None;
            self.restore_physics_sizes();
            // Окна, открытые во время анимации: их коммит пришёлся на переход,
            // а статичный клиент второго коммита может и не прислать.
            let orphans: Vec<Window> = self
                .space
                .elements()
                .filter(|w| !Self::is_override_redirect(w))
                .filter(|w| self.pieces_of(w).next().is_none())
                .cloned()
                .collect();
            for window in orphans {
                self.physics_spawn_window(&window);
            }
            self.sync_space_positions();
        }
        self.needs_render = true;
    }

    /// Возвращает окнам размеры, которые были у них в физике до тайлинга.
    fn restore_physics_sizes(&mut self) {
        for (window, size) in std::mem::take(&mut self.physics_sizes) {
            if window.geometry().size == size || !self.space.elements().any(|w| *w == window) {
                continue;
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(size));
                toplevel.send_configure();
                self.pending_configures.insert(window.clone());
            } else if let Some(x11) = window.x11_surface() {
                let mut geo = x11.geometry();
                geo.size = size;
                let _ = x11.configure(geo);
            }
        }
    }

    fn set_pieces_enabled(&mut self, enabled: bool) {
        let ids: Vec<u32> = self.pieces.iter().map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.set_piece_enabled(id, enabled);
            }
        }
    }

    /// Сбрасывает незавершённые drag/spin/инструменты.
    fn cancel_interactions(&mut self) {
        if let Some(phys) = self.physics.as_mut() {
            phys.drag_end();
            phys.set_spin(None);
        }
        self.dragging = false;
        self.spin_dir = 0.0;
        self.spin_piece = None;
        self.camera_pan_active = false;
        self.camera_pan_last_ptr = None;
        self.tool = Tool::None;
        self.pointer_grab_target = None;
        self.soft_grab_end();
    }

    // ── Шаг симуляции ──────────────────────────────────────────────

    /// Один кадр физического режима. Вызывается из рендер-цикла.
    pub fn step_physics(&mut self) {
        let now = Instant::now();
        // Автосохранение и ожидание восстанавливаемых окон — в любом режиме.
        self.maybe_autosave(now);
        if self.layout_mode != LayoutMode::Physics {
            return;
        }
        if let Some(t) = self.transition.as_ref() {
            let p = t.progress(now);
            let e = ease(p);
            let cam = (
                t.cam_from.0 + (t.cam_to.0 - t.cam_from.0) * e,
                t.cam_from.1 + (t.cam_to.1 - t.cam_from.1) * e,
                t.cam_from.2 + (t.cam_to.2 - t.cam_from.2) * e,
            );
            self.camera_offset = (cam.0, cam.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = cam.2;
            self.needs_render = true;
            if p >= 1.0 {
                self.finish_transition();
            }
            return;
        }

        let elapsed = self
            .last_physics_tick
            .map_or(self.config.physics.physics_dt as f64, |t| {
                now.duration_since(t).as_secs_f64()
            })
            .min(0.25);
        self.last_physics_tick = Some(now);

        let spin_rate = self.config.physics.spin_rate;
        let spin = if self.spin_dir != 0.0 {
            self.spin_piece.map(|p| (p, self.spin_dir * spin_rate))
        } else {
            None
        };
        let drag_target = self
            .dragging
            .then(|| self.screen_to_world(self.pointer_location.x, self.pointer_location.y));
        let Some(phys) = self.physics.as_mut() else {
            return;
        };
        phys.set_spin(spin);
        if let Some((x, y)) = drag_target {
            phys.drag_update(v2(x, y));
        }
        let steps = phys.advance(elapsed as f32);
        let mut moving = phys.any_moving() || spin.is_some();
        moving |= self.step_soft(elapsed as f32);

        // Плавная камера (Lerp).
        let lerp = self.config.physics.camera_lerp.clamp(0.01, 1.0);
        let cam_dx = self.target_camera_offset.0 - self.camera_offset.0;
        let cam_dy = self.target_camera_offset.1 - self.camera_offset.1;
        let mut camera_moved = false;
        if cam_dx * cam_dx + cam_dy * cam_dy > 0.25 / (self.camera_zoom * self.camera_zoom) {
            self.camera_offset.0 += cam_dx * lerp;
            self.camera_offset.1 += cam_dy * lerp;
            camera_moved = true;
            moving = true;
        } else if cam_dx != 0.0 || cam_dy != 0.0 {
            self.camera_offset = self.target_camera_offset;
            camera_moved = true;
        }

        if steps > 0 || camera_moved {
            self.sync_space_positions();
        }
        if moving || camera_moved {
            self.needs_render = true;
        }
    }

    /// Нужно ли держать кадры (физика/камера/переход ещё в движении).
    pub fn physics_animating(&self) -> bool {
        if self.layout_mode != LayoutMode::Physics {
            return false;
        }
        self.transition.is_some()
            || self.physics.as_ref().map_or(false, |p| p.any_moving())
            || self.spin_dir != 0.0
            || self.soft_grab.is_some()
            || self.soft.values().any(|s| s.mesh.is_active())
            || self.target_camera_offset != self.camera_offset
    }

    /// Обновляет позиции окон в [`Space`] (output enter/leave, кадры) и
    /// X11-геометрию, не меняя порядок окон в стеке.
    ///
    /// `Space::map_element` поднимает элемент наверх, а позиции применялись
    /// каждый кадр в порядке обхода HashMap — z-порядок окон в физике
    /// перемешивался каждый кадр, а меню Steam уходили под окна.
    pub fn sync_space_positions(&mut self) {
        if self.layout_mode != LayoutMode::Physics {
            return;
        }
        let order: Vec<Window> = self.space.elements().cloned().collect();
        let mut moved = false;
        for window in &order {
            if Self::is_override_redirect(window) {
                continue;
            }
            let Some(piece) = self.main_piece(window).cloned() else {
                continue;
            };
            let Some(xf) = self.piece_xf(&piece) else {
                continue;
            };
            let bounds = xf.bounds(piece.rect);
            let loc = Point::<i32, Logical>::from((
                bounds.loc.x + self.camera_offset.0.round() as i32,
                bounds.loc.y + self.camera_offset.1.round() as i32,
            ));
            if self.space.element_location(window) != Some(loc) {
                self.space.map_element(window.clone(), loc, false);
                moved = true;
            }
            // X11: X-геометрия в ЭКРАННЫХ координатах — X-сервер клампит
            // указатель к своему экрану, мировые координаты рвут клики CEF.
            if let Some(x11) = window.x11_surface() {
                let (ox, oy) = xf.apply(0.0, 0.0);
                let target = Rectangle::new(
                    (ox.round() as i32, oy.round() as i32).into(),
                    window.geometry().size,
                );
                if x11.geometry() != target {
                    let _ = x11.configure(target);
                }
            }
        }
        if moved {
            for window in &order {
                self.space.raise_element(window, false);
            }
        }
    }

    // ── Отрисовка ──────────────────────────────────────────────────

    /// Что рисовать в физическом режиме, front-to-back.
    pub fn piece_draws(&self) -> Vec<PieceDraw> {
        let mut out = Vec::new();
        if let Some(t) = self.transition.as_ref() {
            let e = ease(t.progress(Instant::now()));
            // Сверху — окна, которые выше в стеке Space.
            for window in self.space.elements().rev() {
                for item in t.items.iter().filter(|i| &i.window == window) {
                    let pose = item.from.lerp(&item.to, e);
                    let c = (
                        item.clip.loc.x + item.clip.size.w * 0.5,
                        item.clip.loc.y + item.clip.size.h * 0.5,
                    );
                    out.push(PieceDraw {
                        window: item.window.clone(),
                        key: item.key,
                        clip: Some(item.clip),
                        xf: Affine::piece((pose.cx, pose.cy), pose.angle, pose.scale, c),
                        popups: item.popups,
                        mesh: None,
                    });
                }
            }
            return out;
        }
        for window in self.space.elements().rev() {
            if Self::is_override_redirect(window) {
                // Меню/тултипы клиент ставит в экранных координатах.
                if let Some(geo) = self.space.element_geometry(window) {
                    out.push(PieceDraw {
                        window: window.clone(),
                        key: 0,
                        clip: None,
                        xf: Affine::translation(geo.loc.x as f64, geo.loc.y as f64),
                        popups: false,
                        mesh: None,
                    });
                }
                continue;
            }
            let main = self.main_piece(window).map(|p| p.id);
            // Основной кусок первым: его попапы должны быть поверх остальных
            // кусков окна (hit-test тоже отдаёт попапам приоритет).
            let mut pieces: Vec<&Piece> = self.pieces_of(window).collect();
            pieces.sort_by_key(|p| Some(p.id) != main);
            for piece in pieces {
                if let Some(xf) = self.piece_xf(piece) {
                    out.push(PieceDraw {
                        window: window.clone(),
                        key: piece.id,
                        clip: Some(piece.rect),
                        xf,
                        popups: Some(piece.id) == main,
                        mesh: self.mesh_draw(piece, &xf),
                    });
                }
            }
        }
        out
    }

    /// Геометрия оверлея тремя слоями, front-to-back: интерфейс инструментов
    /// (превью, значок у курсора), мир (линии, верёвки, связи), пол. Слои —
    /// отдельные render-элементы: значок, бегущий за курсором, перерисовывает
    /// только свой прямоугольник, а не весь экран до пола.
    pub fn overlay_geometry(&self) -> [OverlayGeometry; 3] {
        let mut ui = OverlayGeometry::new();
        let mut g = OverlayGeometry::new();
        let mut floor = OverlayGeometry::new();
        if self.layout_mode != LayoutMode::Physics {
            return [ui, g, floor];
        }
        let Some(phys) = self.physics.as_ref() else {
            return [ui, g, floor];
        };
        let cam = self.camera_offset;
        let zoom = self.camera_zoom;
        let screen = self.output_size();
        let materials = &self.config.materials;

        if self.config.physics.floor_visible {
            let color = crate::config::parse_color(&self.config.physics.floor_color);
            let y = ((phys.floor_y() as f64 - cam.1) * zoom) as f32;
            let thickness = (4.0 * zoom as f32).max(1.5);
            if y > -thickness && y < screen.h as f32 {
                floor.segment(
                    [-10.0, y + thickness * 0.5],
                    [screen.w as f32 + 10.0, y + thickness * 0.5],
                    thickness,
                    color,
                );
            }
            if self.config.physics.floor_dots {
                // Два ряда точек 6×6 с шагом 40 px под полом, второй ряд
                // сдвинут на полшага — шахматный порядок. Точки привязаны к
                // миру и едут вместе с камерой; всё это один draw call.
                const STEP: f64 = 40.0;
                let radius = (3.0 * zoom as f32).max(1.0);
                let first = (cam.0 / STEP).floor() as i64 - 1;
                let count = (screen.w as f64 / zoom / STEP).ceil() as i64 + 3;
                for row in 0..2 {
                    let wy = phys.floor_y() as f64 + 23.0 + row as f64 * 20.0;
                    let sy = ((wy - cam.1) * zoom) as f32;
                    if sy < -radius || sy > screen.h as f32 + radius {
                        continue;
                    }
                    let shift = if row == 1 { STEP * 0.5 } else { 0.0 };
                    for k in first..first + count {
                        let wx = k as f64 * STEP + shift;
                        let sx = ((wx - cam.0) * zoom) as f32;
                        floor.disc([sx, sy], radius, color);
                    }
                }
            }
        }

        for line in phys.line_views() {
            let pts: Vec<[f32; 2]> = line
                .points
                .iter()
                .map(|p| to_screen(*p, cam, zoom))
                .collect();
            let width = (line.material.thickness(materials) as f64 * zoom) as f32;
            let color = line.material.color(materials);
            g.polyline(&pts, width.max(1.5), color);
            if line.material == Material::Rope {
                // Закреплённые концы — гвоздики.
                for (end, pinned) in line.pinned.iter().enumerate() {
                    if *pinned {
                        let p = if end == 0 { pts.first() } else { pts.last() };
                        if let Some(p) = p {
                            g.disc(*p, (width * 0.9).max(3.0), [0.85, 0.85, 0.9, 1.0]);
                        }
                    }
                }
            }
        }

        let rope_width = ((materials.rope.thickness as f64 * zoom) as f32).max(1.5);
        // Во время перехода окна летят в экранных координатах независимо от
        // мира — связи «отрывались» бы от них.
        let links = if self.transition.is_some() {
            Vec::new()
        } else {
            phys.link_views()
        };
        for link in links {
            let pts: Vec<[f32; 2]> = link
                .points
                .iter()
                .map(|p| to_screen(*p, cam, zoom))
                .collect();
            match link.kind {
                LinkKind::Weld => {
                    let color = [1.0, 0.55, 0.2, 1.0];
                    if pts.len() == 2 {
                        g.segment(pts[0], pts[1], (6.0 * zoom as f32).max(2.0), color);
                    }
                    for p in &pts {
                        g.disc(*p, (7.0 * zoom as f32).max(3.0), color);
                    }
                }
                LinkKind::Spring => {
                    let color = crate::config::parse_color(&self.config.joints.spring.color);
                    if pts.len() == 2 {
                        g.zigzag(
                            pts[0],
                            pts[1],
                            (3.0 * zoom as f32).max(1.5),
                            (10.0 * zoom as f32).max(4.0),
                            color,
                        );
                    }
                    for p in &pts {
                        g.disc(*p, (5.0 * zoom as f32).max(2.5), color);
                    }
                }
                LinkKind::Rope => {
                    g.polyline(&pts, rope_width, Material::Rope.color(materials));
                }
            }
        }

        // Превью инструментов.
        let ptr = [
            self.pointer_location.x as f32,
            self.pointer_location.y as f32,
        ];
        match &self.tool {
            Tool::Draw {
                material, stroke, ..
            } => {
                if stroke.len() >= 2 {
                    let pts: Vec<[f32; 2]> =
                        stroke.iter().map(|p| to_screen(*p, cam, zoom)).collect();
                    let width = ((material.thickness(materials) as f64 * zoom) as f32).max(1.5);
                    let mut color = material.color(materials);
                    color[3] *= 0.8;
                    ui.polyline(&pts, width, color);
                }
                self.tool_badge(&mut ui, ptr, material.color(materials), 0);
            }
            Tool::Knife { start, end } => {
                if let Some(s) = start {
                    ui.dashed(
                        [s.0 as f32, s.1 as f32],
                        [end.0 as f32, end.1 as f32],
                        2.0,
                        8.0,
                        [1.0, 0.3, 0.3, 1.0],
                    );
                }
                self.tool_badge(&mut ui, ptr, [1.0, 0.3, 0.3, 1.0], 1);
            }
            Tool::Link { kind, first } => {
                let color = match kind {
                    LinkKind::Weld => [1.0, 0.55, 0.2, 1.0],
                    LinkKind::Spring => {
                        crate::config::parse_color(&self.config.joints.spring.color)
                    }
                    LinkKind::Rope => Material::Rope.color(materials),
                };
                if let Some(a) = first {
                    if let Some(p) = self.anchor_world(a) {
                        let s = to_screen(p, cam, zoom);
                        ui.dashed(s, ptr, 2.0, 6.0, color);
                        ui.disc(s, 5.0, color);
                    }
                }
                self.tool_badge(&mut ui, ptr, color, 2);
            }
            Tool::None => {}
        }
        [ui, g, floor]
    }

    /// Маленький значок инструмента рядом с курсором.
    fn tool_badge(&self, g: &mut OverlayGeometry, ptr: [f32; 2], color: [f32; 4], kind: u8) {
        let c = [ptr[0] + 18.0, ptr[1] + 18.0];
        g.disc(c, 8.0, [0.0, 0.0, 0.0, 0.6]);
        g.disc(c, 6.0, color);
        if kind == 1 {
            g.segment(
                [c[0] - 4.0, c[1] - 4.0],
                [c[0] + 4.0, c[1] + 4.0],
                1.5,
                [1.0; 4],
            );
        } else if kind == 2 {
            g.disc(c, 2.0, [1.0; 4]);
        }
    }

    fn anchor_world(&self, a: &Anchor) -> Option<Vector> {
        let (c, angle) = self.physics.as_ref()?.piece_transform(a.piece)?;
        Some(c + crate::physics::rotate(a.local, angle))
    }

    // ── Указатель и обратное проецирование ─────────────────────────

    /// Экранная точка → координаты окна для куска. Повёрнутые/масштабированные
    /// куски инвертируются аффинно, деформированные мягкие — через сетку
    /// (концепт §6: приложение получает координаты в своём плоском буфере).
    /// `inside` — попала ли точка в кусок; `extrapolate` — продолжать
    /// отображение за краем (захват указателя при выделении текста).
    fn piece_local_point(
        &self,
        piece: &Piece,
        sx: f64,
        sy: f64,
        extrapolate: bool,
    ) -> Option<(Point<f64, Logical>, bool)> {
        let inv = self.piece_xf(piece)?.inverse()?;
        let (x, y) = inv.apply(sx, sy);
        let r = piece.rect;
        if self.config.soft_body.text_reprojection {
            if let Some(soft) = self.soft.get(&piece.id) {
                if soft.mesh.is_deformed() {
                    let (cx, cy) = piece.center();
                    let local = [(x - cx) as f32, (y - cy) as f32];
                    let inside = soft.mesh.local_to_uv(local, false);
                    let uv = match inside {
                        Some(uv) => uv,
                        None if extrapolate => soft.mesh.local_to_uv(local, true)?,
                        None => return Some(((x, y).into(), false)),
                    };
                    let p = (
                        r.loc.x + uv[0] as f64 * r.size.w,
                        r.loc.y + uv[1] as f64 * r.size.h,
                    );
                    return Some((p.into(), inside.is_some()));
                }
            }
        }
        let inside =
            x >= r.loc.x && y >= r.loc.y && x < r.loc.x + r.size.w && y < r.loc.y + r.size.h;
        Some(((x, y).into(), inside))
    }

    /// Окно/кусок под экранной точкой и координаты в окне (физический режим).
    pub fn physics_pointer_target(
        &self,
        sx: f64,
        sy: f64,
    ) -> Option<(PointerTarget, Point<f64, Logical>)> {
        if self.transition.is_some() {
            return None;
        }
        for window in self.space.elements().rev() {
            if Self::is_override_redirect(window) {
                let geo = self.space.element_geometry(window)?;
                let local =
                    Point::<f64, Logical>::from((sx - geo.loc.x as f64, sy - geo.loc.y as f64));
                // Только реальная зона ввода: невидимые служебные OR-слои
                // CEF не должны съедать клики.
                if local.x >= 0.0
                    && local.y >= 0.0
                    && local.x <= geo.size.w as f64
                    && local.y <= geo.size.h as f64
                    && window
                        .surface_under(local, WindowSurfaceType::ALL)
                        .is_some()
                {
                    return Some((
                        PointerTarget {
                            window: window.clone(),
                            piece: None,
                        },
                        local,
                    ));
                }
                continue;
            }
            // Попапы окна живут в той же плоскости, что и основной кусок,
            // но выходят за его прямоугольник.
            if let Some(main) = self.main_piece(window) {
                if let Some((local, _)) = self.piece_local_point(main, sx, sy, false) {
                    let geo_loc = window.geometry().loc.to_f64();
                    if window
                        .surface_under(local + geo_loc, WindowSurfaceType::POPUP)
                        .is_some()
                    {
                        return Some((
                            PointerTarget {
                                window: window.clone(),
                                piece: Some(main.id),
                            },
                            local,
                        ));
                    }
                }
            }
            for piece in self.pieces_of(window) {
                if let Some((local, true)) = self.piece_local_point(piece, sx, sy, false) {
                    return Some((
                        PointerTarget {
                            window: window.clone(),
                            piece: Some(piece.id),
                        },
                        local,
                    ));
                }
            }
        }
        None
    }

    /// Координаты окна для заданной цели даже за её пределами — для
    /// неявного захвата указателя (выделение текста мышью уходит за край
    /// повёрнутого окна, а приложение должно получать согласованные точки).
    pub fn physics_target_point(
        &self,
        target: &PointerTarget,
        sx: f64,
        sy: f64,
    ) -> Option<Point<f64, Logical>> {
        match target.piece {
            Some(id) => {
                let piece = self.piece_by_id(id)?;
                self.piece_local_point(piece, sx, sy, true).map(|(p, _)| p)
            }
            None => {
                let geo = self.space.element_geometry(&target.window)?;
                Some((sx - geo.loc.x as f64, sy - geo.loc.y as f64).into())
            }
        }
    }

    /// Поверхность под точкой окна → (surface, её начало) в «виртуальном»
    /// пространстве указателя, где начало буфера окна = (0,0).
    pub fn physics_surface_under(
        &self,
        window: &Window,
        local: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let geo_loc = window.geometry().loc.to_f64();
        window
            .surface_under(local + geo_loc, WindowSurfaceType::ALL)
            .map(|(s, loc)| (s, loc.to_f64()))
    }

    /// Окно-кусок под курсором для drag/spin/инструментов.
    fn piece_at(&self, sx: f64, sy: f64) -> Option<(u32, Point<f64, Logical>)> {
        let (target, local) = self.physics_pointer_target(sx, sy)?;
        Some((target.piece?, local))
    }

    // ── Drag / spin ────────────────────────────────────────────────

    /// Super+ЛКМ: схватить окно в точке под курсором.
    pub fn physics_drag_begin(&mut self, sx: f64, sy: f64) -> Option<Window> {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return None;
        }
        let (piece, _) = self.piece_at(sx, sy)?;
        let window = self.piece_by_id(piece)?.window.clone();
        let (wx, wy) = self.screen_to_world(sx, sy);
        if self.physics.as_mut()?.drag_begin(piece, v2(wx, wy)) {
            self.dragging = true;
            self.last_physics_tick.get_or_insert_with(Instant::now);
            self.needs_render = true;
            Some(window)
        } else {
            None
        }
    }

    pub fn physics_drag_end(&mut self) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        if let Some(phys) = self.physics.as_mut() {
            phys.drag_end();
        }
        self.needs_render = true;
    }

    /// Удержание Super+A/D: крутить активное окно.
    pub fn physics_spin_hold(&mut self, dir: f32) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let Some(piece) = self.main_piece(&window).map(|p| p.id) else {
            return;
        };
        self.spin_dir = dir.signum();
        self.spin_piece = Some(piece);
        self.needs_render = true;
    }

    pub fn physics_spin_release(&mut self) {
        if self.spin_dir == 0.0 {
            return;
        }
        if let Some(phys) = self.physics.as_mut() {
            phys.set_spin(None);
        }
        self.spin_dir = 0.0;
        self.spin_piece = None;
        self.needs_render = true;
    }

    // ── Инструменты ────────────────────────────────────────────────

    pub fn tool_active(&self) -> bool {
        !matches!(self.tool, Tool::None)
    }

    /// Включает/выключает инструмент рисования.
    pub fn toggle_draw_tool(&mut self) {
        if self.transition.is_some() {
            return;
        }
        if matches!(self.tool, Tool::Draw { .. }) {
            self.tool = Tool::None;
        } else {
            if self.layout_mode == LayoutMode::Tiling {
                self.enter_physics_mode();
            }
            let material = self.last_material.unwrap_or_else(|| {
                Material::from_name(&self.config.drawing.default_material)
                    .unwrap_or(Material::Steel)
            });
            self.tool = Tool::Draw {
                material,
                stroke: Vec::new(),
                drawing: false,
                erasing: false,
            };
            println!("[tool] рисование: {}", material.name());
        }
        self.needs_render = true;
    }

    pub fn toggle_knife_tool(&mut self) {
        if self.transition.is_some() {
            return;
        }
        if matches!(self.tool, Tool::Knife { .. }) {
            self.tool = Tool::None;
        } else {
            if self.layout_mode == LayoutMode::Tiling {
                self.enter_physics_mode();
            }
            let p = (self.pointer_location.x, self.pointer_location.y);
            self.tool = Tool::Knife {
                start: None,
                end: p,
            };
        }
        self.needs_render = true;
    }

    /// Инструмент связи. Если активное окно есть, оно сразу становится
    /// первым (точка крепления — центр), иначе первое окно выбирается кликом.
    pub fn start_link_tool(&mut self, kind: LinkKind) {
        if self.transition.is_some() {
            return;
        }
        if let Tool::Link { kind: k, .. } = &self.tool {
            if *k == kind {
                self.tool = Tool::None;
                self.needs_render = true;
                return;
            }
        }
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        self.tool = Tool::Link { kind, first: None };
        self.needs_render = true;
    }

    /// Клавиши внутри активного инструмента (без Super). true — съедено.
    pub fn tool_key(&mut self, keysym: u32, evdev: u32) -> bool {
        use smithay::input::keyboard::keysyms as xkb;
        if keysym == xkb::KEY_Escape {
            if self.tool_active() {
                self.tool = Tool::None;
                self.needs_render = true;
                return true;
            }
            return false;
        }
        let Tool::Draw { material, .. } = &mut self.tool else {
            return false;
        };
        // Цифры — и по символу, и по физической клавише (AZERTY).
        let digit = |d: &str| {
            crate::config::ControlsConfig::matches_key(d, keysym)
                || crate::config::ControlsConfig::matches_digit_key(d, evdev)
        };
        let pick = match keysym {
            _ if digit("1") => Some(Material::Steel),
            _ if digit("2") => Some(Material::Glue),
            _ if digit("3") => Some(Material::Trampoline),
            _ if digit("4") => Some(Material::Rope),
            xkb::KEY_Tab => {
                let i = Material::ALL
                    .iter()
                    .position(|m| m == material)
                    .unwrap_or(0);
                Some(Material::ALL[(i + 1) % Material::ALL.len()])
            }
            _ => None,
        };
        if let Some(m) = pick {
            *material = m;
            self.last_material = Some(m);
            println!("[tool] материал: {}", m.name());
            self.needs_render = true;
            return true;
        }
        if keysym == xkb::KEY_Delete || keysym == xkb::KEY_BackSpace {
            if let Some(phys) = self.physics.as_mut() {
                phys.clear_lines();
            }
            self.needs_render = true;
            return true;
        }
        false
    }

    /// Кнопка мыши при активном инструменте. true — событие съедено.
    pub fn tool_button(&mut self, button: u32, pressed: bool, sx: f64, sy: f64) -> bool {
        const BTN_LEFT: u32 = 0x110;
        const BTN_RIGHT: u32 = 0x111;
        if !self.tool_active()
            || self.layout_mode != LayoutMode::Physics
            || self.transition.is_some()
        {
            return false;
        }
        let (wx, wy) = self.screen_to_world(sx, sy);
        let zoom = self.camera_zoom;
        let mut tool = std::mem::replace(&mut self.tool, Tool::None);
        let consumed = match &mut tool {
            Tool::Draw {
                material,
                stroke,
                drawing,
                erasing,
            } => {
                if button == BTN_LEFT {
                    if pressed {
                        *drawing = true;
                        stroke.clear();
                        stroke.push(v2(wx, wy));
                    } else if *drawing {
                        *drawing = false;
                        let pts = std::mem::take(stroke);
                        if let Some(phys) = self.physics.as_mut() {
                            if pts.len() >= 2 {
                                phys.add_line(*material, &pts, None);
                            }
                        }
                    }
                    true
                } else if button == BTN_RIGHT {
                    *erasing = pressed;
                    if pressed {
                        self.erase_line_at(wx, wy, zoom);
                    }
                    true
                } else {
                    false
                }
            }
            Tool::Knife { start, end } => {
                if button == BTN_LEFT {
                    if pressed {
                        *start = Some((sx, sy));
                        *end = (sx, sy);
                    } else if let Some(s) = start.take() {
                        self.knife_cut(s, (sx, sy));
                    }
                    true
                } else {
                    button == BTN_RIGHT
                }
            }
            Tool::Link { kind, first } => {
                if button == BTN_LEFT && pressed {
                    match self.piece_at(sx, sy) {
                        Some((piece, local)) => {
                            let anchor = self.anchor_for(piece, local);
                            match first.take() {
                                None => *first = Some(anchor),
                                Some(a) if a.piece == anchor.piece => *first = Some(a),
                                Some(a) => {
                                    let kind = *kind;
                                    if let Some(phys) = self.physics.as_mut() {
                                        if phys.add_link(kind, a, anchor).is_some() {
                                            println!("[link] {} создана", kind.name());
                                        }
                                    }
                                    self.needs_render = true;
                                    // Связь готова — инструмент закрывается.
                                    return true;
                                }
                            }
                        }
                        None => *first = None,
                    }
                    true
                } else {
                    button == BTN_LEFT || button == BTN_RIGHT
                }
            }
            Tool::None => false,
        };
        self.tool = tool;
        self.needs_render = true;
        consumed
    }

    /// Движение мыши при активном инструменте. true — событие съедено.
    pub fn tool_motion(&mut self, sx: f64, sy: f64) -> bool {
        if !self.tool_active() || self.layout_mode != LayoutMode::Physics {
            return false;
        }
        let (wx, wy) = self.screen_to_world(sx, sy);
        let spacing =
            (self.config.drawing.min_point_spacing.max(1.0) as f64 / self.camera_zoom) as f32;
        let max_points = self.config.drawing.max_points.max(2) as usize;
        let zoom = self.camera_zoom;
        let mut erase = false;
        match &mut self.tool {
            Tool::Draw {
                stroke,
                drawing,
                erasing,
                ..
            } => {
                if *drawing && stroke.len() < max_points {
                    let p = v2(wx, wy);
                    if stroke.last().map_or(true, |l| (p - *l).length() >= spacing) {
                        stroke.push(p);
                    }
                }
                erase = *erasing;
            }
            Tool::Knife { end, .. } => *end = (sx, sy),
            _ => {}
        }
        if erase {
            self.erase_line_at(wx, wy, zoom);
        }
        self.needs_render = true;
        // Курсор всё равно должен двигаться у клиентов только вне рисования —
        // пусть указатель продолжает обновляться обычным путём.
        false
    }

    fn erase_line_at(&mut self, wx: f64, wy: f64, zoom: f64) {
        if let Some(phys) = self.physics.as_mut() {
            if let Some(id) = phys.line_at(v2(wx, wy), (8.0 / zoom) as f32) {
                phys.remove_line(id);
                self.needs_render = true;
            }
        }
    }

    /// Точка крепления на куске: координаты окна → локальные (от центра куска).
    fn anchor_for(&self, piece: u32, local: Point<f64, Logical>) -> Anchor {
        let center = self.piece_by_id(piece).map_or((0.0, 0.0), |p| p.center());
        Anchor {
            piece,
            local: v2(local.x - center.0, local.y - center.1),
        }
    }

    // ── Нож (концепт §7) ───────────────────────────────────────────

    /// Режет верхнее окно, через которое прошёл отрезок (экранные координаты).
    /// Разрез выровнен по осям окна: части — прямоугольные кадры одного
    /// буфера, как у `wp_viewporter`, приложение продолжает работать целиком.
    fn knife_cut(&mut self, s0: (f64, f64), s1: (f64, f64)) {
        if (s1.0 - s0.0).hypot(s1.1 - s0.1) < 8.0 {
            return;
        }
        let min_piece = self.config.slicing.min_piece_size.max(1.0) as f64;
        let gap = self.config.slicing.cut_gap.max(0.0) as f64;
        let candidates: Vec<Window> = self
            .space
            .elements()
            .rev()
            .filter(|w| !Self::is_override_redirect(w))
            .cloned()
            .collect();
        for window in candidates {
            let pieces: Vec<Piece> = self.pieces_of(&window).cloned().collect();
            let mut cut_any = false;
            for piece in pieces {
                let Some(inv) = self.piece_xf(&piece).and_then(|x| x.inverse()) else {
                    continue;
                };
                let a = inv.apply(s0.0, s0.1);
                let b = inv.apply(s1.0, s1.1);
                let Some((t0, t1)) = clip_segment(a, b, piece.rect) else {
                    continue;
                };
                if t1 - t0 < 1e-6 {
                    continue;
                }
                let p0 = (a.0 + (b.0 - a.0) * t0, a.1 + (b.1 - a.1) * t0);
                let p1 = (a.0 + (b.0 - a.0) * t1, a.1 + (b.1 - a.1) * t1);
                let r = piece.rect;
                // Горизонтальный штрих → горизонтальный разрез, и наоборот.
                let horizontal = (b.0 - a.0).abs() >= (b.1 - a.1).abs();
                let (r1, r2) = if horizontal {
                    let y = ((p0.1 + p1.1) * 0.5).round();
                    if y - r.loc.y < min_piece || r.loc.y + r.size.h - y < min_piece {
                        continue;
                    }
                    (
                        Rectangle::new(r.loc, (r.size.w, y - r.loc.y).into()),
                        Rectangle::new(
                            (r.loc.x, y).into(),
                            (r.size.w, r.loc.y + r.size.h - y).into(),
                        ),
                    )
                } else {
                    let x = ((p0.0 + p1.0) * 0.5).round();
                    if x - r.loc.x < min_piece || r.loc.x + r.size.w - x < min_piece {
                        continue;
                    }
                    (
                        Rectangle::new(r.loc, (x - r.loc.x, r.size.h).into()),
                        Rectangle::new(
                            (x, r.loc.y).into(),
                            (r.loc.x + r.size.w - x, r.size.h).into(),
                        ),
                    )
                };
                self.split_piece(&piece, r1, r2, horizontal, gap);
                cut_any = true;
            }
            if cut_any {
                println!(
                    "[knife] окно разрезано, кусков: {}",
                    self.pieces_of(&window).count()
                );
                self.sync_space_positions();
                self.needs_render = true;
                return;
            }
        }
    }

    fn split_piece(
        &mut self,
        piece: &Piece,
        r1: Rectangle<f64, Logical>,
        r2: Rectangle<f64, Logical>,
        horizontal: bool,
        gap: f64,
    ) {
        let Some(phys) = self.physics.as_ref() else {
            return;
        };
        let Some((c, angle)) = phys.piece_transform(piece.id) else {
            return;
        };
        let (linvel, angvel) = phys.piece_velocity(piece.id).unwrap_or((Vector::ZERO, 0.0));
        let floating = phys.is_floating(piece.id);
        let weight = phys.piece_weight(piece.id);
        let (pc_x, pc_y) = piece.center();
        let world_of = |r: &Rectangle<f64, Logical>, sign: f64| -> Vector {
            let rc = (r.loc.x + r.size.w * 0.5, r.loc.y + r.size.h * 0.5);
            let mut d = (rc.0 - pc_x, rc.1 - pc_y);
            // Части чуть расходятся по нормали к разрезу.
            if horizontal {
                d.1 += sign * gap * 0.5;
            } else {
                d.0 += sign * gap * 0.5;
            }
            c + crate::physics::rotate(v2(d.0, d.1), angle)
        };
        let c1 = world_of(&r1, -1.0);
        let c2 = world_of(&r2, 1.0);
        let window = piece.window.clone();
        let links = self.take_links(&[piece.id]);
        // Разрезанная сетка теряет смысл — части снова жёсткие.
        self.drop_soft(piece.id);
        if let Some(phys) = self.physics.as_mut() {
            phys.remove_piece(piece.id);
        }
        self.pieces.retain(|p| p.id != piece.id);
        for (rect, center) in [(r1, c1), (r2, c2)] {
            let id = self.spawn_piece(&window, rect, center, angle, weight);
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_velocity(id, linvel, angvel);
                if floating {
                    phys.set_floating(id, true);
                }
            }
        }
        self.restore_links(&window, links);
    }

    /// Связи, в которых участвуют куски `ids`: (вид, концы в координатах окна
    /// или чужой якорь). Нужно, чтобы пережить пересоздание кусков.
    fn take_links(&mut self, ids: &[u32]) -> Vec<(LinkKind, [LinkEnd; 2])> {
        let Some(phys) = self.physics.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for link in phys.link_views() {
            if !ids.contains(&link.a.piece) && !ids.contains(&link.b.piece) {
                continue;
            }
            let end = |a: &Anchor| -> LinkEnd {
                if ids.contains(&a.piece) {
                    let c = self.piece_by_id(a.piece).map_or((0.0, 0.0), |p| p.center());
                    LinkEnd::Window(c.0 + a.local.x as f64, c.1 + a.local.y as f64)
                } else {
                    LinkEnd::Anchor(*a)
                }
            };
            out.push((link.kind, [end(&link.a), end(&link.b)]));
        }
        out
    }

    /// Восстанавливает связи после разреза/склейки: точка крепления в окне
    /// переходит к куску, в который она попала (или к ближайшему).
    fn restore_links(&mut self, window: &Window, links: Vec<(LinkKind, [LinkEnd; 2])>) {
        let pieces: Vec<Piece> = self.pieces_of(window).cloned().collect();
        let resolve = |end: &LinkEnd| -> Option<Anchor> {
            match *end {
                LinkEnd::Anchor(a) => Some(a),
                LinkEnd::Window(x, y) => {
                    let piece = pieces.iter().min_by(|a, b| {
                        let d = |p: &Piece| {
                            let r = p.rect;
                            let dx = (r.loc.x - x).max(x - (r.loc.x + r.size.w)).max(0.0);
                            let dy = (r.loc.y - y).max(y - (r.loc.y + r.size.h)).max(0.0);
                            dx * dx + dy * dy
                        };
                        d(a).total_cmp(&d(b))
                    })?;
                    let c = piece.center();
                    Some(Anchor {
                        piece: piece.id,
                        local: v2(x - c.0, y - c.1),
                    })
                }
            }
        };
        let resolved: Vec<(LinkKind, Anchor, Anchor)> = links
            .iter()
            .filter_map(|(kind, [a, b])| Some((*kind, resolve(a)?, resolve(b)?)))
            .collect();
        if let Some(phys) = self.physics.as_mut() {
            for (kind, a, b) in resolved {
                phys.add_link(kind, a, b);
            }
        }
    }

    /// Super+Z: склеивает разрезанное окно обратно в один кусок.
    pub fn restore_focused_window(&mut self) {
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let pieces: Vec<Piece> = self.pieces_of(&window).cloned().collect();
        // Форма: деформацию сбрасываем, мягкость окна сохраняется.
        for p in &pieces {
            if let Some(soft) = self.soft.get_mut(&p.id) {
                soft.mesh.reset();
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_hull(p.id, None);
            }
        }
        self.needs_render = true;
        if pieces.len() <= 1 {
            return;
        }
        for p in &pieces {
            self.drop_soft(p.id);
        }
        let Some(main) = self.main_piece(&window).cloned() else {
            return;
        };
        let Some((c, angle)) = self
            .physics
            .as_ref()
            .and_then(|p| p.piece_transform(main.id))
        else {
            return;
        };
        let floating = self.window_floating(&window);
        let weight = self
            .physics
            .as_ref()
            .map_or(1.0, |p| p.piece_weight(main.id));
        let size = window.geometry().size;
        let full = Rectangle::new((0.0, 0.0).into(), (size.w as f64, size.h as f64).into());
        // Целое окно ставим так, чтобы основной кусок остался на месте.
        let (mx, my) = main.center();
        let d = (full.size.w * 0.5 - mx, full.size.h * 0.5 - my);
        let center = c + crate::physics::rotate(v2(d.0, d.1), angle);
        let ids: Vec<u32> = pieces.iter().map(|p| p.id).collect();
        // Связи между кусками одного окна после склейки теряют смысл.
        let links: Vec<_> = self
            .take_links(&ids)
            .into_iter()
            .filter(|(_, ends)| ends.iter().any(|e| matches!(e, LinkEnd::Anchor(_))))
            .collect();
        for p in &pieces {
            if let Some(phys) = self.physics.as_mut() {
                phys.remove_piece(p.id);
            }
        }
        self.pieces.retain(|p| p.window != window);
        let id = self.spawn_piece(&window, full, center, angle, weight);
        if floating {
            if let Some(phys) = self.physics.as_mut() {
                phys.set_floating(id, true);
            }
        }
        self.restore_links(&window, links);
        self.sync_space_positions();
        self.needs_render = true;
    }

    /// Super+U: снять с активного окна связи, клей и подвесы.
    pub fn unlink_focused(&mut self) {
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let ids: Vec<u32> = self.pieces_of(&window).map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.unlink_piece(id);
            }
        }
        self.needs_render = true;
    }

    // ── Сессия (концепт §3) ────────────────────────────────────────

    /// Снимок рабочего пространства для сохранения.
    pub fn session_snapshot(&self) -> crate::session::SessionData {
        use crate::session::{
            SavedAnchor, SavedJoint, SavedLine, SavedPiece, SavedWindow, SessionData,
        };
        let mut data = SessionData {
            layout_mode: match self.layout_mode {
                LayoutMode::Tiling => "tiling".into(),
                LayoutMode::Physics => "physics".into(),
            },
            camera: if self.layout_mode == LayoutMode::Physics && self.transition.is_none() {
                [self.camera_offset.0, self.camera_offset.1, self.camera_zoom]
            } else {
                [
                    self.saved_camera.0,
                    self.saved_camera.1,
                    self.saved_camera.2,
                ]
            },
            ..Default::default()
        };
        let Some(phys) = self.physics.as_ref() else {
            data.layout_mode = "tiling".into();
            return data;
        };
        data.gravity_mode = if phys.zero_gravity() {
            "zero"
        } else {
            "normal"
        }
        .into();
        data.floor_y = Some(phys.floor_y());

        // Окна в порядке стека; запоминаем индекс каждого куска для связей.
        let mut piece_index: HashMap<u32, (usize, usize)> = HashMap::new();
        for window in self.space.elements() {
            if Self::is_override_redirect(window) {
                continue;
            }
            let pieces: Vec<&Piece> = self.pieces_of(window).collect();
            let size = window.geometry().size;
            let pid = window_pid(&self.display_handle, window);
            let mut saved = SavedWindow {
                app_id: window_app_id(window),
                title: window_title(window),
                x11: window.x11_surface().is_some(),
                command: pid
                    .and_then(crate::session::process_command)
                    .unwrap_or_default(),
                cwd: pid.and_then(crate::session::process_cwd),
                pid: pid.unwrap_or(0),
                size: [size.w, size.h],
                weight: pieces.first().map_or(1.0, |p| phys.piece_weight(p.id)),
                softness: pieces
                    .iter()
                    .find_map(|p| self.soft.get(&p.id))
                    .map_or(0.0, |s| s.params.softness.max(0.01)),
                ..Default::default()
            };
            for (i, p) in pieces.iter().enumerate() {
                let Some((c, angle)) = phys.piece_transform(p.id) else {
                    continue;
                };
                piece_index.insert(p.id, (data.windows.len(), i));
                saved.pieces.push(SavedPiece {
                    rect: [p.rect.loc.x, p.rect.loc.y, p.rect.size.w, p.rect.size.h],
                    center: [c.x, c.y],
                    angle,
                    floating: phys.is_floating(p.id),
                });
            }
            if saved.pieces.is_empty() {
                // Окно ещё не было в физике: центр — его место в тайлинге.
                let geo = self.space.element_geometry(window).unwrap_or_default();
                saved.pieces.push(SavedPiece {
                    rect: [0.0, 0.0, size.w as f64, size.h as f64],
                    center: [
                        geo.loc.x as f32 + geo.size.w as f32 * 0.5,
                        geo.loc.y as f32 + geo.size.h as f32 * 0.5,
                    ],
                    ..Default::default()
                });
            }
            data.windows.push(saved);
        }
        for line in phys.line_views() {
            data.lines.push(SavedLine {
                material: line.material.name().into(),
                points: line.points.iter().map(|p| [p.x, p.y]).collect(),
                pinned: if line.material == Material::Rope {
                    line.pinned.to_vec()
                } else {
                    Vec::new()
                },
            });
        }
        for link in phys.link_views() {
            let (Some(a), Some(b)) = (
                piece_index.get(&link.a.piece),
                piece_index.get(&link.b.piece),
            ) else {
                continue;
            };
            data.joints.push(SavedJoint {
                kind: link.kind.name().into(),
                a: SavedAnchor {
                    window: a.0,
                    piece: a.1,
                    local: [link.a.local.x, link.a.local.y],
                },
                b: SavedAnchor {
                    window: b.0,
                    piece: b.1,
                    local: [link.b.local.x, link.b.local.y],
                },
            });
        }
        data
    }

    fn session_file(&self) -> (std::path::PathBuf, crate::session::SessionFormat) {
        let format = crate::session::SessionFormat::from_config(&self.config.session.format);
        (
            crate::session::session_path(&self.config.session.save_path, format),
            format,
        )
    }

    /// Сохраняет сессию (выход, автосохранение, Super+F5).
    pub fn save_session(&mut self) {
        if !self.config.session.enabled {
            return;
        }
        let data = self.session_snapshot();
        let (path, format) = self.session_file();
        match crate::session::save(&path, format, &data) {
            Ok(()) => println!(
                "[session] сохранено: {} ({} окон, {} линий, {} связей)",
                path.display(),
                data.windows.len(),
                data.lines.len(),
                data.joints.len()
            ),
            Err(e) => eprintln!("[session] не удалось сохранить {}: {e}", path.display()),
        }
        if let Some(a) = self.autosave.as_mut() {
            a.reset(Instant::now());
        }
    }

    pub fn maybe_autosave(&mut self, now: Instant) {
        self.maybe_relaunch(now);
        if self.autosave.as_mut().map_or(false, |a| a.due(now)) {
            self.save_session();
        }
        if let Some(r) = self.restorer.as_mut() {
            r.expire(now);
            if !r.is_pending() {
                self.restorer = None;
                self.pending_joints.clear();
            }
        }
    }

    /// Вызывается, когда у бэкенда появился выход: загрузка сессии,
    /// линии/связи, режим, перезапуск приложений.
    pub fn on_output_ready(&mut self) {
        if self.session_loaded {
            return;
        }
        self.session_loaded = true;
        let cfg = self.config.session.clone();
        if !cfg.enabled {
            return;
        }
        if cfg.autosave && cfg.autosave_interval_secs > 0 {
            self.autosave = Some(crate::session::Autosave::new(
                std::time::Duration::from_secs(cfg.autosave_interval_secs as u64),
                Instant::now(),
            ));
        }
        let (path, format) = self.session_file();
        let Some(data) = crate::session::load(&path, format) else {
            return;
        };
        println!(
            "[session] загружено: {} ({} окон, {} линий, {} связей)",
            path.display(),
            data.windows.len(),
            data.lines.len(),
            data.joints.len()
        );
        self.restored_floor_y = data.floor_y;
        self.saved_camera = (
            data.camera[0],
            data.camera[1],
            data.camera[2].clamp(self.config.physics.zoom_min, self.config.physics.zoom_max),
        );
        {
            let phys = self.ensure_physics();
            for line in &data.lines {
                let Some(material) = Material::from_name(&line.material) else {
                    continue;
                };
                let pts: Vec<Vector> = line
                    .points
                    .iter()
                    .map(|p| Vector::new(p[0], p[1]))
                    .collect();
                let pinned = (line.pinned.len() == 2).then(|| [line.pinned[0], line.pinned[1]]);
                phys.add_line(material, &pts, pinned);
            }
            if data.gravity_mode == "zero" {
                phys.set_zero_gravity(true);
            }
        }
        self.pending_joints = data.joints.clone();
        self.restorer = Some(crate::session::SessionRestorer::new(
            &data,
            Instant::now(),
            std::time::Duration::from_secs(cfg.restore_timeout_secs.max(1) as u64),
        ));
        self.restored_windows.clear();
        if data.layout_mode == "physics" {
            self.enter_physics_mode();
        }
        if cfg.restore_apps {
            // Запуск откладывается до готовности XWayland (DISPLAY), иначе
            // восстановленные X11-приложения (Steam) стартуют без X-сервера.
            self.pending_relaunch = crate::session::relaunch_commands(&data);
            self.relaunch_deadline = Some(Instant::now() + std::time::Duration::from_secs(5));
        }
    }

    /// Запускает приложения из сессии, как только XWayland готов (или через
    /// 5 секунд без него).
    fn maybe_relaunch(&mut self, now: Instant) {
        let Some(deadline) = self.relaunch_deadline else {
            return;
        };
        if self.xdisplay.is_none() && now < deadline {
            return;
        }
        self.relaunch_deadline = None;
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        for (argv, cwd) in std::mem::take(&mut self.pending_relaunch) {
            if let Err(e) = crate::session::spawn(&argv, cwd.as_deref(), &display) {
                eprintln!("[session] не удалось запустить {argv:?}: {e}");
            }
        }
    }

    /// Новое окно — одно из сохранённых? Тогда куски встают на старые места.
    fn try_restore_window(&mut self, window: &Window) -> bool {
        let Some(restorer) = self.restorer.as_mut() else {
            return false;
        };
        let pid = window_pid(&self.display_handle, window);
        let command = pid.and_then(crate::session::process_command);
        let Some((index, saved)) = restorer.claim(
            &window_app_id(window),
            &window_title(window),
            window.x11_surface().is_some(),
            command.as_deref(),
        ) else {
            return false;
        };
        let size = window.geometry().size;
        let (sx, sy) = (
            if saved.size[0] > 0 {
                size.w as f64 / saved.size[0] as f64
            } else {
                1.0
            },
            if saved.size[1] > 0 {
                size.h as f64 / saved.size[1] as f64
            } else {
                1.0
            },
        );
        let mut ids = Vec::new();
        for p in &saved.pieces {
            if !(p.center[0].is_finite() && p.center[1].is_finite()) {
                ids.clear();
                break;
            }
            let rect = Rectangle::new(
                (p.rect[0] * sx, p.rect[1] * sy).into(),
                ((p.rect[2] * sx).max(1.0), (p.rect[3] * sy).max(1.0)).into(),
            );
            let id = self.spawn_piece(
                window,
                rect,
                Vector::new(p.center[0], p.center[1]),
                p.angle,
                saved.weight,
            );
            if p.floating {
                if let Some(phys) = self.physics.as_mut() {
                    phys.set_floating(id, true);
                }
            }
            if self.layout_mode == LayoutMode::Tiling || self.transition.is_some() {
                if let Some(phys) = self.physics.as_mut() {
                    phys.set_piece_enabled(id, false);
                }
            }
            ids.push(id);
        }
        if ids.is_empty() {
            return false;
        }
        if saved.softness > 0.0 {
            let mut params = self.soft_params(window);
            params.softness = saved.softness;
            for &id in &ids {
                self.make_soft(id, params);
            }
        }
        println!("[session] восстановлено окно {:?}", saved.app_id);
        self.restored_windows.insert(index, ids);
        self.restore_pending_joints();
        true
    }

    /// Связи, оба конца которых уже восстановлены.
    fn restore_pending_joints(&mut self) {
        let mut rest = Vec::new();
        for joint in std::mem::take(&mut self.pending_joints) {
            let a = self
                .restored_windows
                .get(&joint.a.window)
                .and_then(|ids| ids.get(joint.a.piece))
                .copied();
            let b = self
                .restored_windows
                .get(&joint.b.window)
                .and_then(|ids| ids.get(joint.b.piece))
                .copied();
            match (a, b, LinkKind::from_name(&joint.kind)) {
                (Some(a), Some(b), Some(kind)) => {
                    if let Some(phys) = self.physics.as_mut() {
                        phys.add_link(
                            kind,
                            Anchor {
                                piece: a,
                                local: Vector::new(joint.a.local[0], joint.a.local[1]),
                            },
                            Anchor {
                                piece: b,
                                local: Vector::new(joint.b.local[0], joint.b.local[1]),
                            },
                        );
                    }
                }
                (_, _, Some(_)) => rest.push(joint),
                _ => {}
            }
        }
        self.pending_joints = rest;
    }

    /// Восстановить место окна из сессии, не включая физику (тайлинг).
    pub fn restore_window_for_later(&mut self, window: &Window) {
        // Одна попытка на окно: сопоставление читает /proc, а видеоплеер
        // коммитит по 60 раз в секунду.
        if self.restore_tried.insert(window.clone()) {
            self.try_restore_window(window);
        }
    }

    // ── Мягкие тела (концепт §5) ───────────────────────────────────

    /// Параметры мягкости для окна: конфиг + override по app_id.
    fn soft_params(&self, window: &Window) -> crate::softbody::SoftParams {
        let cfg = &self.config.soft_body;
        let softness = self
            .window_override(window)
            .softness
            .filter(|s| *s > 0.0)
            .unwrap_or(cfg.softness);
        crate::softbody::SoftParams {
            softness: softness.clamp(0.0, 1.0),
            impact_squash: cfg.impact_squash.clamp(0.0, 1.0),
            plasticity: cfg.plasticity.clamp(0.0, 1.0),
        }
    }

    /// Делает кусок мягким (если ещё не).
    fn make_soft(&mut self, piece: u32, params: crate::softbody::SoftParams) {
        if self.soft.contains_key(&piece) {
            return;
        }
        let Some(rect) = self.piece_by_id(piece).map(|p| p.rect) else {
            return;
        };
        let mesh = crate::softbody::SoftMesh::new(
            rect.size.w as f32,
            rect.size.h as f32,
            self.config.soft_body.mesh_resolution.clamp(1, 32),
        );
        let rest_generation = mesh.rest_generation();
        self.soft.insert(
            piece,
            SoftPiece {
                mesh,
                params,
                prev_vel: None,
                rest_generation,
            },
        );
    }

    fn drop_soft(&mut self, piece: u32) {
        if self.soft.remove(&piece).is_some() {
            if self.soft_grab.as_ref().map_or(false, |(p, _)| *p == piece) {
                self.soft_grab = None;
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_hull(piece, None);
            }
        }
    }

    /// Super+J: мягкое ↔ жёсткое окно.
    pub fn toggle_soft_focused(&mut self) {
        if self.layout_mode == LayoutMode::Tiling {
            self.enter_physics_mode();
        }
        let Some(window) = self.get_focused_window() else {
            return;
        };
        let ids: Vec<u32> = self.pieces_of(&window).map(|p| p.id).collect();
        let soft = ids.iter().any(|id| self.soft.contains_key(id));
        let params = self.soft_params(&window);
        for id in ids {
            if soft {
                self.drop_soft(id);
            } else {
                self.make_soft(id, params);
            }
        }
        println!(
            "[soft] окно {:?}: {}",
            window_app_id(&window),
            if soft {
                "жёсткое"
            } else {
                "мягкое"
            }
        );
        self.needs_render = true;
    }

    /// Super+Shift+ЛКМ: схватить окно за точку и тянуть (деформация).
    pub fn soft_grab_begin(&mut self, sx: f64, sy: f64) -> Option<Window> {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return None;
        }
        let (piece_id, _) = self.piece_at(sx, sy)?;
        let piece = self.piece_by_id(piece_id)?.clone();
        let params = self.soft_params(&piece.window);
        self.make_soft(piece_id, params);
        let local = self.rigid_local(&piece, sx, sy)?;
        let radius = (piece.rect.size.w.max(piece.rect.size.h)) as f32;
        let soft = self.soft.get_mut(&piece_id)?;
        let grab = soft.mesh.grab(local, radius)?;
        self.soft_grab = Some((piece_id, grab));
        self.last_physics_tick.get_or_insert_with(Instant::now);
        self.needs_render = true;
        Some(piece.window)
    }

    pub fn soft_grab_update(&mut self, sx: f64, sy: f64) {
        let Some((piece_id, _)) = self.soft_grab.as_ref() else {
            return;
        };
        let piece_id = *piece_id;
        let Some(piece) = self.piece_by_id(piece_id).cloned() else {
            self.soft_grab = None;
            return;
        };
        let Some(target) = self.rigid_local(&piece, sx, sy) else {
            return;
        };
        if let (Some((_, grab)), Some(soft)) =
            (self.soft_grab.as_ref(), self.soft.get_mut(&piece_id))
        {
            let params = soft.params;
            soft.mesh.drag(grab, target, &params);
        }
        self.needs_render = true;
    }

    pub fn soft_grab_end(&mut self) {
        if let Some((piece, grab)) = self.soft_grab.take() {
            if let Some(soft) = self.soft.get_mut(&piece) {
                soft.mesh.release(grab);
            }
            self.needs_render = true;
        }
    }

    pub fn soft_grab_active(&self) -> bool {
        self.soft_grab.is_some()
    }

    /// Экран → локальные координаты куска (от центра) без учёта сетки.
    fn rigid_local(&self, piece: &Piece, sx: f64, sy: f64) -> Option<[f32; 2]> {
        let inv = self.piece_xf(piece)?.inverse()?;
        let (x, y) = inv.apply(sx, sy);
        let (cx, cy) = piece.center();
        Some([(x - cx) as f32, (y - cy) as f32])
    }

    /// Шаг мягких тел за кадр: инерционные псевдосилы из ускорения тела.
    /// Возвращает true, пока сетки ещё колеблются.
    fn step_soft(&mut self, dt: f32) -> bool {
        if self.soft.is_empty() || dt <= 0.0 {
            return false;
        }
        let Some(phys) = self.physics.as_mut() else {
            return false;
        };
        let mut active = false;
        let mut hulls = Vec::new();
        for (&id, soft) in self.soft.iter_mut() {
            let (Some((vel, angvel)), Some((_, angle))) =
                (phys.piece_velocity(id), phys.piece_transform(id))
            else {
                continue;
            };
            // Собственное ускорение (a − g): в свободном падении сетка не
            // деформируется, при ударе об пол — сплющивается, в покое — оседает.
            let accel = soft.prev_vel.map_or(Vector::ZERO, |prev| (vel - prev) / dt);
            soft.prev_vel = Some(vel);
            let proper = accel - phys.piece_gravity(id);
            let local = crate::physics::rotate(proper, -angle);
            soft.mesh.step(dt, [local.x, local.y], angvel, &soft.params);
            active |= soft.mesh.is_active();
            let generation = soft.mesh.rest_generation();
            if generation != soft.rest_generation {
                soft.rest_generation = generation;
                let hull: Vec<Vector> = soft
                    .mesh
                    .rest_hull()
                    .into_iter()
                    .map(|p| Vector::new(p[0], p[1]))
                    .collect();
                hulls.push((id, hull));
            }
        }
        for (id, hull) in hulls {
            // Вытянутое в треугольник окно и сталкивается как треугольник.
            phys.set_piece_hull(id, Some(&hull));
        }
        active || self.soft_grab.is_some()
    }

    /// Сетка для отрисовки, если кусок деформирован.
    fn mesh_draw(&self, piece: &Piece, xf: &Affine) -> Option<crate::render::MeshDraw> {
        let soft = self.soft.get(&piece.id)?;
        if !(soft.mesh.is_deformed() || soft.mesh.is_active()) {
            return None;
        }
        let (cx, cy) = piece.center();
        let size = piece.window.geometry().size;
        let (gw, gh) = (size.w.max(1) as f64, size.h.max(1) as f64);
        let r = piece.rect;
        let mut fingerprint: u64 = 0xcbf2_9ce4_8422_2325;
        let positions: Vec<[f32; 2]> = soft
            .mesh
            .positions()
            .iter()
            .map(|p| {
                let (x, y) = xf.apply(cx + p[0] as f64, cy + p[1] as f64);
                for v in [x as f32, y as f32] {
                    fingerprint ^= v.to_bits() as u64;
                    fingerprint = fingerprint.wrapping_mul(0x0100_0000_01b3);
                }
                [x as f32, y as f32]
            })
            .collect();
        let geometry_uv = soft
            .mesh
            .uvs()
            .iter()
            .map(|uv| {
                [
                    ((r.loc.x + uv[0] as f64 * r.size.w) / gw) as f32,
                    ((r.loc.y + uv[1] as f64 * r.size.h) / gh) as f32,
                ]
            })
            .collect();
        Some(crate::render::MeshDraw {
            positions,
            geometry_uv,
            indices: soft.mesh.indices().to_vec(),
            fingerprint,
        })
    }

    /// Все id кусков (для проверки целостности в тестах/отладке).
    #[allow(dead_code)]
    pub fn piece_ids(&self) -> HashSet<u32> {
        self.pieces.iter().map(|p| p.id).collect()
    }
}

/// app_id (Wayland) или WM_CLASS class (X11).
pub fn window_app_id(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.class();
    }
    window
        .toplevel()
        .and_then(|t| {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
            })
        })
        .unwrap_or_default()
}

pub fn window_title(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.title();
    }
    window
        .toplevel()
        .and_then(|t| {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok().and_then(|d| d.title.clone()))
            })
        })
        .unwrap_or_default()
}

/// PID процесса-владельца окна.
pub fn window_pid(
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    window: &Window,
) -> Option<u32> {
    if let Some(x11) = window.x11_surface() {
        return x11.pid();
    }
    let surface = window.wl_surface()?;
    let client = dh.get_client(surface.id()).ok()?;
    client.get_credentials(dh).ok().map(|c| c.pid as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_segment_crosses_rect() {
        let r = Rectangle::new((0.0, 0.0).into(), (100.0, 50.0).into());
        let (t0, t1) = clip_segment((-50.0, 25.0), (150.0, 25.0), r).unwrap();
        assert!((t0 - 0.25).abs() < 1e-9 && (t1 - 0.75).abs() < 1e-9);
        assert!(clip_segment((-50.0, 80.0), (150.0, 80.0), r).is_none());
    }

    #[test]
    fn wrap_angles() {
        assert!((wrap_angle(3.0 * std::f64::consts::PI) - std::f64::consts::PI).abs() < 1e-9);
        assert!((wrap_angle(-0.5) + 0.5).abs() < 1e-12);
    }
}
