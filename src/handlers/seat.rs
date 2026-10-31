//! wl_seat (фокус клавиатуры, курсор) и буфер обмена: wl_data_device и
//! primary selection.

use smithay::input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};

use crate::state::AppState;

impl SeatHandler for AppState {
    type KeyboardFocus = crate::focus::KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(
        &mut self,
        seat: &Seat<Self>,
        focused: Option<&crate::focus::KeyboardFocusTarget>,
    ) {
        let focused = focused.and_then(|f| f.surface());
        let focused = focused.as_ref();
        // Буфер обмена привязан к клиенту с клавиатурным фокусом: без этого
        // клиент не получает wl_data_offer и копирование/вставка не работают.
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
        // Клавиатура ушла с игры — захват мыши снимается.
        self.release_constraint_on_focus_change(focused);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.needs_render = true;
    }
}
smithay::delegate_seat!(AppState);

impl SelectionHandler for AppState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for AppState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for AppState {}
impl ServerDndGrabHandler for AppState {}

impl PrimarySelectionHandler for AppState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

smithay::delegate_data_device!(AppState);
smithay::delegate_primary_selection!(AppState);
