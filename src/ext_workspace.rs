//! ext-workspace-v1: список рабочих столов для панелей (waybar,
//! модуль `ext/workspaces`) — какой стол активен, переключение кликом.
//!
//! Одна группа столов на выход; столы фиксированы (`[workspaces] count`),
//! поэтому создание/удаление/перенос столов не поддерживаются — только
//! `activate`.

use smithay::output::Output;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
    protocol::wl_output::WlOutput,
};
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::state::AppState;

const VERSION: u32 = 1;

/// Привязка клиента к менеджеру: его группа и handle-ы столов.
struct Instance {
    manager: ExtWorkspaceManagerV1,
    group: ExtWorkspaceGroupHandleV1,
    workspaces: Vec<ExtWorkspaceHandleV1>,
    /// wl_output клиента, о которых группа уже сообщила (`output_enter`).
    outputs: Vec<WlOutput>,
}

pub struct ExtWorkspaceState {
    _global: GlobalId,
    instances: Vec<Instance>,
    /// `activate`, ждущий `commit`.
    pending_activate: Option<usize>,
}

impl ExtWorkspaceState {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            _global: display.create_global::<AppState, ExtWorkspaceManagerV1, _>(VERSION, ()),
            instances: Vec::new(),
            pending_activate: None,
        }
    }
}

fn state_flags(active: bool) -> ext_workspace_handle_v1::State {
    if active {
        ext_workspace_handle_v1::State::Active
    } else {
        ext_workspace_handle_v1::State::empty()
    }
}

impl AppState {
    /// Активный стол сменился — сообщаем панелям.
    pub fn ext_workspace_changed(&mut self) {
        let active = self.active_workspace;
        let state = &mut self.ext_workspace_state;
        state.instances.retain(|i| i.manager.is_alive());
        for inst in &state.instances {
            for (idx, ws) in inst.workspaces.iter().enumerate() {
                if ws.is_alive() {
                    ws.state(state_flags(idx == active));
                }
            }
            inst.manager.done();
        }
    }

    /// Клиент привязал wl_output: группа столов «входит» на этот выход.
    pub fn ext_workspace_output_bound(&mut self, wl_output: &WlOutput) {
        let Some(client) = wl_output.client() else {
            return;
        };
        for inst in &mut self.ext_workspace_state.instances {
            if inst.manager.client().map(|c| c.id()) != Some(client.id())
                || inst.outputs.contains(wl_output)
            {
                continue;
            }
            inst.group.output_enter(wl_output);
            inst.outputs.push(wl_output.clone());
            inst.manager.done();
        }
    }

    fn ext_workspace_bind(
        &mut self,
        dh: &DisplayHandle,
        client: &Client,
        manager: ExtWorkspaceManagerV1,
    ) {
        let version = manager.version();
        let Ok(group) =
            client.create_resource::<ExtWorkspaceGroupHandleV1, _, AppState>(dh, version, ())
        else {
            return;
        };
        manager.workspace_group(&group);
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
        let mut outputs = Vec::new();
        let space_outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &space_outputs {
            for wl_output in output.client_outputs(client) {
                group.output_enter(&wl_output);
                outputs.push(wl_output);
            }
        }
        let mut workspaces = Vec::new();
        for idx in 0..self.workspaces.len() {
            let Ok(ws) =
                client.create_resource::<ExtWorkspaceHandleV1, _, AppState>(dh, version, idx)
            else {
                continue;
            };
            manager.workspace(&ws);
            ws.id(format!("sandboxwm-{}", idx + 1));
            ws.name(format!("{}", idx + 1));
            ws.coordinates((idx as u32).to_ne_bytes().to_vec());
            ws.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
            ws.state(state_flags(idx == self.active_workspace));
            group.workspace_enter(&ws);
            workspaces.push(ws);
        }
        manager.done();
        self.ext_workspace_state.instances.push(Instance {
            manager,
            group,
            workspaces,
            outputs,
        });
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for AppState {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.ext_workspace_bind(dh, client, manager);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                if let Some(idx) = state.ext_workspace_state.pending_activate.take() {
                    state.switch_workspace(idx);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                state
                    .ext_workspace_state
                    .instances
                    .retain(|i| &i.manager != resource);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        state
            .ext_workspace_state
            .instances
            .retain(|i| &i.manager != resource);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for AppState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // create_workspace не объявлен в capabilities; destroy — без действий.
    }
}

impl Dispatch<ExtWorkspaceHandleV1, usize> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &usize,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_workspace_handle_v1::Request::Activate = request {
            state.ext_workspace_state.pending_activate = Some(*data);
        }
    }
}
