//! wl_compositor (коммиты поверхностей) и буферы: wl_shm, linux-dmabuf,
//! wp_viewporter.

use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shm::{ShmHandler, ShmState};

use crate::state::{AppState, ClientState, LayoutMode};

impl CompositorHandler for AppState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Обычные Wayland-клиенты несут ClientState, а клиент XWayland —
        // XWaylandClientData (его создаёт smithay при XWayland::spawn).
        // Раньше unwrap() здесь ронял композитор (SIGABRT в global_bind),
        // как только Xwayland подключался к композитору.
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Поверхность курсора: wl_surface.offset сдвигает hotspot.
        if let CursorImageStatus::Surface(cursor) = &self.cursor_status {
            if cursor == surface {
                crate::cursor::apply_hotspot_delta(surface);
                self.needs_render = true;
            }
        }
        self.popup_manager.commit(surface);
        self.popup_manager.cleanup();
        // Обновляем буферы рендера
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Панель/фон/лаунчер (layer-shell).
        if self.layer_commit(surface) {
            return;
        }

        self.activate_pending_constraint(surface);

        // Найдём окно, которому принадлежит эта поверхность, и обновим его bbox.
        // ВАЖНО: recalculate_layout() здесь НЕ вызываем — это создаёт петлю
        // commit → configure → commit → ... (~30–40% CPU на простое рабочем столе,
        // т.к. мигание курсора терминала триггерит commit). Тайловые позиции
        // выставляются в new_toplevel / remove_window_common и отложенной
        // раскладкой (layout_dirty).
        let mut found = None;
        for window in self.space.elements() {
            if window.wl_surface().as_ref().map(|s| s.as_ref()) == Some(surface) {
                found = Some(window.clone());
                break;
            }
        }
        if found.is_none() {
            // Окно скрытого рабочего стола: на экране его нет, но ответ на
            // configure засчитываем, иначе после возврата ему не уйдёт новый.
            if let Some(window) =
                self.find_hidden_window(|w| w.wl_surface().as_deref() == Some(surface))
            {
                window.on_commit();
                self.pending_configures.remove(&window);
                return;
            }
        }
        if let Some(window) = found {
            let bbox = window.bbox();
            window.on_commit();
            // Окно впервые отрисовалось или сменило размер — под курсором
            // теперь, может быть, оно. Полноэкранное окно (игра) получает
            // указатель, как только у него появилась поверхность: раньше, чем
            // захватит мышь и вернёт курсор в центр. Иначе `enter` приходил
            // позже, и XWayland переносил X-курсор игры к курсору композитора.
            let pointer_elsewhere = self.is_fullscreen(&window)
                && self.seat.get_pointer().is_some_and(|p| {
                    p.current_focus()
                        .is_none_or(|s| &crate::input::constraints::root_surface(&s) != surface)
                });
            if window.bbox() != bbox || pointer_elsewhere {
                self.queue_pointer_refresh();
            }
            // Client has committed a buffer, meaning it has processed the last
            // configure event. We can send new configure events to it now.
            self.pending_configures.remove(&window);
            if self.x11_stacking_dirty && window.x11_surface().is_some() {
                self.x11_stacking_dirty = false;
                self.sync_x11_stacking();
            }
            // Размер, отложенный backpressure-ом (например, сосед закрылся,
            // пока клиент ещё не ответил на прошлый configure), уходит сейчас:
            // иначе окно так и оставалось старого размера.
            if let Some(toplevel) = window.toplevel() {
                if toplevel.is_initial_configure_sent() && toplevel.has_pending_changes() {
                    toplevel.send_configure();
                    self.pending_configures.insert(window.clone());
                }
            }
            // Окно перерисовалось в размер тайла — ставим его ровно в тайл.
            self.reposition_tiled_window(&window);

            let is_or = window
                .x11_surface()
                .map_or(false, |x| x.is_override_redirect());
            let size = window.geometry().size;
            if !is_or && size.w > 1 && size.h > 1 {
                if self.pieces_of(&window).next().is_none() {
                    // Первый настоящий буфер: в физике окно получает тело
                    // (или сохранённое место из сессии); в тайлинге —
                    // только сохранённое место на будущее.
                    if self.layout_mode == LayoutMode::Physics && self.transition.is_none() {
                        self.physics_spawn_window(&window);
                    } else if self.restorer.is_some() {
                        self.restore_window_for_later(&window);
                    }
                } else {
                    // Клиент сменил размер — коллайдер следует за окном.
                    self.physics_window_resized(&window);
                }
            }

            // During active resize: do NOT call apply_window_geometry or
            // recalculate_layout here (commit → configure → commit loop).
        }

        // Новый буфер — экран нужно перерисовать.
        self.needs_render = true;
    }
}

smithay::delegate_compositor!(AppState);

impl BufferHandler for AppState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl smithay::wayland::dmabuf::DmabufHandler for AppState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        // We accept all dmabufs. The actual GPU import is done lazily by the GLES
        // renderer's EGL buffer reader (set up via bind_wl_display) when the
        // client attaches this buffer to a surface and commits it — so there is
        // no renderer to test against here.
        let _ = notifier.successful::<AppState>();
    }
}

impl ShmHandler for AppState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
smithay::delegate_shm!(AppState);
smithay::delegate_dmabuf!(AppState);
smithay::delegate_viewporter!(AppState);
