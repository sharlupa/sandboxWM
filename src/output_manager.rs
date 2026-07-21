use smithay::output::Output;
use smithay::utils::{Logical, Point};
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
    protocol::wl_output,
};

pub trait OutputManagerHandler {
    fn outputs(&self) -> Vec<(Output, Option<Point<i32, Logical>>)>;
}

#[derive(Debug)]
pub struct OutputManagerState {
    _global: GlobalId,
}

impl OutputManagerState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrOutputManagerV1, ()> + 'static,
    {
        let global = display.create_global::<D, ZwlrOutputManagerV1, ()>(4, ());
        Self { _global: global }
    }
}

// ---------------------------------------------------------
// Manager Implementation
// ---------------------------------------------------------

impl<D> GlobalDispatch<ZwlrOutputManagerV1, (), D> for OutputManagerState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputHeadV1, ()>
        + Dispatch<ZwlrOutputModeV1, ()>
        + OutputManagerHandler
        + AsMut<OutputManagerState>,
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());

        let mut serial = 0;

        for (output, loc) in state.outputs() {
            let head = client
                .create_resource::<ZwlrOutputHeadV1, _, D>(_handle, manager.version(), ())
                .unwrap();
            manager.head(&head);

            head.name(output.name());
            head.description(output.description());

            let props = output.physical_properties();
            head.physical_size(props.size.w, props.size.h);

            // Just output the current mode for now
            if let Some(mode) = output.current_mode() {
                let wlr_mode = client
                    .create_resource::<ZwlrOutputModeV1, _, D>(_handle, head.version(), ())
                    .unwrap();
                head.mode(&wlr_mode);
                wlr_mode.size(mode.size.w, mode.size.h);
                wlr_mode.refresh(mode.refresh);
                wlr_mode.preferred();

                head.current_mode(&wlr_mode);
            }

            head.enabled(1);
            if let Some(pos) = loc {
                head.position(pos.x, pos.y);
            } else {
                head.position(0, 0);
            }
            head.transform(wl_output::Transform::Normal);
            head.scale(output.current_scale().fractional_scale());

            serial += 1;
        }

        manager.done(serial);
    }
}

impl<D> Dispatch<ZwlrOutputManagerV1, (), D> for OutputManagerState
where
    D: Dispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputConfigurationV1, ()>
        + AsMut<OutputManagerState>,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial: _ } => {
                data_init.init(id, ());
            }
            zwlr_output_manager_v1::Request::Stop => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------
// Head & Mode Implementation (Read-only)
// ---------------------------------------------------------

impl<D> Dispatch<ZwlrOutputHeadV1, (), D> for OutputManagerState
where
    D: Dispatch<ZwlrOutputHeadV1, ()> + AsMut<OutputManagerState>,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

impl<D> Dispatch<ZwlrOutputModeV1, (), D> for OutputManagerState
where
    D: Dispatch<ZwlrOutputModeV1, ()> + AsMut<OutputManagerState>,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

// ---------------------------------------------------------
// Configuration Implementation (Dummy)
// ---------------------------------------------------------

impl<D> Dispatch<ZwlrOutputConfigurationV1, (), D> for OutputManagerState
where
    D: Dispatch<ZwlrOutputConfigurationV1, ()>
        + Dispatch<ZwlrOutputConfigurationHeadV1, ()>
        + AsMut<OutputManagerState>,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head: _ } => {
                data_init.init(id, ());
            }
            zwlr_output_configuration_v1::Request::DisableHead { head: _ } => {}
            zwlr_output_configuration_v1::Request::Apply => {
                // Reject changes since we are read-only
                resource.failed();
            }
            zwlr_output_configuration_v1::Request::Test => {
                resource.failed();
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch<ZwlrOutputConfigurationHeadV1, (), D> for OutputManagerState
where
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ()> + AsMut<OutputManagerState>,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        _request: zwlr_output_configuration_head_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

// ---------------------------------------------------------
// Macros
// ---------------------------------------------------------

#[macro_export]
macro_rules! delegate_output_manager {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::output_manager::OutputManagerState);

        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::output_manager::OutputManagerState);

        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1: ()
        ] => $crate::output_manager::OutputManagerState);

        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_mode_v1::ZwlrOutputModeV1: ()
        ] => $crate::output_manager::OutputManagerState);

        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1: ()
        ] => $crate::output_manager::OutputManagerState);

        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1: ()
        ] => $crate::output_manager::OutputManagerState);
    };
}
