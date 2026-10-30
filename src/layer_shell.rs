//! wlr-layer-shell: панели (waybar), фоны (swaybg), лаунчеры (wofi,
//! fuzzel, rofi-wayland), уведомления (mako) и прочие поверхности слоёв.
//!
//! Слои рисуются в экранных координатах поверх/под окнами в обоих режимах:
//! `overlay` и `top` — над окнами (`top` прячется под полноэкранным окном),
//! `bottom` и `background` — под ними. Эксклюзивные зоны панелей вычитаются
//! из области тайлинга, а пол физики встаёт над нижней панелью.

use smithay::backend::renderer::element::{AsRenderElements, surface::WaylandSurfaceRenderElement};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::utils::{Logical, Point, Rectangle, Scale};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};

use crate::render::CustomRenderElements;
use crate::state::AppState;

impl WlrLayerShellHandler for AppState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            // Выхода ещё нет — поверхности негде жить.
            surface.send_close();
            return;
        };
        let layer = LayerSurface::new(surface, namespace);
        if let Err(e) = layer_map_for_output(&output).map_layer(&layer) {
            eprintln!(
                "[layer-shell] не удалось разместить {:?}: {e}",
                layer.namespace()
            );
        }
        // Первый configure уйдёт на первом коммите (см. `layer_commit`).
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in outputs {
            let mut map = layer_map_for_output(&output);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                map.unmap_layer(&layer);
            }
        }
        // Лаунчер закрылся — фокус возвращается окну.
        let had_focus = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|f| f.surface())
            .is_some_and(|f| &f == surface.wl_surface());
        if had_focus {
            self.focus_top_window();
        }
        self.layers_changed();
    }
}
smithay::delegate_layer_shell!(AppState);

impl AppState {
    /// Область экрана без эксклюзивных зон панелей.
    pub fn usable_area(&self) -> Rectangle<i32, Logical> {
        match self.space.outputs().next() {
            Some(output) => {
                let zone = layer_map_for_output(output).non_exclusive_zone();
                if zone.size.w > 0 && zone.size.h > 0 {
                    zone
                } else {
                    self.output_rect()
                }
            }
            None => self.output_rect(),
        }
    }

    /// Перерасчёт слоёв (сменился размер выхода или зоны панелей).
    pub fn arrange_layers(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in outputs {
            layer_map_for_output(&output).arrange();
        }
    }

    /// Эксклюзивные зоны могли измениться: тайлинг подстраивается.
    fn layers_changed(&mut self) {
        self.layout_dirty = true;
        self.needs_render = true;
    }

    /// Коммит поверхности слоя: первый configure, раскладка, фокус
    /// клавиатуры. `true` — поверхность принадлежит слою.
    pub fn layer_commit(&mut self, surface: &WlSurface) -> bool {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                layer_map_for_output(o)
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned()
        else {
            return false;
        };
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|d| d.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });
        let (zone_before, zone_after, layer) = {
            let mut map = layer_map_for_output(&output);
            let before = map.non_exclusive_zone();
            map.arrange();
            let after = map.non_exclusive_zone();
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .cloned();
            (before, after, layer)
        };
        let Some(layer) = layer else {
            return true;
        };
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }
        if zone_before != zone_after {
            self.layers_changed();
        }
        // Лаунчеры (wofi, fuzzel) просят эксклюзивную клавиатуру.
        let state = layer.cached_state();
        let mapped = layer.bbox().size.w > 0;
        if mapped
            && state.keyboard_interactivity == KeyboardInteractivity::Exclusive
            && matches!(state.layer, Layer::Top | Layer::Overlay)
        {
            self.focus_layer(&layer);
        }
        self.needs_render = true;
        true
    }

    /// Клавиатурный фокус — поверхности слоя.
    pub fn focus_layer(&mut self, layer: &LayerSurface) {
        let surface = layer.wl_surface().clone();
        let Some(kbd) = self.seat.get_keyboard() else {
            return;
        };
        if kbd.current_focus().and_then(|f| f.surface()).as_ref() == Some(&surface) {
            return;
        }
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        kbd.set_focus(self, Some(surface.into()), serial);
        self.needs_render = true;
    }

    /// Фокус — верхнему обычному окну активного стола (или никому).
    pub fn focus_top_window(&mut self) {
        let next = self.fullscreen_window().or_else(|| {
            self.space
                .elements()
                .rev()
                .find(|w| !w.x11_surface().is_some_and(|x| x.is_override_redirect()))
                .cloned()
        });
        match next {
            Some(w) => self.focus_window(&w),
            None => {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(kbd) = self.seat.get_keyboard() {
                    kbd.set_focus(self, None, serial);
                }
            }
        }
    }

    /// Слой с эксклюзивной клавиатурой держит фокус (лаунчер открыт).
    pub fn exclusive_layer_focus(&self) -> bool {
        let Some(focus) = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|f| f.surface())
        else {
            return false;
        };
        self.space.outputs().any(|o| {
            layer_map_for_output(o)
                .layer_for_surface(&focus, WindowSurfaceType::TOPLEVEL)
                .is_some_and(|l| {
                    l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                })
        })
    }

    /// Поверхность слоя под экранной точкой: (поверхность, её начало на
    /// экране) и сам слой.
    pub fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
        layers: &[Layer],
    ) -> Option<((WlSurface, Point<f64, Logical>), LayerSurface)> {
        let output = self.space.outputs().next()?;
        let map = layer_map_for_output(output);
        for layer in layers {
            let Some(l) = map.layer_under(*layer, pos) else {
                continue;
            };
            let Some(geo) = map.layer_geometry(l) else {
                continue;
            };
            if let Some((surface, loc)) =
                l.surface_under(pos - geo.loc.to_f64(), WindowSurfaceType::ALL)
            {
                return Some(((surface, (loc + geo.loc).to_f64()), l.clone()));
            }
        }
        None
    }

    /// Слои, которые сейчас перехватывают указатель над окнами.
    pub fn upper_layers(&self) -> &'static [Layer] {
        if self.fullscreen_window().is_some() {
            &[Layer::Overlay]
        } else {
            &[Layer::Overlay, Layer::Top]
        }
    }

    /// Отправляет кадровые колбэки поверхностям слоёв и чистит мёртвые.
    pub fn send_layer_frames<T: Into<std::time::Duration> + Copy>(&self, output: &Output, now: T) {
        let mut map = layer_map_for_output(output);
        map.cleanup();
        for layer in map.layers() {
            layer.send_frame(output, now, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }
}

/// Render-элементы слоёв `layers` (по порядку: первый — верхний),
/// front-to-back, в экранных координатах.
pub fn layer_elements(
    renderer: &mut GlesRenderer,
    output: &Output,
    layers: &[Layer],
) -> Vec<CustomRenderElements> {
    let map = layer_map_for_output(output);
    let mut out = Vec::new();
    for layer in layers {
        // Позже размещённые — выше.
        for surface in map.layers_on(*layer).rev() {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                surface.render_elements(renderer, geo.loc.to_physical(1), Scale::from(1.0), 1.0);
            out.extend(elements.into_iter().map(CustomRenderElements::Surface));
        }
    }
    out
}
