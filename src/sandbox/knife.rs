//! Нож (концепт §7): разрезание окна и склейка кусков.

use rapier2d::math::Vector;
use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};

use crate::physics::{Anchor, LinkKind};
use crate::state::{AppState, LayoutMode};

use super::{LinkEnd, Piece, shape, v2};

impl AppState {
    /// Режет верхнее окно, через которое прошёл штрих (экранные координаты):
    /// каждый задетый кусок — по прямой штриха, под любым углом. Почти
    /// горизонтальный или вертикальный штрих режет ровно по оси окна. Части —
    /// кадры одного буфера (косые рисуются сеткой по снимку окна),
    /// приложение продолжает работать целиком.
    pub(super) fn knife_cut(&mut self, s0: (f64, f64), s1: (f64, f64)) {
        if (s1.0 - s0.0).hypot(s1.1 - s0.1) < 8.0 {
            return;
        }
        let min_piece = self.config.slicing.min_piece_size.max(0.0) as f64;
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
                let (a, b) = ([a.0, a.1], [b.0, b.1]);
                let outline = piece.outline();
                // Штрих должен задеть кусок; режет вся его прямая.
                let Some((t0, t1)) = shape::clip_segment(&outline, a, b) else {
                    continue;
                };
                if t1 - t0 < 1e-6 {
                    continue;
                }
                let (la, lb) = cut_line(a, b, (t0 + t1) * 0.5);
                let Some((left, right)) = shape::split(&outline, la, lb) else {
                    continue;
                };
                if shape::thickness(&left, la, lb) < min_piece
                    || shape::thickness(&right, la, lb) < min_piece
                {
                    continue;
                }
                // Нормаль к разрезу в сторону левой части.
                let (dx, dy) = (lb[0] - la[0], lb[1] - la[1]);
                let len = dx.hypot(dy);
                let normal = [-dy / len, dx / len];
                self.split_piece(&piece, left, right, normal, gap);
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

    /// Кусок → две части по разрезу. Части чуть расходятся по `normal`
    /// (к левой части) и наследуют позу, скорость и вес куска.
    fn split_piece(
        &mut self,
        piece: &Piece,
        left: Vec<shape::Pt>,
        right: Vec<shape::Pt>,
        normal: shape::Pt,
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
        let window = piece.window.clone();
        let links = self.take_links(&[piece.id]);
        // Разрезанная сетка теряет смысл — части снова жёсткие.
        self.drop_soft(piece.id);
        if let Some(phys) = self.physics.as_mut() {
            phys.remove_piece(piece.id);
        }
        self.pieces.retain(|p| p.id != piece.id);
        for (poly, sign) in [(left, 1.0), (right, -1.0)] {
            let rect = shape::bounds(&poly);
            let rc = (
                rect.loc.x + rect.size.w * 0.5,
                rect.loc.y + rect.size.h * 0.5,
            );
            let d = (
                rc.0 - pc_x + normal[0] * sign * gap * 0.5,
                rc.1 - pc_y + normal[1] * sign * gap * 0.5,
            );
            let center = c + crate::physics::rotate(v2(d.0, d.1), angle);
            let shape = (!shape::is_axis_rect(&poly)).then_some(poly);
            let id = self.spawn_piece(&window, rect, shape, center, angle, weight);
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
                        let d = |p: &Piece| shape::distance_sq(&p.outline(), [x, y]);
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

    /// Super+C: собрать все окна, разбросанные по холсту, в кадр.
    ///
    /// Окна выстраиваются сеткой на полу (ряды снизу вверх) в текущем месте
    /// камеры — ровно, без скорости; разрезанные — вместе со своими
    /// кусками. Все связи окон (сварка, пружины, верёвки, клей, подвесы)
    /// снимаются — иначе сваренные/подвешенные окна тянули бы друг друга
    /// из сетки. Если сетка не помещается в кадр, камера отдаляется.
    pub fn gather_windows(&mut self) {
        if self.layout_mode != LayoutMode::Physics || self.transition.is_some() {
            return;
        }
        self.exit_fullscreen();
        let Some(floor_y) = self.physics.as_ref().map(|p| p.floor_y() as f64) else {
            return;
        };
        // Окна в порядке стека; размер — охват всех кусков окна.
        let windows: Vec<(Window, Rectangle<f64, Logical>)> = self
            .space
            .elements()
            .filter(|w| !Self::is_override_redirect(w))
            .filter_map(|w| {
                let bounds = self
                    .pieces_of(w)
                    .map(|p| p.rect)
                    .reduce(|a, b| a.merge(b))?;
                Some((w.clone(), bounds))
            })
            .collect();
        if windows.is_empty() {
            return;
        }
        self.cancel_interactions();
        let ids: Vec<u32> = self.pieces.iter().map(|p| p.id).collect();
        if let Some(phys) = self.physics.as_mut() {
            for id in ids {
                phys.unlink_piece(id);
            }
        }

        const GAP: f64 = 40.0;
        let n = windows.len();
        let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
        let rows: Vec<&[(Window, Rectangle<f64, Logical>)]> = windows.chunks(cols).collect();
        let row_width = |row: &[(Window, Rectangle<f64, Logical>)]| -> f64 {
            row.iter().map(|(_, r)| r.size.w).sum::<f64>() + GAP * (row.len() as f64 - 1.0)
        };
        let row_height = |row: &[(Window, Rectangle<f64, Logical>)]| -> f64 {
            row.iter().map(|(_, r)| r.size.h).fold(0.0, f64::max)
        };
        let total_w = rows.iter().map(|r| row_width(r)).fold(0.0, f64::max);
        let total_h: f64 =
            rows.iter().map(|r| row_height(r)).sum::<f64>() + GAP * (rows.len() as f64 - 1.0);

        // Камера: сетка целиком в кадре, пол у нижнего края.
        let screen = self.output_size();
        let (sw, sh) = (screen.w as f64, screen.h as f64);
        let fit = ((sw * 0.9) / total_w).min((sh * 0.85) / (total_h + GAP));
        let zoom = self
            .camera_zoom
            .min(fit)
            .clamp(self.config.physics.zoom_min, self.config.physics.zoom_max);
        let center_x = self.camera_offset.0 + sw / (2.0 * self.camera_zoom);
        let cam = (center_x - sw / (2.0 * zoom), floor_y + GAP - sh / zoom);
        self.camera_zoom = zoom;
        self.camera_offset = cam;
        self.target_camera_offset = cam;

        // Раскладка: ряды снизу вверх, каждый ряд по центру.
        let mut bottom = floor_y - 1.0;
        for row in rows {
            let mut x = center_x - row_width(row) * 0.5;
            for (window, bounds) in row {
                let pieces: Vec<(u32, (f64, f64))> =
                    self.pieces_of(window).map(|p| (p.id, p.center())).collect();
                if let Some(phys) = self.physics.as_mut() {
                    for (id, (pcx, pcy)) in pieces {
                        // Куски сохраняют взаимное расположение внутри окна.
                        let cx = x + (pcx - bounds.loc.x);
                        let cy = bottom - bounds.size.h + (pcy - bounds.loc.y);
                        phys.set_piece_transform(id, v2(cx, cy), 0.0);
                        phys.set_piece_velocity(id, Vector::ZERO, 0.0);
                    }
                }
                x += bounds.size.w + GAP;
            }
            bottom -= row_height(row) + GAP;
        }
        self.last_physics_tick = None;
        self.sync_space_positions();
        self.needs_render = true;
        println!("[physics] окна собраны: {n}");
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
        let id = self.spawn_piece(&window, full, None, center, angle, weight);
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
}

/// Прямая разреза через точку штриха с параметром `t` (внутри куска).
/// Штрих в пределах [`shape::AXIS_SNAP`] от оси окна выравнивается по ней, а
/// разрез встаёт на целый пиксель — края кусков остаются ровными.
fn cut_line(a: shape::Pt, b: shape::Pt, t: f64) -> (shape::Pt, shape::Pt) {
    let m = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let angle = (b[1] - a[1]).atan2(b[0] - a[0]);
    let quarter = std::f64::consts::FRAC_PI_2;
    let axis = (angle / quarter).round() * quarter;
    if (angle - axis).abs() > shape::AXIS_SNAP {
        return (a, b);
    }
    let (dx, dy) = (axis.cos().round(), axis.sin().round());
    if dx != 0.0 {
        let y = m[1].round();
        ([m[0], y], [m[0] + dx, y])
    } else {
        let x = m[0].round();
        ([x, m[1]], [x, m[1] + dy])
    }
}
