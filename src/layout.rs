//! Тайлинг: BSP-дерево ([`crate::tiling`]) → положения и размеры окон в Space.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use crate::state::{AppState, LayoutMode};

/// Отступы тайлинга: между окнами и от краёв экрана.
pub const GAPS_IN: i32 = 4;
pub const GAPS_OUT: i32 = 8;

impl AppState {
    /// Область тайлинга: экран без панелей (layer-shell) и внешних отступов.
    pub fn tiling_area(&self) -> Rectangle<i32, Logical> {
        let usable = self.usable_area();
        Rectangle::new(
            (usable.loc.x + GAPS_OUT, usable.loc.y + GAPS_OUT).into(),
            (
                (usable.size.w - GAPS_OUT * 2).max(1),
                (usable.size.h - GAPS_OUT * 2).max(1),
            )
                .into(),
        )
    }

    /// Тайловый прямоугольник окна (как его расставит `recalculate_layout`).
    pub fn tile_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let cell = self
            .tile_tree
            .as_ref()?
            .area_for(window, self.tiling_area())?;
        Some(crate::tiling::shrink(cell, GAPS_IN))
    }

    /// BSP tiling layout calculation.
    pub fn recalculate_layout(&mut self) {
        if self.layout_mode != LayoutMode::Tiling {
            return;
        }
        let Some(tree) = &self.tile_tree else {
            return;
        };
        let mut rects = Vec::new();
        tree.collect_rects(self.tiling_area(), GAPS_IN, &mut rects);
        // map_element поднимает окно наверх стека — сохраняем порядок, чтобы
        // перерасчёт не прятал X11-меню под окна и не терял поднятое окно.
        let order: Vec<Window> = self.space.elements().cloned().collect();
        let mut moved = false;
        for (win, rect) in rects {
            // Полноэкранное окно живёт вне тайлов.
            if self.is_fullscreen(&win) {
                continue;
            }
            moved |= self.apply_window_geometry(&win, rect);
        }
        if moved {
            for w in &order {
                self.space.raise_element(w, false);
            }
        }
        self.place_fullscreen_window();
    }

    /// Где поставить окно в тайле `rect`. Пока клиент не перерисовался в
    /// новый размер, окно прижато к неподвижному краю тайла (ресайз за
    /// левый/верхний край без рывков); как только размер совпал — ровно в
    /// тайл. Раньше прижатое положение так и оставалось после коммита: окно,
    /// занявшее место закрытого соседа слева, уезжало за правый край экрана.
    fn tile_location(&self, window: &Window, rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
        let current = window.geometry().size;
        let mut loc = rect.loc;
        if current == rect.size {
            return loc;
        }
        if let Some(old_geo) = self.space.element_geometry(window) {
            let old_right = old_geo.loc.x + old_geo.size.w;
            let old_bottom = old_geo.loc.y + old_geo.size.h;
            let target_right = rect.loc.x + rect.size.w;
            let target_bottom = rect.loc.y + rect.size.h;
            if (target_right - old_right).abs() < 5 && rect.loc.x != old_geo.loc.x {
                loc.x = target_right - current.w;
            }
            if (target_bottom - old_bottom).abs() < 5 && rect.loc.y != old_geo.loc.y {
                loc.y = target_bottom - current.h;
            }
        }
        loc
    }

    /// Коммит окна в тайлинге: после смены размера поставить его ровно в
    /// свой тайл (см. [`Self::tile_location`]).
    pub(crate) fn reposition_tiled_window(&mut self, window: &Window) {
        if self.layout_mode != LayoutMode::Tiling
            || self.is_fullscreen(window)
            || window
                .x11_surface()
                .is_some_and(|x| x.is_override_redirect())
        {
            return;
        }
        let Some(rect) = self.tile_rect(window) else {
            return;
        };
        let loc = self.tile_location(window, rect);
        if self.space.element_location(window) == Some(loc) {
            return;
        }
        let order: Vec<Window> = self.space.elements().cloned().collect();
        self.space.map_element(window.clone(), loc, false);
        for w in &order {
            self.space.raise_element(w, false);
        }
        if let Some(x11) = window.x11_surface() {
            let target = Rectangle::new(loc, rect.size);
            if x11.geometry() != target {
                let _ = x11.configure(target);
            }
        }
        self.needs_render = true;
    }

    /// Ставит окно в прямоугольник тайла. Возвращает true, если окно
    /// перемещалось в Space (порядок стека нужно восстановить).
    fn apply_window_geometry(&mut self, window: &Window, rect: Rectangle<i32, Logical>) -> bool {
        if !self.space.elements().any(|w| w == window) {
            // Окно уже закрыто/размаплено — не возвращаем его в Space.
            return false;
        }
        let loc = self.tile_location(window, rect);

        let moved = self.space.element_location(window) != Some(loc);
        if moved {
            self.space.map_element(window.clone(), loc, false);
        }

        // Ask the client to resize its surface — but only if the size actually
        // changed. Sending a configure with the same size on every relayout used
        // to trigger an O(N²) "configure storm".
        if let Some(toplevel) = window.toplevel() {
            let mut changed = false;
            toplevel.with_pending_state(|state| {
                if state.size != Some(rect.size) {
                    state.size = Some(rect.size);
                    changed = true;
                }
            });
            // Backpressure: only send a new configure if the client has
            // already processed and committed the previous one.
            if changed && !self.pending_configures.contains(window) {
                toplevel.send_configure();
                self.pending_configures.insert(window.clone());
            }
        }

        // X11-окна (XWayland): размер/позиция задаются напрямую через configure.
        if let Some(x11) = window.x11_surface() {
            let target = Rectangle::new(loc, rect.size);
            if x11.geometry() != target {
                let _ = x11.configure(target);
            }
        }
        moved
    }
}
