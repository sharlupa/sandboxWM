//! Куски окон: тела в физическом мире, спавн и удаление окон.

use rapier2d::math::Vector;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use crate::overlay::to_screen;
use crate::render::Affine;
use crate::state::{AppState, LayoutMode};

use super::{Piece, ScreenPose, Tool, shape, v2, window_app_id};

impl AppState {
    pub fn pieces_of(&self, window: &Window) -> impl Iterator<Item = &Piece> + '_ {
        let window = window.clone();
        self.pieces.iter().filter(move |p| p.window == window)
    }

    pub(super) fn piece_by_id(&self, id: u32) -> Option<&Piece> {
        self.pieces.iter().find(|p| p.id == id)
    }

    /// Основной кусок окна (самый большой) — с ним рисуются попапы.
    pub(super) fn main_piece(&self, window: &Window) -> Option<&Piece> {
        self.pieces_of(window)
            .max_by(|a, b| a.area().total_cmp(&b.area()))
    }

    /// Координаты окна → экран для куска.
    pub(super) fn piece_xf(&self, piece: &Piece) -> Option<Affine> {
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

    pub(super) fn piece_screen_pose(&self, piece: &Piece) -> Option<ScreenPose> {
        let phys = self.physics.as_ref()?;
        let (c, angle) = phys.piece_transform(piece.id)?;
        Some(ScreenPose {
            cx: (c.x as f64 - self.camera_offset.0) * self.camera_zoom,
            cy: (c.y as f64 - self.camera_offset.1) * self.camera_zoom,
            angle: angle as f64,
            scale: self.camera_zoom,
        })
    }

    pub(super) fn is_override_redirect(window: &Window) -> bool {
        window
            .x11_surface()
            .map_or(false, |x| x.is_override_redirect())
    }

    /// Свойства окна из конфига по app_id / X11-классу.
    pub(super) fn window_override(&self, window: &Window) -> crate::config::WindowOverride {
        self.config.window_override(&window_app_id(window))
    }

    /// Создаёт тело для окна, которое впервые отрисовалось в физике.
    pub fn physics_spawn_window(&mut self, window: &Window) {
        // Полноэкранное окно получит тело, когда выйдет из полноэкранного режима.
        if Self::is_override_redirect(window)
            || self.pieces_of(window).next().is_some()
            || self.is_fullscreen(window)
        {
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
        self.spawn_piece(
            window,
            rect,
            None,
            v2(x, y),
            0.0,
            props.weight.unwrap_or(1.0),
        );
        if props.floating.unwrap_or(false) {
            self.set_window_floating(window, true);
        }
        let body = props.body_kind();
        if body != crate::config::BodyKind::Rigid {
            self.set_window_body(window, body);
        }
        self.sync_space_positions();
    }

    /// Тело куска окна: охват `rect` (координаты окна) с центром в мировой
    /// точке `center`; `shape` — форма косо разрезанного куска.
    pub(super) fn spawn_piece(
        &mut self,
        window: &Window,
        rect: Rectangle<f64, Logical>,
        shape: Option<Vec<shape::Pt>>,
        center: Vector,
        angle: f32,
        weight: f32,
    ) -> u32 {
        // X11 не умеет повёрнутые окна: глобальные координаты кликов
        // (Steam/CEF) съезжают. Блокируем вращение.
        let lock = window.x11_surface().is_some();
        let outline = body_outline(rect, shape.as_deref());
        let id = self.ensure_physics().spawn_piece(
            center,
            angle,
            rect.size.w as f32,
            rect.size.h as f32,
            outline,
            weight,
            lock,
        );
        self.pieces.push(Piece {
            id,
            window: window.clone(),
            rect,
            shape,
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
        // Полноэкранный размер — временный: тело остаётся прежним.
        if self.layout_mode != LayoutMode::Physics
            || self.transition.is_some()
            || self.is_fullscreen(window)
        {
            return;
        }
        let size = window.geometry().size;
        if size.w <= 1 || size.h <= 1 {
            return;
        }
        let pieces: Vec<(u32, Rectangle<f64, Logical>, Option<Vec<shape::Pt>>)> = self
            .pieces_of(window)
            .map(|p| (p.id, p.rect, p.shape.clone()))
            .collect();
        if pieces.is_empty() {
            return;
        }
        let bounds = pieces
            .iter()
            .map(|(_, r, _)| *r)
            .reduce(|a, b| a.merge(b))
            .unwrap_or_default();
        let (w, h) = (size.w as f64, size.h as f64);
        if bounds.loc == Point::from((0.0, 0.0)) && bounds.size == (w, h).into() {
            return;
        }
        let sx = w / bounds.size.w.max(1.0);
        let sy = h / bounds.size.h.max(1.0);
        let single = pieces.len() == 1;
        for (id, r, shape) in pieces {
            let scale = |p: shape::Pt| [(p[0] - bounds.loc.x) * sx, (p[1] - bounds.loc.y) * sy];
            let (rect, shape) = if single && shape.is_none() {
                (Rectangle::new((0.0, 0.0).into(), (w, h).into()), None)
            } else {
                (
                    Rectangle::new(
                        ((r.loc.x - bounds.loc.x) * sx, (r.loc.y - bounds.loc.y) * sy).into(),
                        (r.size.w * sx, r.size.h * sy).into(),
                    ),
                    shape.map(|poly| poly.into_iter().map(scale).collect::<Vec<_>>()),
                )
            };
            let outline = body_outline(rect, shape.as_deref());
            if let Some(p) = self.pieces.iter_mut().find(|p| p.id == id) {
                p.rect = rect;
                p.shape = shape;
            }
            if let Some(phys) = self.physics.as_mut() {
                phys.set_piece_shape(id, rect.size.w as f32, rect.size.h as f32, outline);
            }
            if let Some(soft) = self.soft.get_mut(&id) {
                soft.mesh.resize(rect.size.w as f32, rect.size.h as f32);
            }
        }
    }
}

/// Форма куска для физики: вершины относительно центра охвата.
fn body_outline(rect: Rectangle<f64, Logical>, shape: Option<&[shape::Pt]>) -> Option<Vec<Vector>> {
    let (cx, cy) = (
        rect.loc.x + rect.size.w * 0.5,
        rect.loc.y + rect.size.h * 0.5,
    );
    shape.map(|poly| poly.iter().map(|p| v2(p[0] - cx, p[1] - cy)).collect())
}
