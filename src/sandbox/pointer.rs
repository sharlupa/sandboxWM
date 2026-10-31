//! Указатель и обратное проецирование: окно и точка в нём под курсором.

use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use crate::state::AppState;

use super::{Piece, PointerTarget};

impl AppState {
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
        Some(((x, y).into(), piece.contains(x, y)))
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
    pub(super) fn piece_at(&self, sx: f64, sy: f64) -> Option<(u32, Point<f64, Logical>)> {
        let (target, local) = self.physics_pointer_target(sx, sy)?;
        Some((target.piece?, local))
    }
}
