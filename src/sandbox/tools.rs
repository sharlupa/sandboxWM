//! Инструменты: рисование линий, связи между окнами, клавиши инструментов.

use smithay::utils::{Logical, Point};

use crate::physics::{Anchor, LinkKind, Material};
use crate::state::{AppState, LayoutMode};

use super::{Tool, v2};

impl AppState {
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
}
