//! Физическая «песочница» поверх [`AppState`]: куски окон, инструменты,
//! режимы гравитации, плавные переходы тайлинг ↔ физика, обратное
//! проецирование курсора, оверлей и сессия.
//!
//! Здесь — общие типы; методы `AppState` разложены по темам: `camera`,
//! `pieces` (тела окон), `modes` (тайлинг ↔ физика, гравитация), `step`
//! (шаг симуляции), `draw`, `pointer` (что под курсором), `drag`, `tools`,
//! `knife`, `session`, `soft` (мягкие тела).
//!
//! [`AppState`]: crate::state::AppState
//!
//! Координаты:
//! - **экран** — физические пиксели выхода (scale 1);
//! - **мир** — бесконечный холст rapier: `экран = (мир − camera_offset) · zoom`;
//! - **координаты окна** — относительно верхнего левого угла геометрии окна
//!   (без CSD-тени), как у плоского буфера клиента. Кусок окна — прямоугольник
//!   `rect` в этих координатах, его центр совпадает с центром тела в мире.

use std::time::Instant;

use rapier2d::math::Vector;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus;

use crate::physics::{Anchor, LinkKind, Material};

mod camera;
mod drag;
mod draw;
mod knife;
mod modes;
mod pieces;
mod pointer;
mod session;
pub mod shape;
mod soft;
mod step;
mod tools;

/// Кусок окна в физическом режиме (неразрезанное окно — один кусок).
#[derive(Debug, Clone)]
pub struct Piece {
    pub id: u32,
    pub window: Window,
    /// Охват куска в координатах окна; его центр — центр тела в мире.
    pub rect: Rectangle<f64, Logical>,
    /// Форма после косого разреза: выпуклый многоугольник в координатах
    /// окна. `None` — кусок занимает весь `rect`.
    pub shape: Option<Vec<shape::Pt>>,
}

impl Piece {
    fn center(&self) -> (f64, f64) {
        (
            self.rect.loc.x + self.rect.size.w * 0.5,
            self.rect.loc.y + self.rect.size.h * 0.5,
        )
    }

    /// Контур куска в координатах окна.
    fn outline(&self) -> Vec<shape::Pt> {
        self.shape
            .clone()
            .unwrap_or_else(|| shape::rect_outline(self.rect))
    }

    fn area(&self) -> f64 {
        match &self.shape {
            Some(poly) => shape::area(poly),
            None => self.rect.size.w * self.rect.size.h,
        }
    }

    /// Точка (координаты окна) внутри куска.
    fn contains(&self, x: f64, y: f64) -> bool {
        match &self.shape {
            Some(poly) => shape::contains(poly, [x, y]),
            None => {
                let r = self.rect;
                x >= r.loc.x && y >= r.loc.y && x < r.loc.x + r.size.w && y < r.loc.y + r.size.h
            }
        }
    }
}

/// Мягкое тело куска (концепт §5): сетка + параметры + прошлая скорость
/// тела для инерционных псевдосил (удар об пол сплющивает окно).
pub struct SoftPiece {
    pub mesh: crate::softbody::SoftMesh,
    pub params: crate::softbody::SoftParams,
    /// Мягкое или желе.
    pub kind: crate::config::BodyKind,
    prev_vel: Option<Vector>,
    /// Постоянная нагрузка (вес на полу) сетку не сминает.
    load: crate::softbody::LoadFilter,
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
    shape: Option<Vec<shape::Pt>>,
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
    fn wrap_angles() {
        assert!((wrap_angle(3.0 * std::f64::consts::PI) - std::f64::consts::PI).abs() < 1e-9);
        assert!((wrap_angle(-0.5) + 0.5).abs() < 1e-12);
    }
}
