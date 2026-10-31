//! wl_output / xdg-output, wlr-output-management и wlr-screencopy.

use smithay::wayland::output::OutputHandler;

use crate::state::AppState;

impl OutputHandler for AppState {
    fn output_bound(
        &mut self,
        _output: smithay::output::Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        self.ext_workspace_output_bound(&wl_output);
    }
}
smithay::delegate_output!(AppState);

impl AsMut<crate::screencopy::ScreencopyState> for AppState {
    fn as_mut(&mut self) -> &mut crate::screencopy::ScreencopyState {
        &mut self.screencopy_state
    }
}

impl AsMut<crate::output_manager::OutputManagerState> for AppState {
    fn as_mut(&mut self) -> &mut crate::output_manager::OutputManagerState {
        &mut self.wlr_output_manager_state
    }
}

impl crate::output_manager::OutputManagerHandler for AppState {
    fn outputs(
        &self,
    ) -> Vec<(
        smithay::output::Output,
        Option<smithay::utils::Point<i32, smithay::utils::Logical>>,
    )> {
        self.space
            .outputs()
            .map(|o| {
                (
                    o.clone(),
                    Some(
                        self.space
                            .output_geometry(o)
                            .unwrap_or(smithay::utils::Rectangle::from_size((0, 0).into()))
                            .loc,
                    ),
                )
            })
            .collect()
    }
}

crate::delegate_screencopy!(AppState);
crate::delegate_output_manager!(AppState);
