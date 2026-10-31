//! Отрисовка: позы кусков на экране и оверлей (линии, связи, пол).

use std::time::Instant;

use rapier2d::math::Vector;

use crate::overlay::{OverlayGeometry, to_screen};
use crate::physics::{Anchor, LinkKind, Material};
use crate::render::{Affine, MeshDraw, PieceDraw};
use crate::state::{AppState, LayoutMode};

use super::{Piece, Tool, ease, shape};

impl AppState {
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
                    let xf = Affine::piece((pose.cx, pose.cy), pose.angle, pose.scale, c);
                    out.push(PieceDraw {
                        window: item.window.clone(),
                        key: item.key,
                        clip: Some(item.clip),
                        mesh: item
                            .shape
                            .as_ref()
                            .map(|s| shape_mesh(&item.window, s, &xf)),
                        shape: item.shape.clone(),
                        xf,
                        popups: item.popups,
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
                        shape: None,
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
                    let mesh = self.mesh_draw(piece, &xf).or_else(|| {
                        let shape = piece.shape.as_ref()?;
                        Some(shape_mesh(window, shape, &xf))
                    });
                    out.push(PieceDraw {
                        window: window.clone(),
                        key: piece.id,
                        clip: Some(piece.rect),
                        shape: piece.shape.clone(),
                        xf,
                        popups: Some(piece.id) == main,
                        mesh,
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
}

/// Сетка косо отрезанного куска: веер треугольников по его форме
/// (координаты окна), текстура — снимок окна.
fn shape_mesh(window: &smithay::desktop::Window, shape: &[shape::Pt], xf: &Affine) -> MeshDraw {
    let size = window.geometry().size;
    let (gw, gh) = (size.w.max(1) as f64, size.h.max(1) as f64);
    let mut fingerprint: u64 = 0xcbf2_9ce4_8422_2325;
    let positions = shape
        .iter()
        .map(|p| {
            let (x, y) = xf.apply(p[0], p[1]);
            for v in [x as f32, y as f32] {
                fingerprint ^= v.to_bits() as u64;
                fingerprint = fingerprint.wrapping_mul(0x0100_0000_01b3);
            }
            [x as f32, y as f32]
        })
        .collect();
    MeshDraw {
        positions,
        geometry_uv: shape
            .iter()
            .map(|p| [(p[0] / gw) as f32, (p[1] / gh) as f32])
            .collect(),
        indices: shape::fan_indices(shape.len()),
        fingerprint,
    }
}
