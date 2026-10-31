//! Нож (концепт §7): разрезание окна и склейка кусков.

use rapier2d::math::Vector;
use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};

use crate::physics::{Anchor, LinkKind};
use crate::state::{AppState, LayoutMode};

use super::{LinkEnd, Piece, clip_segment, v2};

impl AppState {
    /// Режет верхнее окно, через которое прошёл отрезок (экранные координаты).
    /// Разрез выровнен по осям окна: части — прямоугольные кадры одного
    /// буфера, как у `wp_viewporter`, приложение продолжает работать целиком.
    pub(super) fn knife_cut(&mut self, s0: (f64, f64), s1: (f64, f64)) {
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
}
