//! Синхронизация буферов клиентов с GPU.
//!
//! Коммит с новым dmabuf применяется, только когда клиент дорисовал кадр:
//! по точке acquire явной синхронизации (`linux-drm-syncobj-v1`) или по
//! неявному фенсу самого dmabuf. Иначе композитор брал недорисованный
//! буфер, и его кадр (или флип на экран) ждал GPU клиента.
//!
//! Явная синхронизация нужна XWayland на NVIDIA: у драйвера нет неявной, и
//! без `wp_linux_drm_syncobj_manager_v1` XWayland не отдаёт кадры игр
//! напрямую (Present flip, `xwl_present_check_flip`), а копирует каждый —
//! отсюда меньше FPS и неровные кадры в X11-играх. Сигнал release Smithay
//! подаёт сам, когда буфер больше не нужен рендереру и экрану.

use std::io;

use smithay::backend::drm::DrmDeviceFd;
use smithay::reexports::calloop::{EventSource, Interest};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::wayland::compositor::{
    BufferAssignment, CompositorHandler, SurfaceAttributes, add_blocker, add_pre_commit_hook,
    with_states,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::drm_syncobj::{
    DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd,
};

use crate::state::AppState;

impl DrmSyncobjHandler for AppState {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}
smithay::delegate_drm_syncobj!(AppState);

impl AppState {
    /// Включить явную синхронизацию: точки синхронизации клиентов
    /// импортируются и ждутся на устройстве `device`. Глобал должен появиться
    /// до того, как XWayland прочитает реестр, — бэкенды зовут это до первой
    /// обработки клиентов.
    pub fn enable_explicit_sync(&mut self, device: DrmDeviceFd) {
        if self.syncobj_state.is_some() {
            return;
        }
        if !supports_syncobj_eventfd(&device) {
            println!("[sync] драйвер не умеет syncobj eventfd — без явной синхронизации");
            return;
        }
        self.syncobj_state = Some(DrmSyncobjState::new::<Self>(&self.display_handle, device));
        println!("=====> Явная синхронизация (linux-drm-syncobj-v1) включена");
    }

    /// Применить коммит, когда сработает `source` (буфер готов).
    fn unblock_commit_on<S>(&self, source: S, client: Client) -> bool
    where
        S: EventSource<Event = (), Ret = io::Result<()>> + 'static,
    {
        self.loop_handle
            .insert_source(source, move |_, _, state| {
                let dh = state.display_handle.clone();
                state
                    .client_compositor_state(&client)
                    .blocker_cleared(state, &dh);
                Ok(())
            })
            .is_ok()
    }
}

/// Хук новой поверхности: коммит с новым dmabuf ждёт готовности буфера.
pub(super) fn wait_for_buffers(surface: &WlSurface) {
    add_pre_commit_hook::<AppState, _>(surface, |state, _dh, surface| {
        let mut acquire_point = None;
        let dmabuf = with_states(surface, |states| {
            acquire_point.clone_from(
                &states
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point,
            );
            match states
                .cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
                .as_ref()
            {
                Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).cloned().ok(),
                _ => None,
            }
        });
        let (Some(dmabuf), Some(client)) = (dmabuf, surface.client()) else {
            return;
        };
        // Явная синхронизация: ждём точку acquire.
        if let Some(Ok((blocker, source))) = acquire_point.map(|p| p.generate_blocker()) {
            if state.unblock_commit_on(source, client.clone()) {
                add_blocker(surface, blocker);
                return;
            }
        }
        // Неявная: ждём фенс рендера в самом dmabuf (если он ещё не готов).
        if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
            if state.unblock_commit_on(source, client) {
                add_blocker(surface, blocker);
            }
        }
    });
}
