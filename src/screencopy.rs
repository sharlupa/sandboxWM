use std::sync::{Arc, Mutex};
use smithay::output::Output;
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    backend::GlobalId,
    protocol::{wl_buffer, wl_output},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

// We store pending captures.
// A capture can be either waiting for the client to provide a buffer (`copy`),
// or ready to be fulfilled by the compositor's render loop.

pub struct ScreencopyState {
    _global: GlobalId,
    // Frames that the client created but hasn't called `copy` on yet,
    // OR has called `copy` on and are waiting for the next render.
    pub pending_frames: Arc<Mutex<Vec<PendingFrame>>>,
}

pub struct PendingFrame {
    pub frame: ZwlrScreencopyFrameV1,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub output: Option<wl_output::WlOutput>,
    // Optional region to copy. If None, copy whole output.
    pub region: Option<(i32, i32, i32, i32)>,
    pub overlay_cursor: bool,
}

impl ScreencopyState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + 'static,
    {
        let global = display.create_global::<D, ZwlrScreencopyManagerV1, ()>(3, ());
        Self {
            _global: global,
            pending_frames: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

// ---------------------------------------------------------
// Manager Implementation
// ---------------------------------------------------------

impl<D> GlobalDispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + Dispatch<ZwlrScreencopyManagerV1, ()> + AsMut<ScreencopyState>,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyManagerV1, ()> + Dispatch<ZwlrScreencopyFrameV1, ()> + AsMut<ScreencopyState>,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let screencopy_state = state.as_mut();
        
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, overlay_cursor, output } => {
                let frame = data_init.init(frame, ());
                
                // For a simple implementation, we just assume a fixed size or fetch it from Output.
                // We should get the actual output size. We can retrieve the Output data from wl_output.
                let output_data = Output::from_resource(&output).unwrap();
                let (width, height) = output_data.current_mode().map(|m| (m.size.w, m.size.h)).unwrap_or((1920, 1080));
                
                // Send buffer information to client (ARGB8888 as standard format).
                // wl_shm::Format::Argb8888 is 0.
                frame.buffer(wayland_server::protocol::wl_shm::Format::Argb8888, width as u32, height as u32, (width * 4) as u32);
                
                screencopy_state.pending_frames.lock().unwrap().push(PendingFrame {
                    frame,
                    buffer: None,
                    output: Some(output),
                    region: None,
                    overlay_cursor: overlay_cursor != 0,
                });
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let frame = data_init.init(frame, ());
                
                // wl_shm::Format::Argb8888 is 0.
                frame.buffer(wayland_server::protocol::wl_shm::Format::Argb8888, width as u32, height as u32, (width * 4) as u32);
                
                screencopy_state.pending_frames.lock().unwrap().push(PendingFrame {
                    frame,
                    buffer: None,
                    output: Some(output),
                    region: Some((x, y, width, height)),
                    overlay_cursor: overlay_cursor != 0,
                });
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {
                // Client destroyed the manager object.
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------
// Frame Implementation
// ---------------------------------------------------------

impl<D> Dispatch<ZwlrScreencopyFrameV1, (), D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyFrameV1, ()> + AsMut<ScreencopyState>,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let screencopy_state = state.as_mut();
        
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                // Client provided a buffer. We update the pending frame.
                let mut frames = screencopy_state.pending_frames.lock().unwrap();
                if let Some(f) = frames.iter_mut().find(|f| f.frame == *resource) {
                    f.buffer = Some(buffer);
                }
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                // We ignore damage tracking for now and just copy everything.
                let mut frames = screencopy_state.pending_frames.lock().unwrap();
                if let Some(f) = frames.iter_mut().find(|f| f.frame == *resource) {
                    f.buffer = Some(buffer);
                }
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                // Client destroyed the frame, remove it from our queue.
                let mut frames = screencopy_state.pending_frames.lock().unwrap();
                frames.retain(|f| f.frame != *resource);
            }
            _ => {}
        }
    }
}

// Helper macros to make delegation easier in state.rs
#[macro_export]
macro_rules! delegate_screencopy {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1: ()
        ] => $crate::screencopy::ScreencopyState);
        
        wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
    };
}
